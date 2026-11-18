use std::os::unix::fs::{MetadataExt, PermissionsExt};
use tokio::fs;

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

#[derive(Clone)]
pub enum Fs {
    Local,
}

fn err(path: &str, e: std::io::Error) -> String {
    format!("{path}: {e}")
}

fn entry(name: String, m: &std::fs::Metadata) -> Entry {
    let mode = m.permissions().mode();
    Entry {
        name,
        is_dir: mode & 0o170000 == 0o040000,
        size: m.len(),
        mtime: Some(m.mtime()),
        mode,
    }
}

/// Follows symlinks; a broken link falls back to the link itself.
async fn local_meta(path: &str) -> std::io::Result<std::fs::Metadata> {
    match fs::metadata(path).await {
        Err(_) => fs::symlink_metadata(path).await,
        ok => ok,
    }
}

impl Fs {
    pub fn is_local(&self) -> bool {
        matches!(self, Fs::Local)
    }

    pub fn label(&self) -> String {
        match self {
            Fs::Local => "local".into(),
        }
    }

    pub fn home(&self) -> String {
        match self {
            Fs::Local => std::env::var("HOME").unwrap_or_else(|_| "/".into()),
        }
    }

    pub fn alive(&self) -> bool {
        match self {
            Fs::Local => true,
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
                        Ok(m) => v.push(entry(name, &m)),
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
        }
    }

    pub async fn stat(&self, path: &str) -> Result<Option<Entry>, String> {
        match self {
            Fs::Local => match local_meta(path).await {
                Ok(m) => Ok(Some(entry(
                    parent(path).map_or(path.to_string(), |p| p.1),
                    &m,
                ))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(err(path, e)),
            },
        }
    }

    pub async fn mkdir(&self, path: &str) -> Result<(), String> {
        match self {
            Fs::Local => fs::create_dir(path).await.map_err(|e| err(path, e)),
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
        }
    }

    pub async fn open_read(&self, path: &str) -> Result<Reader, String> {
        match self {
            Fs::Local => Ok(Box::new(
                fs::File::open(path).await.map_err(|e| err(path, e))?,
            )),
        }
    }

    pub async fn create_write(&self, path: &str) -> Result<Writer, String> {
        match self {
            Fs::Local => Ok(Box::new(
                fs::File::create(path).await.map_err(|e| err(path, e))?,
            )),
        }
    }

    pub async fn set_mode(&self, path: &str, mode: u32) -> Result<(), String> {
        match self {
            Fs::Local => fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777))
                .await
                .map_err(|e| err(path, e)),
        }
    }

    pub async fn remove_file(&self, path: &str) -> Result<(), String> {
        match self {
            Fs::Local => fs::remove_file(path).await.map_err(|e| err(path, e)),
        }
    }

    pub async fn remove_dir(&self, path: &str) -> Result<(), String> {
        match self {
            Fs::Local => fs::remove_dir(path).await.map_err(|e| err(path, e)),
        }
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        match self {
            Fs::Local => fs::rename(from, to).await.map_err(|e| err(from, e)),
        }
    }

    pub async fn replace(&self, part: &str, dest: &str) -> Result<(), String> {
        match self {
            Fs::Local => self.rename(part, dest).await,
        }
    }

    pub async fn channel(&self) -> Fs {
        match self {
            Fs::Local => Fs::Local,
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
}
