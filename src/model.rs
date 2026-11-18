use crate::fs::{Entry, Fs, sort_entries};
use crate::transfer::{JobId, Resolution};
use ratatui::layout::Rect;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Left,
    Right,
}
impl Side {
    pub fn other(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        }
    }
    pub fn idx(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    CursorUp,
    CursorDown,
    ExtendUp,
    ExtendDown,
    PageUp,
    PageDown,
    Home,
    End,
    Parent,
    Enter,
    SwitchPane,
    ClearMarks,
    QuickCopy,
    CopyTo,
    Move,
    NewDir,
    NewFile,
    ChangeDir,
    Help,
    Refresh,
    Quit,
}

pub const TOOLBAR: [(&str, Command); 6] = [
    ("New Dir", Command::NewDir),
    ("New File", Command::NewFile),
    ("Help", Command::Help),
    ("Move", Command::Move),
    ("Copy to", Command::CopyTo),
    ("Change Dir", Command::ChangeDir),
];

pub struct Pane {
    pub fs: Fs,
    pub cwd: String,
    pub entries: Vec<Entry>, // entries[0] is ".." unless cwd == "/"
    pub cursor: usize,
    pub offset: usize,
    pub marked: BTreeSet<String>,
    anchor: Option<(usize, BTreeSet<String>)>, // (anchor index, marks before the range began)
    pub loading: Option<(String, Option<String>)>, // in-flight (path, select)
    pub seq: u64,
}

impl Pane {
    pub fn new(fs: Fs, cwd: String) -> Pane {
        Pane {
            fs,
            cwd,
            entries: vec![],
            cursor: 0,
            offset: 0,
            marked: BTreeSet::new(),
            anchor: None,
            loading: None,
            seq: 0,
        }
    }

    pub fn set_listing(&mut self, cwd: String, mut entries: Vec<Entry>, select: Option<&str>) {
        sort_entries(&mut entries);
        if cwd != "/" {
            entries.insert(
                0,
                Entry {
                    name: "..".into(),
                    is_dir: true,
                    size: 0,
                    mtime: None,
                    mode: 0o040755,
                },
            );
        }
        let same = cwd == self.cwd;
        self.entries = entries;
        if same {
            let names = &self.entries;
            self.marked.retain(|m| names.iter().any(|e| &e.name == m));
        } else {
            self.marked.clear();
            self.offset = 0;
        }
        let last = self.entries.len().saturating_sub(1);
        self.cursor = match select.and_then(|s| self.entries.iter().position(|e| e.name == s)) {
            Some(i) => i,
            None if same => self.cursor.min(last),
            None => 0,
        };
        self.cwd = cwd;
        self.anchor = None;
        self.loading = None;
    }

    pub fn move_cursor(&mut self, delta: isize, extend: bool) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len().saturating_sub(1) as isize;
        let new = (self.cursor as isize + delta).clamp(0, last) as usize;
        if !extend {
            self.anchor = None;
            self.cursor = new;
            return;
        }
        let (a, base) = self
            .anchor
            .get_or_insert_with(|| (self.cursor, self.marked.clone()));
        let mut marks = base.clone();
        let (lo, hi) = ((*a).min(new), (*a).max(new));
        self.cursor = new;
        marks.extend(
            self.entries[lo..=hi]
                .iter()
                .filter(|e| e.name != "..")
                .map(|e| e.name.clone()),
        );
        self.marked = marks;
    }

    pub fn cursor_to(&mut self, idx: usize) {
        self.anchor = None;
        self.cursor = idx.min(self.entries.len().saturating_sub(1));
    }

    pub fn clear_marks(&mut self) {
        self.marked.clear();
        self.anchor = None;
    }

    pub fn current(&self) -> Option<&Entry> {
        self.entries.get(self.cursor)
    }

    pub fn selection(&self) -> Vec<Entry> {
        let v: Vec<Entry> = self
            .entries
            .iter()
            .filter(|e| self.marked.contains(&e.name))
            .cloned()
            .collect();
        if !v.is_empty() {
            return v;
        }
        self.current()
            .filter(|e| e.name != "..")
            .cloned()
            .into_iter()
            .collect()
    }
}

/// Each kind captures, at open time, the dir it acts on.
pub enum InputKind {
    NewDir {
        dir: String,
    },
    NewFile {
        dir: String,
    },
    ChangeDir {
        base: String,
    },
    CopyTo {
        from: Side,
        items: Vec<Entry>,
        mv: bool,
        src_cwd: String,
    },
}
pub enum Popup {
    Input {
        kind: InputKind,
        title: String,
        value: String,
        cursor: usize, // char index
    },
    Help,
    ConfirmQuit,
}
pub struct ConflictPrompt {
    pub job: JobId,
    pub path: String,
    pub reply: tokio::sync::oneshot::Sender<Resolution>,
}

#[derive(Default)]
pub struct JobProgress {
    pub started: bool,
    pub current: String,
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub done_files: usize,
    pub total_files: usize,
    pub last_error: Option<String>,
}

#[derive(Default)]
pub struct Transfers {
    pub jobs: BTreeMap<JobId, JobProgress>,
    pub speed: f64,
    moved: u64,
    last: Option<(Instant, u64)>,
}

impl Transfers {
    pub fn running(&self) -> usize {
        self.jobs.values().filter(|j| j.started).count()
    }
    pub fn queued(&self) -> usize {
        self.jobs.len() - self.running()
    }
    pub fn totals(&self) -> (u64, u64) {
        self.jobs
            .values()
            .filter(|j| j.started)
            .fold((0, 0), |(d, t), j| (d + j.done_bytes, t + j.total_bytes))
    }
    pub fn progress(&mut self, id: JobId, current: String, done_bytes: u64, done_files: usize) {
        let j = self.jobs.entry(id).or_default();
        self.moved += done_bytes.saturating_sub(j.done_bytes);
        j.current = current;
        j.done_bytes = done_bytes;
        j.done_files = done_files;
    }
    pub fn sample(&mut self, now: Instant) {
        match self.last {
            None => self.last = Some((now, self.moved)),
            Some((t, m)) => {
                let secs = now.duration_since(t).as_secs_f64();
                if secs >= 1.0 {
                    self.speed = (self.moved - m) as f64 / secs;
                    self.last = Some((now, self.moved));
                }
            }
        }
    }
}

#[derive(Default)]
pub struct Hit {
    pub panes: [Rect; 2],   // inner row areas
    pub toolbar: Vec<Rect>, // toolbar[i] <-> TOOLBAR[i]
}
pub struct Drag {
    pub from: Side,
    pub items: Vec<Entry>, // fixed at press time
    pub active: bool,
}
pub struct App {
    pub panes: [Pane; 2],
    pub focus: Side,
    pub popup: Option<Popup>,
    pub conflicts: VecDeque<ConflictPrompt>,
    pub transfers: Transfers,
    pub status: String,
    pub hit: Hit,
    pub drag: Option<Drag>,
    pub last_click: Option<(Instant, Side, usize)>,
    pub icons: bool,
    pub quit: bool,
}
impl App {
    pub fn new(left: Pane, right: Pane, icons: bool) -> App {
        App {
            panes: [left, right],
            focus: Side::Left,
            popup: None,
            conflicts: VecDeque::new(),
            transfers: Transfers::default(),
            status: String::new(),
            hit: Hit::default(),
            drag: None,
            last_click: None,
            icons,
            quit: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn f(name: &str) -> Entry {
        Entry {
            name: name.into(),
            is_dir: false,
            size: 1,
            mtime: None,
            mode: 0o100644,
        }
    }
    fn pane(cwd: &str, names: &[&str]) -> Pane {
        let mut p = Pane::new(Fs::Local, cwd.into());
        p.set_listing(cwd.into(), names.iter().map(|n| f(n)).collect(), None);
        p
    }
    fn marks(p: &Pane) -> Vec<&str> {
        p.marked.iter().map(|s| s.as_str()).collect()
    }

    #[test]
    fn dotdot_prepended_except_root() {
        assert_eq!(pane("/a", &["x"]).entries[0].name, "..");
        let r = pane("/", &["x"]);
        assert_eq!(r.entries[0].name, "x");
        assert_eq!(r.entries.len(), 1);
    }

    #[test]
    fn shift_range_grows_and_shrinks() {
        let mut p = pane("/", &["a", "b", "c", "d", "e"]);
        p.move_cursor(1, true);
        p.move_cursor(1, true);
        assert_eq!(marks(&p), ["a", "b", "c"]);
        p.move_cursor(-1, true);
        assert_eq!(marks(&p), ["a", "b"]);
    }

    #[test]
    fn plain_move_keeps_marks_and_new_range_unions() {
        let mut p = pane("/", &["a", "b", "c", "d", "e"]);
        p.move_cursor(1, true); // a,b
        p.move_cursor(2, false); // cursor d
        assert_eq!(marks(&p), ["a", "b"]);
        p.move_cursor(1, true); // d,e
        assert_eq!(marks(&p), ["a", "b", "d", "e"]);
    }

    #[test]
    fn selection_excludes_dotdot_and_falls_back_to_cursor() {
        let mut p = pane("/a", &["x", "y"]);
        assert!(p.selection().is_empty()); // on ..
        p.cursor_to(2);
        assert_eq!(p.selection()[0].name, "y");
        p.move_cursor(-2, true); // from y up to ..
        let names: Vec<_> = p.selection().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["x", "y"]);
    }

    #[test]
    fn cd_clears_marks_refresh_keeps_existing() {
        let mut p = pane("/a", &["x", "y", "z", "w"]);
        p.marked.insert("x".into());
        p.marked.insert("gone".into());
        p.cursor_to(3);
        p.set_listing("/a".into(), vec![f("x"), f("y")], None);
        assert_eq!(marks(&p), ["x"]);
        assert_eq!(p.cursor, 2); // clamped
        p.cursor_to(2);
        p.set_listing("/b".into(), vec![f("q")], None);
        assert!(p.marked.is_empty());
        assert_eq!(p.cursor, 0);
    }

    #[test]
    fn select_name_positions_cursor() {
        let mut p = pane("/a", &["x", "y"]);
        p.set_listing("/a".into(), vec![f("x"), f("y")], Some("y"));
        assert_eq!(p.current().unwrap().name, "y");
    }

    #[test]
    fn transfers_running_queued_totals() {
        let mut t = Transfers::default();
        t.jobs.insert(
            1,
            JobProgress {
                started: true,
                done_bytes: 5,
                total_bytes: 10,
                ..Default::default()
            },
        );
        t.jobs.insert(
            2,
            JobProgress {
                done_bytes: 1,
                total_bytes: 100,
                ..Default::default()
            },
        );
        assert_eq!((t.running(), t.queued()), (1, 1));
        assert_eq!(t.totals(), (5, 10));
    }

    #[test]
    fn speed_sampling() {
        let (mut t, t0) = (Transfers::default(), Instant::now());
        t.sample(t0);
        t.progress(1, "f".into(), 1_000_000, 0);
        t.sample(t0 + Duration::from_secs(2));
        assert_eq!(t.speed, 500_000.0);
    }

    #[test]
    fn speed_survives_job_removal() {
        let (mut t, t0) = (Transfers::default(), Instant::now());
        t.sample(t0);
        t.progress(1, "f".into(), 1_000_000, 0);
        t.jobs.remove(&1);
        t.sample(t0 + Duration::from_secs(2));
        assert_eq!(t.speed, 500_000.0);
    }

    #[test]
    fn empty_pane_movement_does_not_panic() {
        let mut p = Pane::new(Fs::Local, "/".into());
        p.move_cursor(1, true);
        p.move_cursor(-1, true);
        p.move_cursor(1, false);
        p.cursor_to(5);
        assert!(p.current().is_none());
        assert!(p.selection().is_empty());
    }
}
