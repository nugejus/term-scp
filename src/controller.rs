use crate::fs::{Entry, Fs, join, parent, resolve_path};
use crate::model::{
    App, Command, ConflictPrompt, Drag, InputKind, JobProgress, Popup, Side, TOOLBAR,
};
use crate::transfer::{self, Emit, Job, JobId, Resolution, TransferEvent};
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Position;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

pub enum AppEvent {
    Input(Event),
    Listed {
        side: Side,
        seq: u64,
        select: Option<String>,
        result: Result<(String, Vec<Entry>), String>,
    },
    OpDone {
        side: Side,
        select: Option<String>,
        result: Result<(), String>,
    },
    Transfer(TransferEvent),
}

/// The Key table. Letters match regardless of SHIFT; Ctrl+letter is only Ctrl+R and Ctrl+C.
pub fn key_to_command(k: KeyEvent) -> Option<Command> {
    use Command::*;
    if k.kind != KeyEventKind::Press {
        return None;
    }
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    Some(match k.code {
        KeyCode::Up if shift => ExtendUp,
        KeyCode::Down if shift => ExtendDown,
        KeyCode::Up => CursorUp,
        KeyCode::Down => CursorDown,
        KeyCode::PageUp => PageUp,
        KeyCode::PageDown => PageDown,
        KeyCode::Home => Home,
        KeyCode::End => End,
        KeyCode::Left => Parent,
        KeyCode::Right | KeyCode::Enter => Enter,
        KeyCode::Tab | KeyCode::BackTab => SwitchPane,
        KeyCode::Esc => ClearMarks,
        KeyCode::Char('r') if ctrl => Refresh,
        KeyCode::Char('c') if ctrl => Quit,
        KeyCode::Char(_) if ctrl => return None,
        KeyCode::Char('c') => QuickCopy,
        KeyCode::Char('C') => CopyTo,
        KeyCode::Char('m') => Move,
        KeyCode::Char('n') => NewDir,
        KeyCode::Char('N') => NewFile,
        KeyCode::Char('g') => ChangeDir,
        KeyCode::Char('?') => Help,
        KeyCode::Char('q') => Quit,
        _ => return None,
    })
}

/// One item and no trailing `/`: `value` is the exact destination (the engine puts the item
/// inside it if it is an existing dir of another name). Otherwise items go into it.
pub fn copy_targets(
    items: &[Entry],
    src_cwd: &str,
    value: &str,
    dst_cwd: &str,
    dst_home: &str,
) -> Vec<(String, String)> {
    let resolved = resolve_path(value, dst_cwd, dst_home);
    match items {
        [one] if !value.ends_with('/') => vec![(join(src_cwd, &one.name), resolved)],
        _ => items
            .iter()
            .map(|e| (join(src_cwd, &e.name), join(&resolved, &e.name)))
            .collect(),
    }
}

fn input(kind: InputKind, title: &str, value: String) -> Option<Popup> {
    let cursor = value.chars().count();
    Some(Popup::Input {
        kind,
        title: title.into(),
        value,
        cursor,
    })
}

fn byte_at(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(i, _)| i)
}

/// The pane under `pos`, and the entry index under it (None below the last row).
fn under(app: &App, pos: Position) -> Option<(Side, Option<usize>)> {
    let s = [Side::Left, Side::Right]
        .into_iter()
        .find(|s| app.hit.panes[s.idx()].contains(pos))?;
    let p = &app.panes[s.idx()];
    let i = p.offset + (pos.y - app.hit.panes[s.idx()].y) as usize;
    Some((s, (i < p.entries.len()).then_some(i)))
}

pub struct Controller {
    rt: tokio::runtime::Handle,
    tx: Sender<AppEvent>,
    sem: Arc<Semaphore>,
    next_job: JobId,
}

impl Controller {
    pub fn new(rt: tokio::runtime::Handle, tx: Sender<AppEvent>) -> Controller {
        Controller {
            rt,
            tx,
            sem: Arc::new(Semaphore::new(4)),
            next_job: 1,
        }
    }

    pub fn handle(&mut self, app: &mut App, ev: AppEvent) {
        match ev {
            AppEvent::Input(Event::Key(k)) => self.on_key(app, k),
            AppEvent::Input(Event::Mouse(m)) => self.on_mouse(app, m),
            AppEvent::Input(_) => {} // Resize: the next draw adapts.
            AppEvent::Listed {
                side,
                seq,
                select,
                result,
            } => {
                let p = &mut app.panes[side.idx()];
                if seq != p.seq {
                    return;
                }
                match result {
                    Ok((cwd, entries)) => p.set_listing(cwd, entries, select.as_deref()),
                    Err(e) => {
                        p.loading = None;
                        app.status = match &p.fs {
                            Fs::Remote(r) if !r.alive() => format!("{e} — {}", r.stderr_tail()),
                            _ => e,
                        };
                    }
                }
            }
            AppEvent::OpDone {
                side,
                select,
                result,
            } => match result {
                Ok(()) if app.panes[side.idx()].loading.is_some() => self.reload(app, side),
                Ok(()) => {
                    let cwd = app.panes[side.idx()].cwd.clone();
                    self.load(app, side, cwd, select);
                }
                Err(e) => app.status = e,
            },
            AppEvent::Transfer(t) => self.on_transfer(app, t),
        }
    }

    fn on_key(&mut self, app: &mut App, k: KeyEvent) {
        if k.kind != KeyEventKind::Press {
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match app.popup.take() {
            Some(Popup::Input {
                kind,
                title,
                mut value,
                mut cursor,
            }) => {
                match k.code {
                    KeyCode::Enter => return self.confirm(app, kind, value),
                    KeyCode::Esc => return,
                    KeyCode::Char('u') if ctrl => (value, cursor) = (String::new(), 0),
                    KeyCode::Char(c) if !ctrl => {
                        value.insert(byte_at(&value, cursor), c);
                        cursor += 1;
                    }
                    KeyCode::Backspace if cursor > 0 => {
                        cursor -= 1;
                        value.remove(byte_at(&value, cursor));
                    }
                    KeyCode::Left => cursor = cursor.saturating_sub(1),
                    KeyCode::Right => cursor = (cursor + 1).min(value.chars().count()),
                    KeyCode::Home => cursor = 0,
                    KeyCode::End => cursor = value.chars().count(),
                    _ => {}
                }
                app.popup = Some(Popup::Input {
                    kind,
                    title,
                    value,
                    cursor,
                });
            }
            Some(Popup::Help) => {}
            Some(Popup::ConfirmQuit) => app.quit = matches!(k.code, KeyCode::Char('y' | 'Y')),
            None if !app.conflicts.is_empty() => {
                let r = match k.code {
                    KeyCode::Char('o') => Resolution::Overwrite,
                    KeyCode::Char('s') => Resolution::Skip,
                    KeyCode::Char('a') => Resolution::OverwriteAll,
                    KeyCode::Esc => Resolution::Cancel,
                    _ => return,
                };
                if let Some(c) = app.conflicts.pop_front() {
                    let _ = c.reply.send(r); // Err: the job is already gone
                }
            }
            None => {
                // A message stays until the next key; whatever this key causes sets its own.
                app.status.clear();
                if let Some(cmd) = key_to_command(k) {
                    self.apply(app, cmd);
                }
            }
        }
    }

    fn on_mouse(&mut self, app: &mut App, m: MouseEvent) {
        // Any release ends the drag, even one the gate below swallows.
        let released = match m.kind {
            MouseEventKind::Up(_) => app.drag.take(),
            _ => None,
        };
        if app.popup.is_some() || !app.conflicts.is_empty() {
            return;
        }
        let pos = Position::new(m.column, m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(i) = app.hit.toolbar.iter().position(|r| r.contains(pos)) {
                    return self.apply(app, TOOLBAR[i].1);
                }
                let Some((s, Some(idx))) = under(app, pos) else {
                    return;
                };
                app.focus = s;
                let p = &mut app.panes[s.idx()];
                p.cursor_to(idx);
                // Fixed now, so a listing landing mid-drag can't change what is copied.
                let (e, src_cwd) = (&p.entries[idx], p.cwd.clone());
                let items = match () {
                    _ if p.marked.contains(&e.name) => p.selection(),
                    _ if e.name == ".." => vec![],
                    _ => vec![e.clone()],
                };
                let now = Instant::now();
                match app.last_click {
                    Some((t, ls, li))
                        if (ls, li) == (s, idx) && now - t < Duration::from_millis(400) =>
                    {
                        app.last_click = None;
                        self.apply(app, Command::Enter);
                    }
                    _ => app.last_click = Some((now, s, idx)),
                }
                app.drag = Some(Drag {
                    from: s,
                    items,
                    src_cwd,
                    active: false,
                });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(d) = &mut app.drag {
                    d.active = true;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(d) = released
                    && d.active
                    && !d.items.is_empty()
                    && let Some((s, row)) = under(app, pos)
                    && s == d.from.other()
                {
                    let dst = &app.panes[s.idx()];
                    let target = match row.map(|i| &dst.entries[i]) {
                        Some(e) if e.is_dir && e.name != ".." => join(&dst.cwd, &e.name),
                        _ => dst.cwd.clone(),
                    };
                    let pairs = d
                        .items
                        .iter()
                        .map(|e| (join(&d.src_cwd, &e.name), join(&target, &e.name)));
                    self.submit_copy(app, d.from, pairs.collect(), false);
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                if let Some((s, _)) = under(app, pos) {
                    let delta = if m.kind == MouseEventKind::ScrollUp {
                        -3
                    } else {
                        3
                    };
                    app.panes[s.idx()].move_cursor(delta, false);
                }
            }
            _ => {}
        }
    }

    /// Enter in an Input popup (already closed). Acts on the dir captured at open,
    /// never on the pane's cwd now: a listing may have landed meanwhile.
    fn confirm(&mut self, app: &mut App, kind: InputKind, value: String) {
        let side = app.focus; // focus cannot change while a popup is open
        let fs = app.panes[side.idx()].fs.clone();
        let mk = matches!(kind, InputKind::NewDir { .. });
        match kind {
            InputKind::NewDir { .. } | InputKind::NewFile { .. } if value.is_empty() => {}
            InputKind::NewDir { dir } | InputKind::NewFile { dir } => {
                let (path, tx) = (join(&dir, &value), self.tx.clone());
                self.rt.spawn(async move {
                    let result = match mk {
                        true => fs.mkdir(&path).await,
                        false => fs.create_new(&path).await,
                    };
                    let select = Some(value);
                    let _ = tx.send(AppEvent::OpDone {
                        side,
                        select,
                        result,
                    });
                });
            }
            InputKind::ChangeDir { base } => {
                let path = resolve_path(&value, &base, &fs.home());
                self.load(app, side, path, None);
            }
            // An emptied field would resolve to the other cwd and merge a dir into it.
            InputKind::CopyTo { .. } if value.trim().is_empty() => {}
            InputKind::CopyTo {
                from,
                items,
                mv,
                src_cwd,
            } => {
                // The prefilled value is absolute, so the other pane's cwd now is fine here.
                let dst = &app.panes[from.other().idx()];
                let targets = copy_targets(&items, &src_cwd, &value, &dst.cwd, &dst.fs.home());
                self.submit_copy(app, from, targets, mv);
            }
        }
    }

    pub fn apply(&mut self, app: &mut App, cmd: Command) {
        use Command::*;
        let side = app.focus;
        let page = app.hit.panes[side.idx()].height as isize;
        let p = &mut app.panes[side.idx()];
        match cmd {
            CursorUp => p.move_cursor(-1, false),
            CursorDown => p.move_cursor(1, false),
            ExtendUp => p.move_cursor(-1, true),
            ExtendDown => p.move_cursor(1, true),
            PageUp => p.move_cursor(-page, false),
            PageDown => p.move_cursor(page, false),
            Home => p.cursor_to(0),
            End => p.cursor_to(usize::MAX),
            SwitchPane => app.focus = side.other(),
            ClearMarks => p.clear_marks(),
            Enter => match p.current() {
                Some(e) if e.name == ".." => self.apply(app, Parent),
                Some(e) if e.is_dir => {
                    let path = join(&p.cwd, &e.name);
                    self.load(app, side, path, None);
                }
                _ => {}
            },
            Parent => {
                if let Some((dir, name)) = parent(&p.cwd) {
                    self.load(app, side, dir, Some(name));
                }
            }
            QuickCopy | CopyTo | Move => {
                let items = p.selection();
                let (src, dst) = (p.cwd.clone(), &app.panes[side.other().idx()].cwd);
                if items.is_empty() {
                    app.status = "Nothing selected".into();
                } else if cmd == QuickCopy {
                    let pairs = items
                        .iter()
                        .map(|e| (join(&src, &e.name), join(dst, &e.name)));
                    self.submit_copy(app, side, pairs.collect(), false);
                } else {
                    let value = match &items[..] {
                        [one] => join(dst, &one.name),
                        _ => dst.clone(),
                    };
                    let (mv, from, src_cwd) = (cmd == Move, side, src);
                    let kind = InputKind::CopyTo {
                        from,
                        items,
                        mv,
                        src_cwd,
                    };
                    let title = if mv { "Move to" } else { "Copy to" };
                    app.popup = input(kind, title, value);
                }
            }
            NewDir => {
                let kind = InputKind::NewDir { dir: p.cwd.clone() };
                app.popup = input(kind, "New Dir", String::new());
            }
            NewFile => {
                let kind = InputKind::NewFile { dir: p.cwd.clone() };
                app.popup = input(kind, "New File", String::new());
            }
            ChangeDir => {
                let base = p.cwd.clone();
                app.popup = input(InputKind::ChangeDir { base }, "Change Dir", p.cwd.clone());
            }
            Help => app.popup = Some(Popup::Help),
            Refresh => self.reload(app, side),
            Quit if app.transfers.jobs.is_empty() => app.quit = true,
            Quit => app.popup = Some(Popup::ConfirmQuit),
        }
    }

    pub fn load(&mut self, app: &mut App, side: Side, path: String, select: Option<String>) {
        let p = &mut app.panes[side.idx()];
        p.seq += 1;
        p.loading = Some((path.clone(), select.clone()));
        let (fs, seq, tx) = (p.fs.clone(), p.seq, self.tx.clone());
        self.rt.spawn(async move {
            let result = fs.list(&path).await;
            let _ = tx.send(AppEvent::Listed {
                side,
                seq,
                select,
                result,
            });
        });
    }

    /// Re-issues the in-flight navigation if there is one, so a refresh never cancels it.
    pub fn reload(&mut self, app: &mut App, side: Side) {
        let p = &app.panes[side.idx()];
        let (path, select) = p.loading.clone().unwrap_or_else(|| {
            let name = p.current().map(|e| e.name.clone());
            (p.cwd.clone(), name)
        });
        self.load(app, side, path, select);
    }

    pub fn submit_copy(
        &mut self,
        app: &mut App,
        from: Side,
        targets: Vec<(String, String)>,
        mv: bool,
    ) {
        let id = self.next_job;
        self.next_job += 1;
        // Before submit: a `q` right after `c` must already see the job.
        app.transfers.jobs.insert(id, JobProgress::default());
        let job = Job {
            id,
            src: app.panes[from.idx()].fs.clone(),
            dst: app.panes[from.other().idx()].fs.clone(),
            items: targets,
            mv,
        };
        let tx = self.tx.clone();
        let emit: Emit = Arc::new(move |e| {
            let _ = tx.send(AppEvent::Transfer(e));
        });
        transfer::submit(&self.rt, self.sem.clone(), job, emit);
    }

    fn on_transfer(&mut self, app: &mut App, ev: TransferEvent) {
        let t = &mut app.transfers;
        match ev {
            TransferEvent::Queued { id } => {
                t.jobs.entry(id).or_default();
            }
            TransferEvent::Started { id } => t.jobs.entry(id).or_default().started = true,
            TransferEvent::Scanned {
                id,
                total_bytes,
                total_files,
            } => {
                let j = t.jobs.entry(id).or_default();
                (j.total_bytes, j.total_files) = (total_bytes, total_files);
            }
            TransferEvent::Progress {
                id,
                current,
                done_bytes,
                done_files,
            } => t.progress(id, current, done_bytes, done_files),
            TransferEvent::Conflict { id, path, reply } => {
                let job = id;
                app.conflicts.push_back(ConflictPrompt { job, path, reply });
            }
            TransferEvent::ItemError { id, path, error } => {
                let msg = match error.starts_with(&path) {
                    true => format!("Job {id}: {error}"),
                    false => format!("Job {id}: {path}: {error}"),
                };
                // Kept for Finished, which replaces the status in the same batch.
                t.jobs.entry(id).or_default().last_error = Some(msg.clone());
                app.status = msg;
            }
            TransferEvent::Finished {
                id,
                copied,
                skipped,
                failed,
                cancelled,
                mv,
            } => {
                let last_error = t.jobs.remove(&id).and_then(|j| j.last_error);
                let verb = if mv { "Moved" } else { "Copied" };
                let mut s = format!("{verb} {copied} item(s)");
                if failed > 0 {
                    s += &format!(", {failed} failed");
                }
                if skipped > 0 {
                    s += &format!(", {skipped} skipped");
                }
                if cancelled {
                    s += " (cancelled)";
                }
                // Every ItemError counts as failed, so this only shows when failed > 0.
                if let Some(e) = last_error {
                    s += &format!(" — {e}");
                }
                app.status = s;
                self.reload(app, Side::Left);
                self.reload(app, Side::Right);
            }
        }
    }

    pub fn tick(&mut self, app: &mut App, now: Instant) {
        app.transfers.sample(now);
        for p in &app.panes {
            if let Fs::Remote(r) = &p.fs
                && let Some(line) = r.take_new_stderr()
            {
                app.status = line;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Pane;
    use Side::*;
    use std::sync::mpsc::Receiver;

    // Field order = drop order: the runtime stops before the tempdirs go.
    struct T {
        _rt: tokio::runtime::Runtime,
        ctl: Controller,
        app: App,
        rx: Receiver<AppEvent>,
        dirs: [String; 2],
        _tmp: [tempfile::TempDir; 2],
    }

    fn pump_until(
        ctl: &mut Controller,
        app: &mut App,
        rx: &Receiver<AppEvent>,
        pred: impl Fn(&App) -> bool,
    ) {
        while !pred(app) {
            let ev = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("timed out waiting for an event");
            ctl.handle(app, ev);
        }
    }

    /// Names ending in `/` are dirs; files contain their own name.
    fn setup(left: &[&str], right: &[&str]) -> T {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let tmp = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let dirs = tmp.each_ref().map(|d| {
            let p = std::fs::canonicalize(d.path()).unwrap();
            p.to_string_lossy().into_owned()
        });
        for (dir, names) in dirs.iter().zip([left, right]) {
            for n in names {
                let p = join(dir, n);
                match n.ends_with('/') {
                    true => std::fs::create_dir_all(&p).unwrap(),
                    false => std::fs::write(&p, n).unwrap(),
                }
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let mut ctl = Controller::new(rt.handle().clone(), tx);
        let pane = |d: &String| Pane::new(Fs::Local, d.clone());
        let mut app = App::new(pane(&dirs[0]), pane(&dirs[1]), false);
        ctl.reload(&mut app, Left);
        ctl.reload(&mut app, Right);
        pump_until(&mut ctl, &mut app, &rx, |a| {
            a.panes.iter().all(|p| p.loading.is_none())
        });
        T {
            _rt: rt,
            ctl,
            app,
            rx,
            dirs,
            _tmp: tmp,
        }
    }

    fn press_mod(t: &mut T, code: KeyCode, m: KeyModifiers) {
        let ev = AppEvent::Input(Event::Key(KeyEvent::new(code, m)));
        t.ctl.handle(&mut t.app, ev);
    }
    fn press(t: &mut T, code: KeyCode) {
        press_mod(t, code, KeyModifiers::NONE);
    }
    fn typ(t: &mut T, s: &str) {
        s.chars().for_each(|c| press(t, KeyCode::Char(c)));
    }
    fn pump(t: &mut T, pred: impl Fn(&App) -> bool) {
        pump_until(&mut t.ctl, &mut t.app, &t.rx, pred);
    }
    fn names(p: &Pane) -> Vec<&str> {
        p.entries.iter().map(|e| e.name.as_str()).collect()
    }
    fn lists(a: &App, side: Side, want: &[&str]) -> bool {
        let p = &a.panes[side.idx()];
        p.loading.is_none() && want.iter().all(|w| names(p).contains(w))
    }
    fn select(t: &mut T, name: &str) {
        let p = &mut t.app.panes[0];
        let i = names(p).iter().position(|n| *n == name).unwrap();
        p.cursor_to(i);
    }
    fn input(a: &App) -> (&str, &str) {
        match &a.popup {
            Some(Popup::Input { title, value, .. }) => (title, value),
            _ => panic!("no input popup"),
        }
    }
    fn file(name: &str) -> Entry {
        Entry {
            name: name.into(),
            is_dir: false,
            size: 0,
            mtime: None,
            mode: 0o100644,
        }
    }

    #[test]
    fn key_table() {
        use Command::*;
        use KeyCode as K;
        let (none, shift) = (KeyModifiers::NONE, KeyModifiers::SHIFT);
        let rows = [
            (K::Up, none, CursorUp),
            (K::Down, none, CursorDown),
            (K::Up, shift, ExtendUp),
            (K::Down, shift, ExtendDown),
            (K::PageUp, none, PageUp),
            (K::PageDown, none, PageDown),
            (K::Home, none, Home),
            (K::End, none, End),
            (K::Left, none, Parent),
            (K::Right, none, Enter),
            (K::Enter, none, Enter),
            (K::Tab, none, SwitchPane),
            (K::BackTab, shift, SwitchPane),
            (K::Esc, none, ClearMarks),
            (K::Char('c'), none, QuickCopy),
            (K::Char('C'), shift, CopyTo),
            (K::Char('C'), none, CopyTo),
            (K::Char('m'), none, Move),
            (K::Char('n'), none, NewDir),
            (K::Char('N'), shift, NewFile),
            (K::Char('g'), none, ChangeDir),
            (K::Char('?'), shift, Help),
            (K::Char('r'), KeyModifiers::CONTROL, Refresh),
            (K::Char('q'), none, Quit),
            (K::Char('c'), KeyModifiers::CONTROL, Quit),
        ];
        for (code, m, cmd) in rows {
            assert_eq!(
                key_to_command(KeyEvent::new(code, m)),
                Some(cmd),
                "{code:?}"
            );
        }
        let mut release = KeyEvent::new(K::Char('q'), none);
        release.kind = KeyEventKind::Release;
        assert_eq!(key_to_command(release), None);
        let ctrl_x = KeyEvent::new(K::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(key_to_command(ctrl_x), None);
    }

    #[test]
    fn enter_and_parent_restore_cursor() {
        let mut t = setup(&["a.txt", "sub/", "sub/x"], &[]);
        select(&mut t, "sub");
        press(&mut t, KeyCode::Enter);
        pump(&mut t, |a| a.panes[0].cwd.ends_with("/sub"));
        assert_eq!(names(&t.app.panes[0]), ["..", "x"]);
        assert_eq!(t.app.panes[0].cursor, 0);
        press(&mut t, KeyCode::Left);
        let root = t.dirs[0].clone();
        pump(&mut t, |a| a.panes[0].cwd == root);
        assert_eq!(t.app.panes[0].current().unwrap().name, "sub");
    }

    #[test]
    fn change_dir_absolute_and_bad_path() {
        let mut t = setup(&[], &[]);
        let ctrl = KeyModifiers::CONTROL;
        press(&mut t, KeyCode::Char('g'));
        assert_eq!(input(&t.app), ("Change Dir", t.dirs[0].as_str()));
        press_mod(&mut t, KeyCode::Char('u'), ctrl);
        let right = t.dirs[1].clone();
        typ(&mut t, &right);
        press(&mut t, KeyCode::Enter);
        assert!(t.app.popup.is_none());
        pump(&mut t, |a| a.panes[0].loading.is_none());
        assert_eq!(t.app.panes[0].cwd, right);

        press(&mut t, KeyCode::Char('g'));
        press_mod(&mut t, KeyCode::Char('u'), ctrl);
        typ(&mut t, &join(&right, "missing"));
        press(&mut t, KeyCode::Enter);
        pump(&mut t, |a| a.panes[0].loading.is_none());
        assert!(t.app.status.contains("missing"), "{}", t.app.status);
        assert_eq!(t.app.panes[0].cwd, right);
    }

    #[test]
    fn new_dir_flow() {
        let mut t = setup(&["z"], &[]);
        press(&mut t, KeyCode::Char('n'));
        typ(&mut t, "abc");
        press(&mut t, KeyCode::Enter);
        assert!(t.app.popup.is_none());
        pump(&mut t, |a| {
            a.panes[0].current().is_some_and(|e| e.name == "abc")
        });
        assert!(t.app.panes[0].current().unwrap().is_dir);
        assert!(std::path::Path::new(&join(&t.dirs[0], "abc")).is_dir());
    }

    #[test]
    fn new_file_existing_name_reports_error() {
        let mut t = setup(&["f"], &[]);
        let before = t.app.panes[0].entries.clone();
        press_mod(&mut t, KeyCode::Char('N'), KeyModifiers::SHIFT);
        assert_eq!(input(&t.app), ("New File", ""));
        typ(&mut t, "f");
        press(&mut t, KeyCode::Enter);
        pump(&mut t, |a| !a.status.is_empty());
        assert!(t.app.status.contains("exists"), "{}", t.app.status);
        assert_eq!(t.app.panes[0].entries, before);
    }

    #[test]
    fn copy_targets_cases() {
        let pairs = |v: &[(&str, &str)]| -> Vec<(String, String)> {
            v.iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect()
        };
        let items = [file("a"), file("b")];
        let one = &items[..1];
        assert_eq!(
            copy_targets(one, "/s", "/d/new", "/d", "/h"),
            pairs(&[("/s/a", "/d/new")])
        );
        assert_eq!(
            copy_targets(one, "/s", "sub/", "/d", "/h"),
            pairs(&[("/s/a", "/d/sub/a")])
        );
        assert_eq!(
            copy_targets(&items, "/s", "/d/sub", "/d", "/h"),
            pairs(&[("/s/a", "/d/sub/a"), ("/s/b", "/d/sub/b")])
        );
        assert_eq!(
            copy_targets(one, "/s", "~/x", "/d", "/h"),
            pairs(&[("/s/a", "/h/x")])
        );
    }

    #[test]
    fn quick_copy_end_to_end() {
        let mut t = setup(&["a.txt", "b.txt", "c.txt"], &[]);
        press(&mut t, KeyCode::Char('c')); // cursor on ".."
        assert_eq!(t.app.status, "Nothing selected");
        assert!(t.app.transfers.jobs.is_empty());
        press(&mut t, KeyCode::Down);
        press_mod(&mut t, KeyCode::Down, KeyModifiers::SHIFT);
        press(&mut t, KeyCode::Char('c'));
        pump(&mut t, |a| lists(a, Right, &["a.txt", "b.txt"]));
        assert!(t.app.transfers.jobs.is_empty());
        assert_eq!(t.app.status, "Copied 2 item(s)");
        assert_eq!(names(&t.app.panes[1]), ["..", "a.txt", "b.txt"]);
        let got = std::fs::read_to_string(join(&t.dirs[1], "b.txt")).unwrap();
        assert_eq!(got, "b.txt");
    }

    #[test]
    fn move_popup_then_enter_removes_sources() {
        let mut t = setup(&["a.txt", "b.txt"], &[]);
        press(&mut t, KeyCode::Down);
        press_mod(&mut t, KeyCode::Down, KeyModifiers::SHIFT);
        press(&mut t, KeyCode::Char('m'));
        assert_eq!(input(&t.app), ("Move to", t.dirs[1].as_str()));
        press(&mut t, KeyCode::Enter);
        pump(&mut t, |a| {
            lists(a, Right, &["a.txt", "b.txt"]) && names(&a.panes[0]) == [".."]
        });
        assert_eq!(t.app.status, "Moved 2 item(s)");
    }

    #[test]
    fn copy_to_single_item_prefills_dest_path() {
        let mut t = setup(&["a.txt"], &[]);
        press_mod(&mut t, KeyCode::Char('C'), KeyModifiers::SHIFT);
        assert!(t.app.popup.is_none(), "on '..'");
        assert_eq!(t.app.status, "Nothing selected");
        select(&mut t, "a.txt");
        press_mod(&mut t, KeyCode::Char('C'), KeyModifiers::SHIFT);
        let want = join(&t.dirs[1], "a.txt");
        assert_eq!(input(&t.app), ("Copy to", want.as_str()));
        typ(&mut t, ".bak");
        press(&mut t, KeyCode::Enter);
        pump(&mut t, |a| lists(a, Right, &["a.txt.bak"]));
        assert_eq!(t.app.status, "Copied 1 item(s)");
    }

    #[test]
    fn copy_to_empty_value_cancels() {
        let mut t = setup(&["a.txt", "d/", "d/x"], &[]);
        select(&mut t, "d");
        press(&mut t, KeyCode::Char('m'));
        press_mod(&mut t, KeyCode::Char('u'), KeyModifiers::CONTROL);
        typ(&mut t, "  ");
        press(&mut t, KeyCode::Enter);
        assert!(t.app.popup.is_none());
        assert!(t.app.transfers.jobs.is_empty());
        assert!(std::path::Path::new(&join(&t.dirs[0], "d/x")).exists());
    }

    #[test]
    fn copy_to_uses_cwd_captured_at_open() {
        let mut t = setup(&["x", "sub/", "sub/x"], &[]);
        let (a, sub) = (t.dirs[0].clone(), join(&t.dirs[0], "sub"));
        select(&mut t, "x");
        press(&mut t, KeyCode::Char('m'));
        let landed = AppEvent::Listed {
            side: Left,
            seq: t.app.panes[0].seq,
            select: None,
            result: Ok((sub.clone(), vec![file("x")])),
        };
        t.ctl.handle(&mut t.app, landed);
        assert_eq!(t.app.panes[0].cwd, sub);
        press(&mut t, KeyCode::Enter);
        pump(&mut t, |a| lists(a, Right, &["x"]));
        let exists = |p: String| std::path::Path::new(&p).exists();
        assert!(!exists(join(&a, "x")));
        assert!(exists(join(&t.dirs[1], "x")));
        assert!(exists(join(&sub, "x")));
    }

    #[test]
    fn finished_keeps_last_item_error() {
        let mut t = setup(&[], &[]);
        let (id, path) = (9, "/p/f".to_string());
        let error = "/p/f: Permission denied".to_string();
        for ev in [
            TransferEvent::Queued { id },
            TransferEvent::Started { id },
            TransferEvent::ItemError { id, path, error },
            TransferEvent::Finished {
                id,
                copied: 0,
                skipped: 0,
                failed: 1,
                cancelled: false,
                mv: false,
            },
        ] {
            t.ctl.handle(&mut t.app, AppEvent::Transfer(ev));
        }
        let s = &t.app.status;
        assert!(
            s.contains("1 failed") && s.contains("Permission denied"),
            "{s}"
        );
    }

    #[test]
    fn conflict_prompt_round_trip() {
        let mut t = setup(&["f"], &["f"]);
        std::fs::write(join(&t.dirs[0], "f"), "new").unwrap();
        select(&mut t, "f");
        press(&mut t, KeyCode::Char('c'));
        pump(&mut t, |a| !a.conflicts.is_empty());
        assert_eq!(t.app.conflicts[0].path, join(&t.dirs[1], "f"));
        press(&mut t, KeyCode::Char('o'));
        assert!(t.app.conflicts.is_empty());
        pump(&mut t, |a| a.status == "Copied 1 item(s)");
        let got = std::fs::read_to_string(join(&t.dirs[1], "f")).unwrap();
        assert_eq!(got, "new");
    }

    #[test]
    fn stale_listing_is_ignored() {
        let mut t = setup(&[], &[]);
        let root = t.dirs[0].clone();
        t.ctl.load(&mut t.app, Left, root.clone(), None);
        t.ctl.load(&mut t.app, Left, root, None);
        let seq = t.app.panes[0].seq;
        let listed = |seq, cwd: &str| AppEvent::Listed {
            side: Left,
            seq,
            select: None,
            result: Ok((cwd.into(), vec![file("x")])),
        };
        t.ctl.handle(&mut t.app, listed(seq - 1, "/stale"));
        assert_eq!(t.app.panes[0].cwd, t.dirs[0]);
        assert!(t.app.panes[0].loading.is_some());
        t.ctl.handle(&mut t.app, listed(seq, "/fresh"));
        assert_eq!(t.app.panes[0].cwd, "/fresh");
        assert!(t.app.panes[0].loading.is_none());
    }

    #[test]
    fn refresh_during_navigation_keeps_target() {
        let mut t = setup(&["sub/"], &[]);
        let sub = join(&t.dirs[0], "sub");
        t.ctl.load(&mut t.app, Left, sub.clone(), None);
        t.ctl.apply(&mut t.app, Command::Refresh);
        pump(&mut t, |a| a.panes[0].loading.is_none());
        assert_eq!(t.app.panes[0].cwd, sub);
    }

    #[test]
    fn op_done_during_navigation_keeps_target() {
        let mut t = setup(&["sub/"], &[]);
        let sub = join(&t.dirs[0], "sub");
        t.ctl.load(&mut t.app, Left, sub.clone(), None);
        let done = AppEvent::OpDone {
            side: Left,
            select: Some("zzz".into()),
            result: Ok(()),
        };
        t.ctl.handle(&mut t.app, done);
        pump(&mut t, |a| a.panes[0].loading.is_none());
        assert_eq!(t.app.panes[0].cwd, sub);
    }

    #[test]
    fn quit_confirms_when_running() {
        let mut t = setup(&["f"], &[]);
        select(&mut t, "f");
        press(&mut t, KeyCode::Char('c'));
        press(&mut t, KeyCode::Char('q'));
        assert!(matches!(t.app.popup, Some(Popup::ConfirmQuit)));
        press(&mut t, KeyCode::Char('n'));
        assert!(t.app.popup.is_none());
        assert!(!t.app.quit);
        press(&mut t, KeyCode::Char('q'));
        press(&mut t, KeyCode::Char('y'));
        assert!(t.app.quit);
        t.app.quit = false;
        press_mod(&mut t, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(t.app.popup, Some(Popup::ConfirmQuit)));
        press_mod(&mut t, KeyCode::Char('Y'), KeyModifiers::SHIFT);
        assert!(t.app.quit);
    }

    #[test]
    fn key_clears_status() {
        let mut t = setup(&["a"], &[]);
        t.app.status = "boom".into();
        press(&mut t, KeyCode::Down);
        assert_eq!(t.app.status, "");
    }

    #[test]
    fn quit_without_jobs_is_immediate() {
        let mut t = setup(&[], &[]);
        press(&mut t, KeyCode::Char('q'));
        assert!(t.app.quit);
    }

    #[test]
    fn popup_text_editing() {
        let mut t = setup(&[], &[]);
        press(&mut t, KeyCode::Char('n'));
        assert_eq!(input(&t.app), ("New Dir", ""));
        typ(&mut t, "a가d");
        press(&mut t, KeyCode::Left);
        press(&mut t, KeyCode::Backspace);
        typ(&mut t, "c");
        assert_eq!(input(&t.app).1, "acd");
        press(&mut t, KeyCode::Home);
        typ(&mut t, "x");
        press(&mut t, KeyCode::End);
        typ(&mut t, "y");
        assert_eq!(input(&t.app).1, "xacdy");
        press_mod(&mut t, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(input(&t.app).1, "");
        press(&mut t, KeyCode::Esc);
        assert!(t.app.popup.is_none());
    }

    /// Fills `app.hit` (and pane offsets) the way a real frame does.
    fn draw(t: &mut T) {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
        term.draw(|f| crate::view::draw(f, &mut t.app)).unwrap();
    }
    fn mouse(t: &mut T, kind: MouseEventKind, (column, row): (u16, u16)) {
        let m = MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        t.ctl.handle(&mut t.app, AppEvent::Input(Event::Mouse(m)));
    }
    fn down(t: &mut T, at: (u16, u16)) {
        mouse(t, MouseEventKind::Down(MouseButton::Left), at);
    }
    /// Press on `from`, drag, release on `to`.
    fn drag(t: &mut T, from: (u16, u16), to: (u16, u16)) {
        down(t, from);
        mouse(t, MouseEventKind::Drag(MouseButton::Left), to);
        mouse(t, MouseEventKind::Up(MouseButton::Left), to);
    }
    /// Screen cell of `name`'s row in `side`.
    fn at(t: &T, side: Side, name: &str) -> (u16, u16) {
        let p = &t.app.panes[side.idx()];
        let i = names(p).iter().position(|n| *n == name).unwrap();
        let r = t.app.hit.panes[side.idx()];
        (r.x + 1, r.y + (i - p.offset) as u16)
    }
    fn exists(p: String) -> bool {
        std::path::Path::new(&p).exists()
    }

    #[test]
    fn click_focuses_and_moves_cursor() {
        let mut t = setup(&[], &["a.txt", "b.txt"]);
        draw(&mut t);
        let b = at(&t, Right, "b.txt");
        down(&mut t, b);
        assert_eq!(t.app.focus, Right);
        assert_eq!(t.app.panes[1].current().unwrap().name, "b.txt");
    }

    #[test]
    fn double_click_enters_dir() {
        let mut t = setup(&["a.txt", "sub/", "sub/x"], &[]);
        draw(&mut t);
        let sub = at(&t, Left, "sub");
        down(&mut t, sub);
        assert!(
            t.app.panes[0].loading.is_none(),
            "single click only selects"
        );
        down(&mut t, sub);
        pump(&mut t, |a| a.panes[0].cwd.ends_with("/sub"));
        assert_eq!(names(&t.app.panes[0]), ["..", "x"]);
    }

    #[test]
    fn toolbar_click_opens_new_dir_popup() {
        let mut t = setup(&[], &[]);
        draw(&mut t);
        let r = t.app.hit.toolbar[0];
        down(&mut t, (r.x, r.y));
        assert_eq!(input(&t.app), ("New Dir", ""));
    }

    #[test]
    fn wheel_moves_cursor_without_focus_change() {
        let mut t = setup(&[], &["a", "b", "c", "d", "e"]);
        draw(&mut t);
        let over = at(&t, Right, "..");
        mouse(&mut t, MouseEventKind::ScrollDown, over);
        assert_eq!(t.app.focus, Left);
        assert_eq!(t.app.panes[1].cursor, 3);
        mouse(&mut t, MouseEventKind::ScrollUp, over);
        assert_eq!(t.app.panes[1].cursor, 0);
    }

    #[test]
    fn drag_to_other_pane_copies_into_cwd() {
        let mut t = setup(&["a.txt", "b.txt"], &[]);
        draw(&mut t);
        // Dropped on the other pane's ".." row: that is still "into its cwd".
        let (from, to) = (at(&t, Left, "b.txt"), at(&t, Right, ".."));
        drag(&mut t, from, to);
        assert!(t.app.drag.is_none());
        pump(&mut t, |a| lists(a, Right, &["b.txt"]));
        assert_eq!(t.app.status, "Copied 1 item(s)");
        assert_eq!(names(&t.app.panes[1]), ["..", "b.txt"]);
    }

    #[test]
    fn drag_onto_dir_row_copies_into_that_dir() {
        let mut t = setup(&["a.txt"], &["d/"]);
        draw(&mut t);
        let (from, to) = (at(&t, Left, "a.txt"), at(&t, Right, "d"));
        drag(&mut t, from, to);
        pump(&mut t, |a| a.status == "Copied 1 item(s)");
        assert!(exists(join(&t.dirs[1], "d/a.txt")));
        assert!(!exists(join(&t.dirs[1], "a.txt")));
    }

    #[test]
    fn drag_marked_row_copies_whole_selection() {
        let mut t = setup(&["a.txt", "b.txt", "c.txt"], &[]);
        draw(&mut t);
        let p = &mut t.app.panes[0];
        p.marked.extend(["a.txt".to_string(), "c.txt".to_string()]);
        let (from, to) = (at(&t, Left, "c.txt"), at(&t, Right, ".."));
        drag(&mut t, from, to);
        pump(&mut t, |a| lists(a, Right, &["a.txt", "c.txt"]));
        assert_eq!(names(&t.app.panes[1]), ["..", "a.txt", "c.txt"]);
    }

    #[test]
    fn drag_items_fixed_at_press() {
        let mut t = setup(&["b.txt"], &[]);
        draw(&mut t);
        let (from, to) = (at(&t, Left, "b.txt"), at(&t, Right, ".."));
        down(&mut t, from);
        // A refresh lands mid-drag: a0.txt now sits where b.txt was pressed.
        std::fs::write(join(&t.dirs[0], "a0.txt"), "a0").unwrap();
        let cwd = t.dirs[0].clone();
        let listing = vec![file("a0.txt"), file("b.txt")];
        t.app.panes[0].set_listing(cwd, listing, None);
        mouse(&mut t, MouseEventKind::Drag(MouseButton::Left), to);
        mouse(&mut t, MouseEventKind::Up(MouseButton::Left), to);
        pump(&mut t, |a| lists(a, Right, &["b.txt"]));
        assert_eq!(names(&t.app.panes[1]), ["..", "b.txt"]);
    }

    #[test]
    fn drop_uses_cwd_captured_at_press() {
        let mut t = setup(&["x", "sub/", "sub/x"], &[]);
        draw(&mut t);
        let (from, to) = (at(&t, Left, "x"), at(&t, Right, ".."));
        down(&mut t, from);
        // A navigation into sub/ (which has its own `x`) lands mid-drag.
        let landed = AppEvent::Listed {
            side: Left,
            seq: t.app.panes[0].seq,
            select: None,
            result: Ok((join(&t.dirs[0], "sub"), vec![file("x")])),
        };
        t.ctl.handle(&mut t.app, landed);
        mouse(&mut t, MouseEventKind::Drag(MouseButton::Left), to);
        mouse(&mut t, MouseEventKind::Up(MouseButton::Left), to);
        pump(&mut t, |a| lists(a, Right, &["x"]));
        let got = std::fs::read_to_string(join(&t.dirs[1], "x")).unwrap();
        assert_eq!(got, "x");
    }

    #[test]
    fn release_while_gated_ends_drag_without_copy() {
        let mut t = setup(&["a.txt"], &[]);
        draw(&mut t);
        let (from, to) = (at(&t, Left, "a.txt"), at(&t, Right, ".."));
        down(&mut t, from);
        mouse(&mut t, MouseEventKind::Drag(MouseButton::Left), to);
        press(&mut t, KeyCode::Char('n'));
        mouse(&mut t, MouseEventKind::Up(MouseButton::Left), to);
        assert!(t.app.drag.is_none());
        assert!(t.app.transfers.jobs.is_empty());
    }

    #[test]
    fn drop_on_same_pane_does_nothing() {
        let mut t = setup(&["a.txt", "d/"], &[]);
        draw(&mut t);
        let (from, to) = (at(&t, Left, "a.txt"), at(&t, Left, "d"));
        drag(&mut t, from, to);
        assert!(t.app.drag.is_none());
        assert!(t.app.transfers.jobs.is_empty());
        // A plain click-release over the other pane is no drop either.
        let other = at(&t, Right, "..");
        down(&mut t, from);
        mouse(&mut t, MouseEventKind::Up(MouseButton::Left), other);
        assert!(t.app.transfers.jobs.is_empty());
    }

    #[test]
    fn mouse_ignored_while_popup_open() {
        let mut t = setup(&[], &["a.txt"]);
        draw(&mut t);
        let a = at(&t, Right, "a.txt");
        press(&mut t, KeyCode::Char('n'));
        down(&mut t, a);
        mouse(&mut t, MouseEventKind::ScrollDown, a);
        assert_eq!(t.app.focus, Left);
        assert_eq!(t.app.panes[1].cursor, 0);
        assert!(t.app.drag.is_none());
        assert_eq!(input(&t.app), ("New Dir", ""));
        press(&mut t, KeyCode::Esc);
        let (reply, _rx) = tokio::sync::oneshot::channel();
        let path = String::new();
        t.app.conflicts.push_back(ConflictPrompt {
            job: 1,
            path,
            reply,
        });
        down(&mut t, a);
        assert_eq!(t.app.focus, Left);
        assert!(t.app.drag.is_none());
    }

    #[test]
    fn help_closes_on_any_key() {
        let mut t = setup(&[], &[]);
        press_mod(&mut t, KeyCode::Char('?'), KeyModifiers::SHIFT);
        assert!(matches!(t.app.popup, Some(Popup::Help)));
        press(&mut t, KeyCode::Char('q'));
        assert!(t.app.popup.is_none());
        assert!(!t.app.quit);
    }
}
