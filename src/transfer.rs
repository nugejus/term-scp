use crate::fs::{Entry, Fs, join, parent};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Semaphore, oneshot};
use unicode_normalization::UnicodeNormalization;

pub type JobId = u64;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Resolution {
    Overwrite,
    Skip,
    OverwriteAll,
    Cancel,
}

/// `items` are (src_path, exact dst_path).
pub struct Job {
    pub id: JobId,
    pub src: Fs,
    pub dst: Fs,
    pub items: Vec<(String, String)>,
    pub mv: bool,
}

#[derive(Debug)]
pub enum TransferEvent {
    Queued {
        id: JobId,
    },
    Started {
        id: JobId,
    },
    Scanned {
        id: JobId,
        total_bytes: u64,
        total_files: usize,
    },
    Progress {
        id: JobId,
        current: String,
        done_bytes: u64,
        done_files: usize,
    },
    Conflict {
        id: JobId,
        path: String,
        reply: oneshot::Sender<Resolution>,
    },
    ItemError {
        id: JobId,
        path: String,
        error: String,
    },
    Finished {
        id: JobId,
        copied: usize,
        skipped: usize,
        failed: usize,
        cancelled: bool,
        mv: bool,
    },
}

pub type Emit = Arc<dyn Fn(TransferEvent) + Send + Sync>;

pub fn dst_name_for(src: &Fs, dst: &Fs, name: &str) -> String {
    if src.is_local() && !dst.is_local() {
        name.nfc().collect()
    } else {
        name.to_string()
    }
}

pub async fn write_via_part(
    dst: &Fs,
    dest: &str,
    reader: &mut (dyn AsyncRead + Send + Unpin),
    mode: u32,
    on_bytes: &mut (dyn FnMut(u64) + Send),
) -> Result<u64, String> {
    let part = format!("{dest}.term-scp-part");
    let copied = async {
        let mut w = dst.create_write(&part).await?;
        let mut buf = vec![0u8; 256 * 1024];
        let mut total = 0;
        loop {
            let n = reader
                .read(&mut buf)
                .await
                .map_err(|e| format!("read: {e}"))?;
            if n == 0 {
                break;
            }
            w.write_all(&buf[..n])
                .await
                .map_err(|e| format!("{part}: {e}"))?;
            total += n as u64;
            on_bytes(n as u64);
        }
        w.shutdown().await.map_err(|e| format!("{part}: {e}"))?;
        dst.set_mode(&part, mode).await?;
        Ok::<_, String>(total)
    }
    .await;
    let total = match copied {
        Ok(n) => n,
        Err(e) => {
            let _ = dst.remove_file(&part).await;
            return Err(e);
        }
    };
    let Err(e) = dst.replace(&part, dest).await else {
        return Ok(total);
    };
    if let Ok(Some(_)) = dst.stat(dest).await {
        let _ = dst.remove_file(&part).await;
        return Err(e);
    }
    // The original is gone: the part may be the only complete copy.
    Err(format!("{dest}: {e} (new content kept in {part})"))
}

enum Op {
    Dir {
        src: String,
        dst: String,
    },
    File {
        src: String,
        dst: String,
        size: u64,
        mode: u32,
    },
    DirDone {
        src: String,
    },
}

enum Walk {
    Visit(String, String, Entry, usize),
    Done(String),
}

/// `p` is `dir` or lies under it.
fn under(p: &str, dir: &str) -> bool {
    p == dir || p.starts_with(&join(dir, ""))
}

/// Walks one item into `ops`; bad entries go to `bad` as (src, error).
async fn scan(
    src: &Fs,
    dst: &Fs,
    item: &(String, String),
    mv: bool,
    ops: &mut Vec<Op>,
    bad: &mut Vec<(String, String)>,
) {
    let (s, d) = item;
    // Only the item's own name is normalized; the existing parent is used as-is.
    let d = parent(d).map_or(d.clone(), |(dir, n)| {
        join(&dir, &dst_name_for(src, dst, &n))
    });
    let e = match src.stat(s).await {
        Ok(Some(e)) => e,
        Ok(None) => return bad.push((s.clone(), "not found".into())),
        Err(e) => return bad.push((s.clone(), e)),
    };
    let mut stack = vec![Walk::Visit(s.clone(), d, e, 0)];
    while let Some(w) = stack.pop() {
        let (s, d, e, depth) = match w {
            Walk::Done(s) => {
                ops.push(Op::DirDone { src: s });
                continue;
            }
            Walk::Visit(s, d, e, depth) => (s, d, e, depth),
        };
        if e.is_regular() {
            ops.push(Op::File {
                src: s,
                dst: d,
                size: e.size,
                mode: e.mode,
            });
        } else if !e.is_dir {
            bad.push((s, "not a regular file".into()));
        } else if depth > 64 {
            bad.push((s, "too deeply nested (symlink loop?)".into()));
        } else {
            match src.list(&s).await {
                Err(e) => bad.push((s, e)),
                // Children hang off the canonical path, so each lookup crosses at most one symlink.
                Ok((canon, kids)) => {
                    ops.push(Op::Dir {
                        src: canon.clone(),
                        dst: d.clone(),
                    });
                    if mv {
                        stack.push(Walk::Done(canon.clone()));
                    }
                    for k in kids.into_iter().rev() {
                        let kd = join(&d, &dst_name_for(src, dst, &k.name));
                        stack.push(Walk::Visit(join(&canon, &k.name), kd, k, depth + 1));
                    }
                }
            }
        }
    }
}

struct Prog {
    id: JobId,
    emit: Emit,
    current: String,
    bytes: u64,
    files: usize,
    last: Instant,
}

impl Prog {
    fn tick(&mut self, force: bool) {
        if force || self.last.elapsed() >= Duration::from_millis(100) {
            self.last = Instant::now();
            (self.emit)(TransferEvent::Progress {
                id: self.id,
                current: self.current.clone(),
                done_bytes: self.bytes,
                done_files: self.files,
            });
        }
    }
}

pub async fn run_job(job: Job, emit: Emit) {
    let id = job.id;
    // `job` keeps the panes' master `Arc<Remote>`s alive until Finished: dropping the last one kills every channel.
    let src = job.src.channel().await;
    let dst = job.dst.channel().await;
    let (mut ops, mut bad) = (Vec::new(), Vec::new());
    for item in &job.items {
        scan(&src, &dst, item, job.mv, &mut ops, &mut bad).await;
    }
    let item_err = |path: &str, error: String| {
        emit(TransferEvent::ItemError {
            id,
            path: path.into(),
            error,
        })
    };
    let mut failed = bad.len();
    // Source paths that stay (failed or skipped); their dirs are not removed.
    let mut kept = Vec::new();
    for (p, e) in bad {
        item_err(&p, e);
        kept.push(p);
    }
    let (total_bytes, total_files) = ops.iter().fold((0, 0), |(b, n), op| match op {
        Op::File { size, .. } => (b + size, n + 1),
        _ => (b, n),
    });
    emit(TransferEvent::Scanned {
        id,
        total_bytes,
        total_files,
    });

    let mut prog = Prog {
        id,
        emit: emit.clone(),
        current: String::new(),
        bytes: 0,
        files: 0,
        last: Instant::now(),
    };
    let (mut copied, mut skipped, mut cancelled, mut all) = (0, 0, false, false);
    // A dst dir that could not be made: everything under it is skipped.
    let mut broken: Option<String> = None;
    for op in ops {
        match op {
            Op::Dir { src: s, dst: d } => {
                if broken.as_deref().is_some_and(|b| under(&d, b)) {
                    kept.push(s);
                    continue;
                }
                let r = match dst.stat(&d).await {
                    Ok(Some(e)) if e.is_dir => Ok(()),
                    Ok(Some(_)) => Err("destination exists and is not a directory".to_string()),
                    Ok(None) => dst.mkdir(&d).await,
                    Err(e) => Err(e),
                };
                if let Err(e) = r {
                    item_err(&d, e);
                    failed += 1;
                    kept.push(s);
                    broken = Some(d);
                }
            }
            Op::File {
                src: s,
                dst: d,
                size,
                mode,
            } => {
                let base = prog.bytes;
                prog.current = parent(&s).map_or(s.clone(), |p| p.1);
                prog.tick(false);
                let r = async {
                    if broken.as_deref().is_some_and(|b| under(&d, b)) {
                        return Ok(Resolution::Skip);
                    }
                    match dst.stat(&d).await? {
                        Some(e) if e.is_dir => return Err("destination is a directory".to_string()),
                        Some(_) if !all => {
                            let (tx, rx) = oneshot::channel();
                            emit(TransferEvent::Conflict {
                                id,
                                path: d.clone(),
                                reply: tx,
                            });
                            match rx.await.unwrap_or(Resolution::Cancel) {
                                Resolution::OverwriteAll => all = true,
                                Resolution::Overwrite => {}
                                r => return Ok(r),
                            }
                        }
                        _ => {}
                    }
                    let mut on_bytes = |n| {
                        prog.bytes += n;
                        prog.tick(false);
                    };
                    write_via_part(
                        &dst,
                        &d,
                        &mut *src.open_read(&s).await?,
                        mode,
                        &mut on_bytes,
                    )
                    .await?;
                    if job.mv {
                        src.remove_file(&s).await?;
                    }
                    Ok(Resolution::Overwrite)
                }
                .await;
                match r {
                    Ok(Resolution::Cancel) => {
                        cancelled = true;
                        break;
                    }
                    Ok(Resolution::Skip) => {
                        skipped += 1;
                        kept.push(s);
                    }
                    Ok(_) => copied += 1,
                    Err(e) => {
                        item_err(&d, e);
                        failed += 1;
                        kept.push(s);
                    }
                }
                prog.bytes = base + size;
                prog.files += 1;
                prog.tick(false);
            }
            Op::DirDone { src: s } => {
                // ponytail: O(n²) prefix scan, fine for thousands of items
                if kept.iter().any(|k| under(k, &s)) {
                    continue;
                }
                if let Err(e) = src.remove_dir(&s).await {
                    item_err(&s, e);
                    failed += 1;
                    kept.push(s);
                }
            }
        }
    }
    prog.tick(true);
    let mv = job.mv;
    emit(TransferEvent::Finished {
        id,
        copied,
        skipped,
        failed,
        cancelled,
        mv,
    });
    drop(job);
}

pub fn submit(rt: &tokio::runtime::Handle, sem: Arc<Semaphore>, job: Job, emit: Emit) {
    emit(TransferEvent::Queued { id: job.id });
    rt.spawn(async move {
        // Held for the whole job. Err only if the semaphore is closed, which never happens.
        let _permit = sem.acquire().await;
        emit(TransferEvent::Started { id: job.id });
        run_job(job, emit).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::test_remote;
    use Resolution::*;
    use std::collections::BTreeMap;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::task::{Context, Poll};
    use tokio::sync::Notify;

    const MIB: usize = 1024 * 1024;
    const NFD: &str = "\u{1100}\u{1161}.txt";
    type Log = Arc<Mutex<Vec<String>>>;
    type Done = Arc<(AtomicUsize, Notify)>;

    fn collect(answer: Resolution) -> (Emit, Log, Done) {
        let (log, done) = (Log::default(), Done::default());
        let (l, d) = (log.clone(), done.clone());
        let emit: Emit = Arc::new(move |ev| {
            l.lock().unwrap().push(format!("{ev:?}"));
            match ev {
                TransferEvent::Conflict { reply, .. } => {
                    let _ = reply.send(answer);
                }
                TransferEvent::Finished { .. } => {
                    d.0.fetch_add(1, SeqCst);
                    d.1.notify_one();
                }
                _ => {}
            }
        });
        (emit, log, done)
    }

    async fn wait_finished(done: &(AtomicUsize, Notify), n: usize) {
        while done.0.load(SeqCst) < n {
            done.1.notified().await
        }
    }

    async fn run(
        src: Fs,
        dst: Fs,
        items: &[(String, String)],
        mv: bool,
        a: Resolution,
    ) -> Vec<String> {
        let (emit, log, done) = collect(a);
        let items = items.to_vec();
        let job = Job {
            id: 1,
            src,
            dst,
            items,
            mv,
        };
        let rt = tokio::runtime::Handle::current();
        submit(&rt, Arc::new(Semaphore::new(4)), job, emit);
        wait_finished(&done, 1).await;
        log.lock().unwrap().clone()
    }

    fn ev<'a>(log: &'a [String], prefix: &str) -> &'a str {
        log.iter().find(|s| s.starts_with(prefix)).expect(prefix)
    }

    fn count(log: &[String], prefix: &str) -> usize {
        log.iter().filter(|s| s.starts_with(prefix)).count()
    }

    fn tmp() -> (tempfile::TempDir, String) {
        let t = tempfile::tempdir().unwrap();
        let r = std::fs::canonicalize(t.path()).unwrap();
        (t, r.to_string_lossy().into_owned())
    }

    async fn guarded<F: std::future::Future<Output = ()>>(f: F) {
        tokio::time::timeout(Duration::from_secs(30), f)
            .await
            .unwrap();
    }

    fn bytes(n: usize, seed: u32) -> Vec<u8> {
        (0..n as u32)
            .map(|i| ((i ^ seed).wrapping_mul(2654435761) >> 24) as u8)
            .collect()
    }

    /// root/d/{a.txt, sub/b.bin (3 MiB)}; returns root/d.
    fn make_tree(root: &str) -> String {
        let d = join(root, "d");
        std::fs::create_dir_all(join(&d, "sub")).unwrap();
        std::fs::write(join(&d, "a.txt"), "hello").unwrap();
        std::fs::write(join(&d, "sub/b.bin"), bytes(3 * MIB, 1)).unwrap();
        d
    }

    fn tree(dir: &str) -> BTreeMap<String, Vec<u8>> {
        let mut m = BTreeMap::new();
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().into_string().unwrap();
            let p = join(dir, &name);
            if e.file_type().unwrap().is_dir() {
                m.extend(tree(&p).into_iter().map(|(k, v)| (join(&name, &k), v)));
            } else {
                m.insert(name, std::fs::read(&p).unwrap());
            }
        }
        m
    }

    fn assert_same(a: &str, b: &str) {
        let (x, y) = (tree(a), tree(b));
        assert_eq!(x.keys().collect::<Vec<_>>(), y.keys().collect::<Vec<_>>());
        assert!(x == y, "contents differ");
    }

    fn names(dir: &str) -> Vec<String> {
        let rd = std::fs::read_dir(dir).unwrap();
        rd.map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect()
    }

    fn exists(p: &str) -> bool {
        std::fs::symlink_metadata(p).is_ok()
    }

    async fn upload_check(src: Fs, dst: Fs) {
        let (_a, s) = tmp();
        let (_b, d) = tmp();
        let sd = make_tree(&s);
        let log = run(src, dst, &[(sd.clone(), join(&d, "d"))], false, Skip).await;
        assert_same(&sd, &join(&d, "d"));
        assert!(ev(&log, "Finished").contains("copied: 2,"), "{log:?}");
        let want = format!("total_bytes: {}, total_files: 2", 5 + 3 * MIB);
        assert!(ev(&log, "Scanned").contains(&want), "{log:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upload_tree_byte_for_byte() {
        guarded(async { upload_check(Fs::Local, test_remote().await).await }).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn download_tree_byte_for_byte() {
        guarded(async { upload_check(test_remote().await, Fs::Local).await }).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_to_remote_tree_byte_for_byte() {
        guarded(async {
            let (_a, s) = tmp();
            let (_b, d) = tmp();
            let sd = make_tree(&s);
            std::fs::write(join(&sd, NFD), "jamo").unwrap();
            let (src, dst) = (test_remote().await, test_remote().await);
            let log = run(src, dst, &[(sd.clone(), join(&d, "d"))], false, Skip).await;
            assert_same(&sd, &join(&d, "d"));
            assert!(names(&join(&d, "d")).contains(&NFD.to_string()));
            assert!(ev(&log, "Finished").contains("copied: 3,"), "{log:?}");
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn preserves_mode() {
        guarded(async {
            use std::os::unix::fs::PermissionsExt;
            let (_a, s) = tmp();
            let (_b, d) = tmp();
            let (sf, df) = (join(&s, "f"), join(&d, "f"));
            std::fs::write(&sf, "x").unwrap();
            std::fs::set_permissions(&sf, std::fs::Permissions::from_mode(0o750)).unwrap();
            run(
                Fs::Local,
                test_remote().await,
                &[(sf, df.clone())],
                false,
                Skip,
            )
            .await;
            assert_eq!(
                std::fs::metadata(&df).unwrap().permissions().mode() & 0o7777,
                0o750
            );
        })
        .await
    }

    /// Local f = "new" → remote f = "old"; returns (remote content, log).
    async fn conflict(answer: Resolution) -> (String, Vec<String>) {
        let (_a, s) = tmp();
        let (_b, d) = tmp();
        let (sf, df) = (join(&s, "f"), join(&d, "f"));
        std::fs::write(&sf, "new").unwrap();
        std::fs::write(&df, "old").unwrap();
        let log = run(
            Fs::Local,
            test_remote().await,
            &[(sf, df.clone())],
            false,
            answer,
        )
        .await;
        assert!(!exists(&format!("{df}.term-scp-part")));
        (std::fs::read_to_string(&df).unwrap(), log)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn conflict_skip_keeps_original() {
        guarded(async {
            let (c, log) = conflict(Skip).await;
            assert_eq!(c, "old");
            assert!(
                ev(&log, "Finished").contains("copied: 0, skipped: 1,"),
                "{log:?}"
            );
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn conflict_overwrite_replaces() {
        guarded(async {
            let (c, log) = conflict(Overwrite).await;
            assert_eq!(c, "new");
            assert!(ev(&log, "Finished").contains("copied: 1,"), "{log:?}");
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn conflict_overwrite_all_asks_once() {
        guarded(async {
            let (_a, s) = tmp();
            let (_b, d) = tmp();
            for root in [&s, &d] {
                std::fs::create_dir(join(root, "d")).unwrap();
                for f in ["x", "y"] {
                    std::fs::write(join(root, &format!("d/{f}")), root.as_bytes()).unwrap();
                }
            }
            let (sd, dd) = (join(&s, "d"), join(&d, "d"));
            let log = run(
                Fs::Local,
                test_remote().await,
                &[(sd.clone(), dd.clone())],
                false,
                OverwriteAll,
            )
            .await;
            assert_eq!(count(&log, "Conflict"), 1, "{log:?}");
            assert_same(&sd, &dd);
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn conflict_cancel_stops_job() {
        guarded(async {
            let (_a, s) = tmp();
            let (_b, d) = tmp();
            std::fs::write(join(&s, "x"), "new").unwrap();
            std::fs::write(join(&s, "y"), "new").unwrap();
            std::fs::write(join(&d, "x"), "old").unwrap();
            let items = [
                (join(&s, "x"), join(&d, "x")),
                (join(&s, "y"), join(&d, "y")),
            ];
            let log = run(Fs::Local, test_remote().await, &items, false, Cancel).await;
            assert!(ev(&log, "Finished").contains("cancelled: true"), "{log:?}");
            assert_eq!(names(&d), ["x"]);
            assert_eq!(std::fs::read_to_string(join(&d, "x")).unwrap(), "old");
        })
        .await
    }

    struct Boom;
    impl AsyncRead for Boom {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(std::io::Error::other("boom")))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_write_leaves_original_and_no_part() {
        guarded(async {
            let (_b, d) = tmp();
            let dest = join(&d, "f");
            std::fs::write(&dest, "old").unwrap();
            let mut r = (&[7u8; 1000][..]).chain(Boom);
            let res = write_via_part(&test_remote().await, &dest, &mut r, 0o644, &mut |_| {}).await;
            assert!(res.unwrap_err().contains("boom"));
            assert_eq!(std::fs::read_to_string(&dest).unwrap(), "old");
            assert_eq!(names(&d), ["f"]);
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_replace_keeps_dest_and_removes_part() {
        guarded(async {
            let (_b, d) = tmp();
            let dest = join(&d, "f");
            std::fs::create_dir(&dest).unwrap();
            std::fs::write(join(&dest, "keep"), "x").unwrap();
            let mut r = &b"new"[..];
            let res = write_via_part(&test_remote().await, &dest, &mut r, 0o644, &mut |_| {}).await;
            assert!(res.is_err());
            assert_eq!(names(&d), ["f"]);
            assert_eq!(names(&dest), ["keep"]);
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn move_removes_sources_only_on_success() {
        guarded(async {
            let (_a, s) = tmp();
            let (_b, d) = tmp();
            let sd = join(&s, "d");
            std::fs::create_dir_all(join(&sd, "e")).unwrap();
            for f in ["x", "y", "e/z"] {
                std::fs::write(join(&sd, f), f).unwrap();
            }
            std::fs::create_dir(join(&d, "d")).unwrap();
            std::fs::write(join(&d, "d/x"), "old").unwrap();
            let log = run(
                Fs::Local,
                test_remote().await,
                &[(sd.clone(), join(&d, "d"))],
                true,
                Skip,
            )
            .await;
            assert!(
                ev(&log, "Finished").contains("copied: 2, skipped: 1, failed: 0"),
                "{log:?}"
            );
            assert_eq!(names(&sd), ["x"]);
            assert_eq!(std::fs::read_to_string(join(&d, "d/x")).unwrap(), "old");
            assert_eq!(std::fs::read_to_string(join(&d, "d/y")).unwrap(), "y");
            assert_eq!(std::fs::read_to_string(join(&d, "d/e/z")).unwrap(), "e/z");
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfc_on_upload() {
        guarded(async {
            let (_a, s) = tmp();
            let (_b, d) = tmp();
            std::fs::write(join(&s, NFD), "x").unwrap();
            run(
                Fs::Local,
                test_remote().await,
                &[(join(&s, NFD), join(&d, NFD))],
                false,
                Skip,
            )
            .await;
            assert_eq!(names(&d), ["\u{AC00}.txt"]);
            let (_c, l) = tmp();
            run(
                test_remote().await,
                Fs::Local,
                &[(join(&s, NFD), join(&l, NFD))],
                false,
                Skip,
            )
            .await;
            assert_eq!(names(&l), [NFD]);
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfc_upload_into_existing_nfd_dir() {
        guarded(async {
            let (_a, s) = tmp();
            let (_b, d) = tmp();
            let dst = test_remote().await;
            let dir = join(&d, "\u{1100}\u{1161}");
            dst.mkdir(&dir).await.unwrap();
            std::fs::write(join(&s, "x.txt"), "x").unwrap();
            let target = join(&dir, "x.txt");
            run(
                Fs::Local,
                dst.clone(),
                &[(join(&s, "x.txt"), target.clone())],
                false,
                Skip,
            )
            .await;
            assert!(dst.stat(&target).await.unwrap().is_some());
            assert_eq!(names(&d), ["\u{1100}\u{1161}"]);
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn symlink_loop_is_bounded() {
        guarded(async {
            let (_a, s) = tmp();
            let (_b, d) = tmp();
            let sd = join(&s, "d");
            std::fs::create_dir(&sd).unwrap();
            std::os::unix::fs::symlink(".", join(&sd, "loop")).unwrap();
            let log = run(
                Fs::Local,
                test_remote().await,
                &[(sd, join(&d, "d"))],
                false,
                Skip,
            )
            .await;
            assert!(
                log.iter()
                    .any(|e| e.starts_with("ItemError") && e.contains("too deeply nested"))
            );
            assert_eq!(count(&log, "Finished"), 1);
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fifo_is_skipped_not_hung() {
        guarded(async {
            let (_a, s) = tmp();
            let (_b, d) = tmp();
            let sd = join(&s, "d");
            std::fs::create_dir(&sd).unwrap();
            std::fs::write(join(&sd, "a.txt"), "a").unwrap();
            let p = join(&sd, "p");
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(&p)
                    .status()
                    .unwrap()
                    .success()
            );
            let log = run(
                Fs::Local,
                test_remote().await,
                &[(sd, join(&d, "d"))],
                false,
                Skip,
            )
            .await;
            let want = format!("path: {p:?}, error: \"not a regular file\"");
            assert!(
                log.iter()
                    .any(|e| e.starts_with("ItemError") && e.contains(&want)),
                "{log:?}"
            );
            assert!(ev(&log, "Finished").contains("copied: 1,"), "{log:?}");
        })
        .await
    }

    #[derive(Default)]
    struct Four {
        held: Vec<oneshot::Sender<Resolution>>,
        answer: bool,
        conflicts: usize,
        started: usize,
        running: usize,
        max: usize,
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exactly_four_run_and_all_complete() {
        guarded(async {
            let (_a, s) = tmp();
            let (_b, d) = tmp();
            let f = |root: &str, i: usize| join(root, &format!("f{i}"));
            for i in 0..6 {
                std::fs::write(f(&s, i), bytes(2 * MIB, i as u32)).unwrap();
                std::fs::write(f(&d, i), "old").unwrap();
            }
            let (st, done) = (Arc::new(Mutex::new(Four::default())), Done::default());
            let (st2, done2) = (st.clone(), done.clone());
            let emit: Emit = Arc::new(move |ev| {
                let mut st = st2.lock().unwrap();
                match ev {
                    TransferEvent::Started { .. } => {
                        st.started += 1;
                        st.running += 1;
                        st.max = st.max.max(st.running);
                    }
                    TransferEvent::Conflict { reply, .. } => {
                        st.conflicts += 1;
                        match st.answer {
                            true => {
                                let _ = reply.send(Overwrite);
                            }
                            false => st.held.push(reply),
                        }
                    }
                    TransferEvent::Finished { .. } => {
                        st.running -= 1;
                        done2.0.fetch_add(1, SeqCst);
                        done2.1.notify_one();
                    }
                    _ => {}
                }
            });
            let (rt, sem, dst) = (
                tokio::runtime::Handle::current(),
                Arc::new(Semaphore::new(4)),
                test_remote().await,
            );
            for i in 0..6 {
                let items = vec![(f(&s, i), f(&d, i))];
                let job = Job {
                    id: i as u64,
                    src: Fs::Local,
                    dst: dst.clone(),
                    items,
                    mv: false,
                };
                submit(&rt, sem.clone(), job, emit.clone());
            }
            while st.lock().unwrap().conflicts < 4 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            {
                let mut st = st.lock().unwrap();
                assert_eq!((st.started, st.conflicts), (4, 4));
                st.answer = true;
                for tx in st.held.drain(..) {
                    let _ = tx.send(Overwrite);
                }
            }
            wait_finished(&done, 6).await;
            assert_eq!(st.lock().unwrap().max, 4);
            for i in 0..6 {
                assert!(
                    std::fs::read(f(&d, i)).unwrap() == bytes(2 * MIB, i as u32),
                    "f{i} differs"
                );
            }
        })
        .await
    }
}
