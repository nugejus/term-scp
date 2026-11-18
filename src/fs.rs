use russh_sftp::client::{Config, SftpSession, error::Error};
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::fs;
use tokio::io::AsyncBufReadExt;

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Option<i64>,
    /// Full st_mode including type bits.
    pub mode: u32,
}

impl Entry {
    pub fn is_regular(&self) -> bool {
        self.mode & 0o170000 == 0o100000
    }
}

pub fn sort_entries(v: &mut [Entry]) {
    v.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

pub fn join(dir: &str, name: &str) -> String {
    format!("{}/{}", dir.trim_end_matches('/'), name)
}

pub fn parent(path: &str) -> Option<(String, String)> {
    let p = path.trim_end_matches('/');
    let i = p.rfind('/')?;
    let dir = if i == 0 { "/" } else { &p[..i] };
    Some((dir.to_string(), p[i + 1..].to_string()))
}

pub fn normalize(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

pub fn resolve_path(input: &str, cwd: &str, home: &str) -> String {
    if input == "~" {
        normalize(home)
    } else if let Some(rest) = input.strip_prefix("~/") {
        normalize(&join(home, rest))
    } else if input.starts_with('/') {
        normalize(input)
    } else {
        normalize(&join(cwd, input))
    }
}

pub type Reader = Box<dyn tokio::io::AsyncRead + Send + Unpin>;
pub type Writer = Box<dyn tokio::io::AsyncWrite + Send + Unpin>;

pub struct SshOpts {
    pub port: Option<u16>,
    pub identity: Option<String>,
    pub options: Vec<String>,
}

pub struct Remote {
    pub label: String,
    pub home: String,
    pub sftp: SftpSession,
    channel_argv: Vec<String>,
    child: Mutex<tokio::process::Child>,
    stderr: Arc<Mutex<String>>,
    stderr_seen: AtomicUsize,
    control_dir: Option<std::path::PathBuf>,
}

pub fn sftp_err(e: Error) -> String {
    match e {
        Error::Status(s) => s.error_message,
        e => e.to_string(),
    }
}

pub fn ssh_argv(host: &str, opts: &SshOpts, control: &str, master: bool) -> Vec<String> {
    let (cm, last) = if master {
        ("yes", "ControlPersist=no")
    } else {
        ("no", "BatchMode=yes")
    };
    let mut v = vec!["ssh".to_string()];
    for o in [
        &format!("ControlMaster={cm}"),
        &format!("ControlPath={control}"),
        last,
        "ForwardX11=no",
        "ForwardAgent=no",
        "PermitLocalCommand=no",
        "ClearAllForwardings=yes",
        "RemoteCommand=none",
        "RequestTTY=no",
    ] {
        v.extend(["-o".into(), o.into()]);
    }
    if let Some(p) = opts.port {
        v.extend(["-p".into(), p.to_string()]);
    }
    if let Some(i) = &opts.identity {
        v.extend(["-i".into(), i.clone()]);
    }
    for o in &opts.options {
        v.extend(["-o".into(), o.clone()]);
    }
    v.extend(["-s".into(), host.into(), "sftp".into()]);
    v
}

fn last_line(s: &str) -> Option<String> {
    s.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(String::from)
}

impl Remote {
    pub async fn connect(
        label: &str,
        argv: &[String],
        channel_argv: Vec<String>,
    ) -> Result<Remote, String> {
        let (prog, args) = argv.split_first().ok_or("empty command")?;
        let mut child = tokio::process::Command::new(prog)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("{prog}: {e}"))?;
        let stderr = Arc::new(Mutex::new(String::new()));
        let mut pipe = tokio::io::BufReader::new(child.stderr.take().unwrap());
        let buf = stderr.clone();
        let task = tokio::spawn(async move {
            let mut line = Vec::new();
            while pipe.read_until(b'\n', &mut line).await.is_ok_and(|n| n > 0) {
                buf.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&line));
                line.clear();
            }
        });
        let io = tokio::io::join(child.stdout.take().unwrap(), child.stdin.take().unwrap());
        let cfg = Config {
            request_timeout_secs: 600,
            ..Default::default()
        };
        let sftp = match SftpSession::new_with_config(io, cfg).await {
            Ok(s) => s,
            Err(e) => {
                // The task ends at stderr EOF, so every line is stored.
                let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
                let msg = stderr.lock().unwrap().trim().to_string();
                return Err(if msg.is_empty() { sftp_err(e) } else { msg });
            }
        };
        sftp.set_timeout(30);
        let home = sftp.canonicalize(".").await.map_err(sftp_err)?;
        let seen = stderr.lock().unwrap().len();
        Ok(Remote {
            label: label.into(),
            home,
            sftp,
            channel_argv,
            child: Mutex::new(child),
            stderr,
            stderr_seen: AtomicUsize::new(seen),
            control_dir: None,
        })
    }

    pub async fn connect_ssh(host: &str, opts: &SshOpts) -> Result<Remote, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        // /tmp, not $TMPDIR: keeps the socket path under 104 bytes.
        let dir = format!("/tmp/tscp-{}-{nanos}", std::process::id());
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| err(&dir, e))?;
        let control = format!("{dir}/cm");
        let master = ssh_argv(host, opts, &control, true);
        match Remote::connect(host, &master, ssh_argv(host, opts, &control, false)).await {
            Ok(mut r) => {
                r.control_dir = Some(dir.into());
                Ok(r)
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                Err(e)
            }
        }
    }

    pub async fn open_channel(&self) -> Result<Remote, String> {
        let mut c =
            Remote::connect(&self.label, &self.channel_argv, self.channel_argv.clone()).await?;
        c.home = self.home.clone();
        Ok(c)
    }

    pub fn alive(&self) -> bool {
        matches!(self.child.lock().unwrap().try_wait(), Ok(None))
    }

    pub fn stderr_tail(&self) -> String {
        last_line(&self.stderr.lock().unwrap()).unwrap_or_default()
    }

    pub fn take_new_stderr(&self) -> Option<String> {
        let buf = self.stderr.lock().unwrap();
        let seen = self.stderr_seen.swap(buf.len(), Ordering::Relaxed);
        last_line(&buf[seen..])
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        if let Some(d) = &self.control_dir {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

#[cfg(test)]
pub fn sftp_server_path() -> String {
    [
        "/usr/libexec/sftp-server",
        "/usr/lib/openssh/sftp-server",
        "/usr/libexec/openssh/sftp-server",
        "/usr/lib/ssh/sftp-server",
    ]
    .into_iter()
    .find(|p| std::path::Path::new(p).exists())
    .expect("no sftp-server found")
    .into()
}

#[cfg(test)]
pub async fn test_remote() -> Fs {
    let p = sftp_server_path();
    Fs::Remote(Arc::new(
        Remote::connect("test", std::slice::from_ref(&p), vec![p.clone()])
            .await
            .unwrap(),
    ))
}

#[derive(Clone)]
pub enum Fs {
    Local,
    Remote(Arc<Remote>),
}

fn err(path: &str, e: std::io::Error) -> String {
    format!("{path}: {e}")
}

fn rerr(path: &str, e: Error) -> String {
    format!("{path}: {}", sftp_err(e))
}

fn entry(name: String, mode: u32, size: u64, mtime: Option<i64>) -> Entry {
    Entry {
        name,
        is_dir: mode & 0o170000 == 0o040000,
        size,
        mtime,
        mode,
    }
}

fn local_entry(name: String, m: &std::fs::Metadata) -> Entry {
    entry(name, m.permissions().mode(), m.len(), Some(m.mtime()))
}

fn remote_entry(name: String, a: &FileAttributes) -> Entry {
    let mode = a.permissions.unwrap_or(0);
    entry(name, mode, a.size.unwrap_or(0), a.mtime.map(i64::from))
}

/// Follows symlinks; a broken link falls back to the link itself.
async fn local_meta(path: &str) -> std::io::Result<std::fs::Metadata> {
    match fs::metadata(path).await {
        Err(_) => fs::symlink_metadata(path).await,
        ok => ok,
    }
}

/// Follows a symlink; a broken link keeps its own (lstat) attrs.
async fn follow(sftp: &SftpSession, path: &str, a: FileAttributes) -> FileAttributes {
    if a.permissions.unwrap_or(0) & 0o170000 != 0o120000 {
        return a;
    }
    sftp.metadata(path).await.unwrap_or(a)
}

impl Fs {
    pub fn is_local(&self) -> bool {
        matches!(self, Fs::Local)
    }

    pub fn label(&self) -> String {
        match self {
            Fs::Local => "local".into(),
            Fs::Remote(r) => r.label.clone(),
        }
    }

    pub fn home(&self) -> String {
        match self {
            Fs::Local => std::env::var("HOME").unwrap_or_else(|_| "/".into()),
            Fs::Remote(r) => r.home.clone(),
        }
    }

    pub fn alive(&self) -> bool {
        match self {
            Fs::Local => true,
            Fs::Remote(r) => r.alive(),
        }
    }

    pub async fn list(&self, path: &str) -> Result<(String, Vec<Entry>), String> {
        match self {
            Fs::Local => {
                let canon = fs::canonicalize(path).await.map_err(|e| err(path, e))?;
                let canon = canon.to_string_lossy().into_owned(); // ponytail: lossy non-UTF-8 local names
                let mut rd = fs::read_dir(&canon).await.map_err(|e| err(&canon, e))?;
                let mut v = Vec::new();
                while let Some(de) = rd.next_entry().await.map_err(|e| err(&canon, e))? {
                    let name = de.file_name().to_string_lossy().into_owned(); // ponytail: lossy non-UTF-8 local names
                    let full = join(&canon, &name);
                    match local_meta(&full).await {
                        Ok(m) => v.push(local_entry(name, &m)),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(_) => v.push(Entry {
                            name,
                            is_dir: false,
                            size: 0,
                            mtime: None,
                            mode: 0,
                        }),
                    }
                }
                Ok((canon, v))
            }
            Fs::Remote(r) => {
                let canon = r.sftp.canonicalize(path).await.map_err(|e| rerr(path, e))?;
                let rd = r.sftp.read_dir(&canon).await.map_err(|e| rerr(&canon, e))?;
                let mut v = Vec::new();
                for de in rd {
                    let name = de.file_name();
                    let a = follow(&r.sftp, &join(&canon, &name), de.metadata()).await;
                    v.push(remote_entry(name, &a));
                }
                Ok((canon, v))
            }
        }
    }

    pub async fn stat(&self, path: &str) -> Result<Option<Entry>, String> {
        match self {
            Fs::Local => match local_meta(path).await {
                Ok(m) => Ok(Some(local_entry(
                    parent(path).map_or(path.to_string(), |p| p.1),
                    &m,
                ))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(err(path, e)),
            },
            Fs::Remote(r) => match r.sftp.symlink_metadata(path).await {
                Ok(a) => Ok(Some(remote_entry(
                    parent(path).map_or(path.to_string(), |p| p.1),
                    &follow(&r.sftp, path, a).await,
                ))),
                Err(Error::Status(s)) if s.status_code == StatusCode::NoSuchFile => Ok(None),
                Err(e) => Err(rerr(path, e)),
            },
        }
    }

    pub async fn mkdir(&self, path: &str) -> Result<(), String> {
        match self {
            Fs::Local => fs::create_dir(path).await.map_err(|e| err(path, e)),
            Fs::Remote(r) => r.sftp.create_dir(path).await.map_err(|e| rerr(path, e)),
        }
    }

    pub async fn create_new(&self, path: &str) -> Result<(), String> {
        match self {
            Fs::Local => fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .await
                .map(drop)
                .map_err(|e| err(path, e)),
            Fs::Remote(r) => {
                let flags = OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE;
                let f = r.sftp.open_with_flags(path, flags).await;
                f.map_err(|e| rerr(path, e))?
                    .close()
                    .await
                    .map_err(|e| err(path, e))
            }
        }
    }

    pub async fn open_read(&self, path: &str) -> Result<Reader, String> {
        match self {
            Fs::Local => Ok(Box::new(
                fs::File::open(path).await.map_err(|e| err(path, e))?,
            )),
            Fs::Remote(r) => Ok(Box::new(
                r.sftp.open(path).await.map_err(|e| rerr(path, e))?,
            )),
        }
    }

    pub async fn create_write(&self, path: &str) -> Result<Writer, String> {
        match self {
            Fs::Local => Ok(Box::new(
                fs::File::create(path).await.map_err(|e| err(path, e))?,
            )),
            Fs::Remote(r) => {
                let flags = OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE;
                let f = r.sftp.open_with_flags(path, flags).await;
                Ok(Box::new(f.map_err(|e| rerr(path, e))?))
            }
        }
    }

    pub async fn set_mode(&self, path: &str, mode: u32) -> Result<(), String> {
        match self {
            Fs::Local => fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777))
                .await
                .map_err(|e| err(path, e)),
            Fs::Remote(r) => {
                let attrs = FileAttributes {
                    permissions: Some(mode & 0o7777),
                    ..FileAttributes::empty()
                };
                r.sftp
                    .set_metadata(path, attrs)
                    .await
                    .map_err(|e| rerr(path, e))
            }
        }
    }

    pub async fn remove_file(&self, path: &str) -> Result<(), String> {
        match self {
            Fs::Local => fs::remove_file(path).await.map_err(|e| err(path, e)),
            Fs::Remote(r) => r.sftp.remove_file(path).await.map_err(|e| rerr(path, e)),
        }
    }

    pub async fn remove_dir(&self, path: &str) -> Result<(), String> {
        match self {
            Fs::Local => fs::remove_dir(path).await.map_err(|e| err(path, e)),
            Fs::Remote(r) => r.sftp.remove_dir(path).await.map_err(|e| rerr(path, e)),
        }
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        match self {
            Fs::Local => fs::rename(from, to).await.map_err(|e| err(from, e)),
            Fs::Remote(r) => r.sftp.rename(from, to).await.map_err(|e| rerr(from, e)),
        }
    }

    pub async fn replace(&self, part: &str, dest: &str) -> Result<(), String> {
        match self {
            Fs::Local => self.rename(part, dest).await,
            // SFTP rename fails onto an existing file.
            Fs::Remote(_) => {
                if self.stat(dest).await?.is_some() {
                    self.remove_file(dest).await?;
                }
                self.rename(part, dest).await
            }
        }
    }

    pub async fn channel(&self) -> Fs {
        match self {
            Fs::Local => Fs::Local,
            Fs::Remote(r) => match r.open_channel().await {
                Ok(c) => {
                    c.sftp.set_timeout(120);
                    Fs::Remote(Arc::new(c))
                }
                Err(_) => self.clone(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn e(name: &str, is_dir: bool) -> Entry {
        Entry {
            name: name.into(),
            is_dir,
            size: 0,
            mtime: None,
            mode: 0,
        }
    }

    #[test]
    fn sorts_dirs_first_then_case_insensitive() {
        let mut v = vec![
            e("b.txt", false),
            e("z", true),
            e("a.txt", false),
            e("A", true),
        ];
        sort_entries(&mut v);
        let n: Vec<_> = v.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(n, ["A", "z", "a.txt", "b.txt"]);
    }

    #[test]
    fn join_and_parent() {
        assert_eq!(join("/", "a"), "/a");
        assert_eq!(join("/x", "a"), "/x/a");
        assert_eq!(parent("/x/y"), Some(("/x".into(), "y".into())));
        assert_eq!(parent("/x"), Some(("/".into(), "x".into())));
        assert_eq!(parent("/"), None);
    }

    #[test]
    fn normalize_is_lexical() {
        assert_eq!(normalize("/a/./b//../c"), "/a/c");
        assert_eq!(normalize("/.."), "/");
    }

    #[test]
    fn resolve_handles_tilde_abs_rel() {
        assert_eq!(resolve_path("~", "/c", "/h"), "/h");
        assert_eq!(resolve_path("~/x", "/c", "/h"), "/h/x");
        assert_eq!(resolve_path("/abs/../y", "/c", "/h"), "/y");
        assert_eq!(resolve_path("sub", "/c", "/h"), "/c/sub");
    }

    #[test]
    fn is_regular_uses_type_bits() {
        let m = |mode| Entry {
            mode,
            ..e("x", false)
        };
        assert!(m(0o100644).is_regular());
        assert!(!m(0o040755).is_regular());
        assert!(!m(0o010644).is_regular());
        assert!(!m(0o140755).is_regular());
    }

    fn tmp() -> (tempfile::TempDir, String) {
        let t = tempfile::tempdir().unwrap();
        let r = std::fs::canonicalize(t.path())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        (t, r)
    }

    async fn guarded<F: std::future::Future<Output = ()>>(f: F) {
        tokio::time::timeout(Duration::from_secs(30), f)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn local_list_stat_mkdir_create() {
        guarded(async {
            let (_t, root) = tmp();
            let fs = Fs::Local;
            let d = join(&root, "d");
            let f = join(&root, "f");
            fs.mkdir(&d).await.unwrap();
            fs.create_new(&f).await.unwrap();
            assert!(fs.create_new(&f).await.is_err());
            let (p, mut v) = fs.list(&root).await.unwrap();
            assert_eq!(p, root);
            sort_entries(&mut v);
            assert_eq!(v.len(), 2);
            assert_eq!((v[0].name.as_str(), v[0].is_dir), ("d", true));
            assert_eq!((v[1].name.as_str(), v[1].is_dir), ("f", false));
            assert!(v[1].is_regular());
            assert_eq!(fs.stat(&join(&root, "missing")).await.unwrap(), None);
            assert_eq!(fs.stat(&f).await.unwrap().unwrap().name, "f");
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn local_symlink_to_dir_lists_as_dir() {
        guarded(async {
            let (_t, root) = tmp();
            let fs = Fs::Local;
            fs.mkdir(&join(&root, "d")).await.unwrap();
            std::os::unix::fs::symlink(join(&root, "d"), join(&root, "l")).unwrap();
            let (_, v) = fs.list(&root).await.unwrap();
            assert!(v.iter().find(|x| x.name == "l").unwrap().is_dir);
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn local_replace_overwrites() {
        guarded(async {
            let (_t, root) = tmp();
            let fs = Fs::Local;
            let (dest, part) = (join(&root, "old"), join(&root, "new"));
            std::fs::write(&dest, "old").unwrap();
            std::fs::write(&part, "new").unwrap();
            fs.replace(&part, &dest).await.unwrap();
            assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
            assert_eq!(fs.stat(&part).await.unwrap(), None);
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn local_set_mode() {
        guarded(async {
            let (_t, root) = tmp();
            let fs = Fs::Local;
            let f = join(&root, "f");
            fs.create_new(&f).await.unwrap();
            fs.set_mode(&f, 0o750).await.unwrap();
            assert_eq!(fs.stat(&f).await.unwrap().unwrap().mode & 0o7777, 0o750);
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn local_list_survives_unstatable_children() {
        guarded(async {
            use std::os::unix::fs::PermissionsExt;
            let (_t, root) = tmp();
            let d = join(&root, "d");
            std::fs::create_dir(&d).unwrap();
            std::fs::write(join(&d, "f"), "x").unwrap();
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o444)).unwrap();
            let is_root = std::fs::metadata(join(&d, "f")).is_ok();
            let r = Fs::Local.list(&d).await;
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
            if is_root {
                return;
            }
            let (_, v) = r.unwrap();
            assert_eq!(v.iter().find(|x| x.name == "f").unwrap().mode, 0);
        })
        .await
    }

    #[test]
    fn ssh_argv_master_and_channel() {
        const G: [&str; 12] = [
            "-o",
            "ForwardX11=no",
            "-o",
            "ForwardAgent=no",
            "-o",
            "PermitLocalCommand=no",
            "-o",
            "ClearAllForwardings=yes",
            "-o",
            "RemoteCommand=none",
            "-o",
            "RequestTTY=no",
        ];
        let o = SshOpts {
            port: Some(2222),
            identity: Some("k".into()),
            options: vec!["A=b".into()],
        };
        let want: Vec<&str> = [
            "ssh",
            "-o",
            "ControlMaster=yes",
            "-o",
            "ControlPath=/tmp/c",
            "-o",
            "ControlPersist=no",
        ]
        .into_iter()
        .chain(G)
        .chain(["-p", "2222", "-i", "k", "-o", "A=b", "-s", "h", "sftp"])
        .collect();
        assert_eq!(ssh_argv("h", &o, "/tmp/c", true), want);
        let none = SshOpts {
            port: None,
            identity: None,
            options: vec![],
        };
        let want: Vec<&str> = [
            "ssh",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=/tmp/c",
            "-o",
            "BatchMode=yes",
        ]
        .into_iter()
        .chain(G)
        .chain(["-s", "h", "sftp"])
        .collect();
        assert_eq!(ssh_argv("h", &none, "/tmp/c", false), want);
    }

    async fn poll(limit: Duration, mut done: impl FnMut() -> bool) {
        let t = std::time::Instant::now();
        while !done() {
            assert!(t.elapsed() < limit, "condition not met within {limit:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_list_stat_mkdir_create() {
        guarded(async {
            let (_t, root) = tmp();
            let fs = test_remote().await;
            let d = join(&root, "d");
            let f = join(&root, "f");
            fs.mkdir(&d).await.unwrap();
            fs.create_new(&f).await.unwrap();
            assert!(fs.create_new(&f).await.is_err());
            let (p, mut v) = fs.list(&root).await.unwrap();
            assert_eq!(p, root);
            sort_entries(&mut v);
            assert_eq!(v.len(), 2);
            assert_eq!((v[0].name.as_str(), v[0].is_dir), ("d", true));
            assert_eq!((v[1].name.as_str(), v[1].is_dir), ("f", false));
            assert!(v[1].is_regular());
            assert_eq!(fs.stat(&join(&root, "missing")).await.unwrap(), None);
            assert_eq!(fs.stat(&f).await.unwrap().unwrap().name, "f");
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_replace_overwrites_existing() {
        guarded(async {
            let (_t, root) = tmp();
            let fs = test_remote().await;
            let (dest, part) = (join(&root, "old"), join(&root, "new"));
            std::fs::write(&dest, "old").unwrap();
            std::fs::write(&part, "new").unwrap();
            fs.replace(&part, &dest).await.unwrap();
            assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
            assert_eq!(fs.stat(&part).await.unwrap(), None);
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_symlink_to_dir_lists_as_dir() {
        guarded(async {
            let (_t, root) = tmp();
            let fs = test_remote().await;
            fs.mkdir(&join(&root, "d")).await.unwrap();
            std::os::unix::fs::symlink(join(&root, "d"), join(&root, "l")).unwrap();
            let (_, v) = fs.list(&root).await.unwrap();
            assert!(v.iter().find(|x| x.name == "l").unwrap().is_dir);
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_set_mode() {
        guarded(async {
            let (_t, root) = tmp();
            let fs = test_remote().await;
            let f = join(&root, "f");
            fs.create_new(&f).await.unwrap();
            fs.set_mode(&f, 0o750).await.unwrap();
            assert_eq!(fs.stat(&f).await.unwrap().unwrap().mode & 0o7777, 0o750);
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_channel_is_independent() {
        guarded(async {
            let (_t, root) = tmp();
            let fs = test_remote().await;
            fs.create_new(&join(&root, "f")).await.unwrap();
            let ch = fs.channel().await;
            assert!(!Arc::ptr_eq(remote_of(&fs), remote_of(&ch)));
            assert_eq!(ch.list(&root).await.unwrap().1.len(), 1);
            drop(ch);
            assert_eq!(fs.list(&root).await.unwrap().1.len(), 1);
        })
        .await
    }

    fn remote_of(fs: &Fs) -> &Arc<Remote> {
        match fs {
            Fs::Remote(r) => r,
            Fs::Local => panic!("not remote"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_channel_falls_back_when_mux_fails() {
        guarded(async {
            let (_t, root) = tmp();
            let p = sftp_server_path();
            let r = Remote::connect("t", &[p], vec!["false".into()]).await;
            let fs = Fs::Remote(Arc::new(r.unwrap()));
            assert!(fs.channel().await.list(&root).await.is_ok());
            assert!(fs.list(&root).await.is_ok());
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_connect_failure_reports_stderr() {
        guarded(async {
            let argv = ["sh".into(), "-c".into(), "echo boom >&2; exit 1".into()];
            let e = Remote::connect("x", &argv, vec![]).await.err().unwrap();
            assert!(e.contains("boom"), "{e}");
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_dead_child_is_not_alive() {
        guarded(async {
            let p = sftp_server_path();
            let r = Remote::connect("t", std::slice::from_ref(&p), vec![p.clone()])
                .await
                .unwrap();
            assert!(r.alive());
            r.child.lock().unwrap().start_kill().unwrap();
            poll(Duration::from_secs(1), || !r.alive()).await;
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_late_stderr_is_surfaced() {
        guarded(async {
            let cmd = format!("(sleep 0.2; echo late >&2) & exec {}", sftp_server_path());
            let argv = ["sh".into(), "-c".into(), cmd];
            let r = Remote::connect("t", &argv, vec![]).await.unwrap();
            poll(Duration::from_secs(2), || {
                r.take_new_stderr().as_deref() == Some("late")
            })
            .await;
            assert_eq!(r.take_new_stderr(), None);
            assert_eq!(r.stderr_tail(), "late");
        })
        .await
    }
}
