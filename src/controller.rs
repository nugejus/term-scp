use crate::fs::{Entry, Fs, join, parent, resolve_path};
use crate::model::{
    App, Command, ConflictPrompt, Drag, InputKind, JobProgress, MENU, Popup, Preview, Side, TOOLBAR,
};
use crate::preview::{Kind, LIMIT, Loaded, NO_PREVIEW, Player, TOO_BIG, load};
use crate::transfer::{self, Emit, Job, JobId, Resolution, TransferEvent};
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Position;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;
use tokio::io::AsyncReadExt;
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
    PreviewProgress {
        seq: u64,
        done: u64,
    },
    Previewed {
        seq: u64,
        loaded: Loaded,
        path: PathBuf,
        temp: Option<NamedTempFile>,
    },
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
        KeyCode::Backspace => Parent,
        KeyCode::Enter => Enter,
        KeyCode::Left => Focus(Side::Left),
        KeyCode::Right => Focus(Side::Right),
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
        KeyCode::Char('d') => Delete,
        KeyCode::Char('r') => Rename,
        KeyCode::Char('f') => Filter,
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

/// A remote file into a temp file of the same extension, reporting bytes done as it goes.
async fn download(
    fs: &Fs,
    path: &str,
    seq: u64,
    tx: &Sender<AppEvent>,
) -> Result<NamedTempFile, String> {
    let ext = Path::new(path).extension();
    let suffix = ext.map_or(String::new(), |x| format!(".{}", x.to_string_lossy()));
    let mut temp = tempfile::Builder::new()
        .prefix("term-scp-preview-")
        .suffix(&suffix)
        .tempfile()
        .map_err(|e| e.to_string())?;
    // Caps a file that grew after it was listed; the preview then shows its first LIMIT bytes.
    let mut r = fs.open_read(path).await?.take(LIMIT);
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0;
    loop {
        let n = r.read(&mut buf).await.map_err(|e| format!("{path}: {e}"))?;
        if n == 0 {
            return Ok(temp);
        }
        temp.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        let _ = tx.send(AppEvent::PreviewProgress { seq, done });
    }
}

pub struct Controller {
    rt: tokio::runtime::Handle,
    tx: Sender<AppEvent>,
    sem: Arc<Semaphore>,
    next_job: JobId,
    next_preview: u64,
}

impl Controller {
    pub fn new(rt: tokio::runtime::Handle, tx: Sender<AppEvent>) -> Controller {
        Controller {
            rt,
            tx,
            sem: Arc::new(Semaphore::new(4)),
            next_job: 1,
            next_preview: 0,
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
                    Ok((cwd, entries)) => {
                        p.set_listing(cwd, entries, select.as_deref());
                        // While the bar is open, the filter is what it shows, whatever folder landed.
                        if let Some(Popup::Input {
                            kind: InputKind::Filter,
                            value,
                            ..
                        }) = &app.popup
                            && side == app.focus
                        {
                            p.set_filter(value);
                        }
                    }
                    Err(e) => {
                        p.loading = None;
                        let tail = match &p.fs {
                            Fs::Remote(r) if !r.alive() => r.stderr_tail(),
                            _ => String::new(),
                        };
                        app.status = match tail.is_empty() {
                            true => e,
                            false => format!("{e} — {tail}"),
                        };
                    }
                }
            }
            AppEvent::OpDone {
                side,
                select,
                result,
            } => {
                // A failure reloads too (a delete that failed midway removed the rest),
                // but the cursor stays: a refused rename's target is not the user's file.
                let select = match result {
                    Ok(()) => select,
                    Err(e) => {
                        app.status = e;
                        None
                    }
                };
                if app.panes[side.idx()].loading.is_some() {
                    self.reload(app, side);
                } else {
                    let cwd = app.panes[side.idx()].cwd.clone();
                    self.load(app, side, cwd, select);
                }
            }
            AppEvent::Transfer(t) => self.on_transfer(app, t),
            // Only for the open preview: anything else is late, and drops (with its temp file).
            AppEvent::PreviewProgress { seq, done } => {
                if let Some(Popup::Preview(p)) = &mut app.popup
                    && p.seq == seq
                    && let Some(got) = &mut p.got
                {
                    *got = done;
                }
            }
            AppEvent::Previewed {
                seq,
                loaded,
                path,
                temp,
            } => {
                if let Some(Popup::Preview(p)) = &mut app.popup
                    && p.seq == seq
                {
                    p.kind = Some(loaded.kind);
                    p.image = loaded.image.map(|i| app.picker.new_resize_protocol(i));
                    (p.path, p.temp, p.got) = (Some(path), temp, None);
                }
            }
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
                    KeyCode::Esc => {
                        if let InputKind::Filter = kind {
                            app.panes[app.focus.idx()].set_filter("");
                        }
                        return;
                    }
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
                if let InputKind::Filter = kind {
                    app.panes[app.focus.idx()].set_filter(&value);
                }
                app.popup = Some(Popup::Input {
                    kind,
                    title,
                    value,
                    cursor,
                });
            }
            Some(Popup::Help) => {}
            // Every key is the preview's: none reaches the panes.
            Some(Popup::Preview(mut p)) => {
                let page = app.hit.preview.height.max(1) as usize;
                match k.code {
                    KeyCode::Esc => {
                        app.clear = true; // sixel/iTerm2 pixels outlive their cells
                        return;
                    }
                    KeyCode::Char(' ') => {
                        if let (Some(Kind::Media(probe)), Some(path)) = (&p.kind, &p.path)
                            && p.player.take().is_none()
                        {
                            match Player::spawn(path, probe, &p.name) {
                                Ok(pl) => p.player = Some(pl),
                                Err(_) => p.no_player = true,
                            }
                        }
                    }
                    // The view clamps.
                    KeyCode::Up => p.scroll = p.scroll.saturating_sub(1),
                    KeyCode::Down => p.scroll = p.scroll.saturating_add(1),
                    KeyCode::PageUp => p.scroll = p.scroll.saturating_sub(page),
                    KeyCode::PageDown => p.scroll = p.scroll.saturating_add(page),
                    KeyCode::Home => p.scroll = 0,
                    KeyCode::End => p.scroll = usize::MAX,
                    _ => {}
                }
                app.popup = Some(Popup::Preview(p));
            }
            Some(Popup::Menu { at, cursor }) => match k.code {
                KeyCode::Up | KeyCode::Down => {
                    let cursor = match k.code {
                        KeyCode::Up => cursor.saturating_sub(1),
                        _ => (cursor + 1).min(MENU.len() - 1),
                    };
                    app.popup = Some(Popup::Menu { at, cursor });
                }
                KeyCode::Enter => self.apply(app, MENU[cursor].2),
                KeyCode::Esc => {}
                _ => self.on_key(app, k), // the key shown beside an item works as always
            },
            Some(Popup::ConfirmQuit) => app.quit = matches!(k.code, KeyCode::Char('y' | 'Y')),
            Some(Popup::ConfirmDelete { dir, items }) => {
                if matches!(k.code, KeyCode::Char('y' | 'Y')) {
                    let fs = app.panes[app.focus.idx()].fs.clone();
                    self.op(app.focus, None, async move {
                        // Every item is tried; the last error is the one shown.
                        let mut result = Ok(());
                        for e in items {
                            if let Err(err) = fs.remove_all(&join(&dir, &e.name)).await {
                                result = Err(err);
                            }
                        }
                        result
                    });
                }
            }
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
                match k.code {
                    // A count: `15` then ↓ moves 15 rows. The status line shows it meanwhile.
                    KeyCode::Char(c @ '0'..='9') if !ctrl && (app.count > 0 || c != '0') => {
                        let d = c as usize - '0' as usize;
                        app.count = (app.count * 10 + d).min(99_999);
                    }
                    _ => {
                        if let Some(cmd) = key_to_command(k) {
                            self.apply(app, cmd);
                        }
                        app.count = 0;
                    }
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
        let pos = Position::new(m.column, m.row);
        if let Some(Popup::Menu { cursor, .. }) = &mut app.popup {
            let menu = app.hit.menu;
            let row = menu.contains(pos).then(|| (pos.y - menu.y) as usize);
            match (m.kind, row) {
                (MouseEventKind::Moved, Some(i)) => *cursor = i,
                (MouseEventKind::Down(MouseButton::Left), Some(i)) => {
                    app.popup = None;
                    return self.apply(app, MENU[i].2);
                }
                // A right-click elsewhere opens the menu there (below); other clicks just close it.
                (MouseEventKind::Down(MouseButton::Right), None) => app.popup = None,
                (MouseEventKind::Down(_), _) => {
                    app.popup = None;
                    return;
                }
                _ => {}
            }
        }
        if let Some(Popup::Preview(p)) = &mut app.popup {
            let over = app.hit.preview.contains(pos);
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) if app.hit.close.contains(pos) => {
                    app.popup = None;
                    app.clear = true;
                }
                MouseEventKind::ScrollUp if over => p.scroll = p.scroll.saturating_sub(3),
                MouseEventKind::ScrollDown if over => p.scroll = p.scroll.saturating_add(3),
                _ => {}
            }
            return;
        }
        if app.popup.is_some() || !app.conflicts.is_empty() {
            return;
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Right) => {
                let Some((s, row)) = under(app, pos) else {
                    return;
                };
                app.focus = s;
                if let Some(idx) = row {
                    // The menu acts on the clicked row, or on the marks when it is one of them.
                    let p = &mut app.panes[s.idx()];
                    if !p.marked.contains(&p.entries[idx].name) {
                        p.clear_marks();
                    }
                    p.cursor_to(idx);
                }
                app.popup = Some(Popup::Menu { at: pos, cursor: 0 });
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(i) = app.hit.toolbar.iter().position(|r| r.contains(pos)) {
                    return self.apply(app, TOOLBAR[i].1);
                }
                let Some((s, row)) = under(app, pos) else {
                    return;
                };
                // Blank space below the rows focuses too; only a row moves the cursor or drags.
                app.focus = s;
                let Some(idx) = row else {
                    return;
                };
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
                    at: None,
                });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(d) = &mut app.drag {
                    d.at = Some(pos);
                }
            }
            // Anywhere on the other box drops, border and title included. No motion is required:
            // a release over the other pane was a drag even if the terminal reported no movement.
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(d) = released
                    && !d.items.is_empty()
                    && app.hit.boxes[d.from.other().idx()].contains(pos)
                {
                    let s = d.from.other();
                    let row = under(app, pos).and_then(|(_, row)| row);
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
                let path = join(&dir, &value);
                self.op(side, Some(value), async move {
                    match mk {
                        true => fs.mkdir(&path).await,
                        false => fs.create_new(&path).await,
                    }
                });
            }
            InputKind::Rename { .. } if value.trim().is_empty() => {}
            // A path moves it, on the same side: `../x`, `~/x`.
            InputKind::Rename { dir, from } => {
                let (src, dst) = (join(&dir, &from), resolve_path(&value, &dir, &fs.home()));
                if src != dst {
                    let select = parent(&dst).map(|(_, n)| n);
                    self.op(side, select, async move { fs.rename_new(&src, &dst).await });
                }
            }
            InputKind::Filter => {} // applied as typed
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
        let n = app.count.max(1) as isize;
        let p = &mut app.panes[side.idx()];
        if matches!(cmd, CursorUp | CursorDown | PageUp | PageDown | Home | End) {
            p.clear_marks(); // moving without Shift ends the selection
        }
        match cmd {
            CursorUp => p.move_cursor(-n, false),
            CursorDown => p.move_cursor(n, false),
            ExtendUp => p.move_cursor(-n, true),
            ExtendDown => p.move_cursor(n, true),
            PageUp => p.move_cursor(-page, false),
            PageDown => p.move_cursor(page, false),
            Home => p.cursor_to(0),
            End => p.cursor_to(usize::MAX),
            Focus(s) => app.focus = s,
            SwitchPane => app.focus = side.other(),
            ClearMarks => p.clear_marks(),
            Enter => match p.current() {
                Some(e) if e.name == ".." => self.apply(app, Parent),
                Some(e) if e.is_dir => {
                    let path = join(&p.cwd, &e.name);
                    self.load(app, side, path, None);
                }
                Some(e) => {
                    let e = e.clone();
                    self.preview(app, side, e);
                }
                None => {}
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
            Delete => {
                let items = p.selection();
                match items.is_empty() {
                    true => app.status = "Nothing selected".into(),
                    false => {
                        let dir = p.cwd.clone();
                        app.popup = Some(Popup::ConfirmDelete { dir, items });
                    }
                }
            }
            Rename => match p.current() {
                Some(e) if e.name != ".." => {
                    let (from, dir) = (e.name.clone(), p.cwd.clone());
                    let value = from.clone();
                    app.popup = input(InputKind::Rename { dir, from }, "Rename", value);
                }
                _ => app.status = "Nothing selected".into(),
            },
            Filter => app.popup = input(InputKind::Filter, "Filter", p.filter.clone()),
            Help => app.popup = Some(Popup::Help),
            Refresh => self.reload(app, side),
            Quit if app.transfers.jobs.is_empty() => app.quit = true,
            Quit => app.popup = Some(Popup::ConfirmQuit),
        }
    }

    /// Opens the popup, Loading, and starts the task that fills it. Remote files are downloaded
    /// to a temp file first; one over LIMIT or not a regular file isn't, and gets its message at once.
    fn preview(&mut self, app: &mut App, side: Side, e: Entry) {
        self.next_preview += 1;
        let (seq, tx) = (self.next_preview, self.tx.clone());
        let pane = &app.panes[side.idx()];
        let (fs, path) = (pane.fs.clone(), join(&pane.cwd, &e.name));
        let (remote, regular) = (matches!(fs, Fs::Remote(_)), e.is_regular());
        let mut p = Preview {
            name: e.name,
            size: e.size,
            seq,
            kind: None,
            got: remote.then_some(0),
            image: None,
            scroll: 0,
            path: None,
            temp: None,
            player: None,
            no_player: false,
            task: None,
        };
        // A device or FIFO would read forever; load() guards the local side.
        if remote && (!regular || e.size > LIMIT) {
            let m = if regular { TOO_BIG } else { NO_PREVIEW };
            (p.kind, p.got) = (Some(Kind::Message(m.into())), None);
        } else {
            let task = self.rt.spawn(async move {
                let file = match remote {
                    true => download(&fs, &path, seq, &tx)
                        .await
                        .map(|t| (t.path().into(), Some(t))),
                    false => Ok((PathBuf::from(path), None)),
                };
                let (loaded, path, temp) = match file {
                    Ok((path, temp)) => {
                        let at = path.clone();
                        let loaded = tokio::task::spawn_blocking(move || load(&at))
                            .await
                            .unwrap_or_else(|e| Loaded {
                                kind: Kind::Message(format!("Can't read: {e}")), // load panicked
                                image: None,
                            });
                        (loaded, path, temp)
                    }
                    Err(e) => {
                        let kind = Kind::Message(format!("Can't read: {e}"));
                        (Loaded { kind, image: None }, PathBuf::new(), None)
                    }
                };
                let _ = tx.send(AppEvent::Previewed {
                    seq,
                    loaded,
                    path,
                    temp,
                });
            });
            p.task = Some(task.abort_handle());
        }
        app.popup = Some(Popup::Preview(Box::new(p)));
    }

    /// A one-off fs call; its OpDone reloads `side` with the cursor on `select`.
    fn op(
        &self,
        side: Side,
        select: Option<String>,
        f: impl Future<Output = Result<(), String>> + Send + 'static,
    ) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let result = f.await;
            let _ = tx.send(AppEvent::OpDone {
                side,
                select,
                result,
            });
        });
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
        if let Some(Popup::Preview(p)) = &mut app.popup
            && p.player.as_mut().is_some_and(|pl| pl.finished())
        {
            p.player = None; // ffplay ended on its own: the footer goes back to Space play
        }
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
    use crate::model::{Pane, Preview};
    use crate::preview::LIMIT;
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
            (K::Backspace, none, Parent),
            (K::Enter, none, Enter),
            (K::Left, none, Focus(Left)),
            (K::Right, none, Focus(Right)),
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
            (K::Char('d'), none, Delete),
            (K::Char('r'), none, Rename),
            (K::Char('f'), none, Filter),
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
        press(&mut t, KeyCode::Backspace);
        let root = t.dirs[0].clone();
        pump(&mut t, |a| a.panes[0].cwd == root);
        assert_eq!(t.app.panes[0].current().unwrap().name, "sub");
    }

    #[test]
    fn plain_move_after_shift_clears_marks() {
        let mut t = setup(&["a", "b", "c"], &[]);
        press(&mut t, KeyCode::Down);
        press_mod(&mut t, KeyCode::Down, KeyModifiers::SHIFT);
        assert_eq!(t.app.panes[0].marked.len(), 2);
        press(&mut t, KeyCode::Down);
        assert!(t.app.panes[0].marked.is_empty());
        assert_eq!(t.app.panes[0].current().unwrap().name, "c");
        press_mod(&mut t, KeyCode::Up, KeyModifiers::SHIFT);
        press(&mut t, KeyCode::End);
        assert!(t.app.panes[0].marked.is_empty());
    }

    #[test]
    fn count_prefix_moves_n_rows() {
        let names: Vec<String> = (10..40).map(|i| format!("f{i}")).collect();
        let mut t = setup(&names.iter().map(|s| s.as_str()).collect::<Vec<_>>(), &[]);
        typ(&mut t, "15");
        assert_eq!(t.app.count, 15);
        press(&mut t, KeyCode::Down);
        assert_eq!(t.app.panes[0].cursor, 15);
        press(&mut t, KeyCode::Down); // the count was used up
        assert_eq!(t.app.panes[0].cursor, 16);
        typ(&mut t, "3");
        press_mod(&mut t, KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(t.app.panes[0].cursor, 13);
        assert_eq!(t.app.panes[0].marked.len(), 4);
        typ(&mut t, "5");
        press(&mut t, KeyCode::Esc); // any other key drops it
        press(&mut t, KeyCode::Down);
        assert_eq!(t.app.panes[0].cursor, 14);
        typ(&mut t, "0"); // a leading 0 is no count
        assert_eq!((t.app.count, t.app.status.as_str()), (0, ""));
        typ(&mut t, "999");
        press(&mut t, KeyCode::Down);
        assert_eq!(t.app.panes[0].cursor, 30); // clamped to the last row
    }

    #[test]
    fn delete_asks_then_removes_files_and_folders() {
        let mut t = setup(&["a", "d/", "d/x", "keep"], &[]);
        press(&mut t, KeyCode::Char('d')); // on ..
        assert!(t.app.popup.is_none());
        assert_eq!(t.app.status, "Nothing selected");
        select(&mut t, "d");
        press_mod(&mut t, KeyCode::Down, KeyModifiers::SHIFT); // d, a
        press(&mut t, KeyCode::Char('d'));
        assert!(
            matches!(&t.app.popup, Some(Popup::ConfirmDelete { items, .. }) if items.len() == 2)
        );
        press(&mut t, KeyCode::Char('n'));
        assert!(t.app.popup.is_none());
        assert!(exists(join(&t.dirs[0], "a")));
        press(&mut t, KeyCode::Char('d'));
        press(&mut t, KeyCode::Char('y'));
        pump(&mut t, |a| names(&a.panes[0]) == ["..", "keep"]);
        assert!(!exists(join(&t.dirs[0], "d")));
        assert!(!exists(join(&t.dirs[0], "a")));
    }

    #[test]
    fn delete_failure_reports_and_reloads() {
        let mut t = setup(&["a", "b"], &[]);
        select(&mut t, "a");
        std::fs::remove_file(join(&t.dirs[0], "a")).unwrap(); // gone behind our back
        press(&mut t, KeyCode::Char('d'));
        press(&mut t, KeyCode::Char('y'));
        pump(&mut t, |a| !a.status.is_empty() && lists(a, Left, &["b"]));
        pump(&mut t, |a| !names(&a.panes[0]).contains(&"a"));
        assert!(t.app.status.contains("/a"), "{}", t.app.status);
    }

    #[test]
    fn rename_flow_refuses_existing_name() {
        let mut t = setup(&["a", "b"], &[]);
        select(&mut t, "a");
        press(&mut t, KeyCode::Char('r'));
        assert_eq!(input(&t.app), ("Rename", "a"));
        press(&mut t, KeyCode::Backspace);
        typ(&mut t, "c");
        press(&mut t, KeyCode::Enter);
        pump(&mut t, |a| {
            a.panes[0].current().is_some_and(|e| e.name == "c")
        });
        assert!(!exists(join(&t.dirs[0], "a")));
        press(&mut t, KeyCode::Char('r'));
        press_mod(&mut t, KeyCode::Char('u'), KeyModifiers::CONTROL);
        typ(&mut t, "b");
        press(&mut t, KeyCode::Enter);
        pump(&mut t, |a| !a.status.is_empty());
        assert!(t.app.status.contains("already exists"), "{}", t.app.status);
        assert_eq!(std::fs::read_to_string(join(&t.dirs[0], "b")).unwrap(), "b");
        assert!(exists(join(&t.dirs[0], "c")));
    }

    #[test]
    fn refused_rename_keeps_the_cursor_on_its_file() {
        let mut t = setup(&["a", "b", "c"], &[]);
        select(&mut t, "a");
        press(&mut t, KeyCode::Char('r'));
        press_mod(&mut t, KeyCode::Char('u'), KeyModifiers::CONTROL);
        typ(&mut t, "c");
        press(&mut t, KeyCode::Enter);
        pump(&mut t, |a| {
            !a.status.is_empty() && a.panes[0].loading.is_none()
        });
        assert!(t.app.status.contains("already exists"), "{}", t.app.status);
        assert_eq!(t.app.panes[0].current().unwrap().name, "a");
    }

    #[test]
    fn listing_landing_under_the_open_bar_takes_its_filter() {
        let mut t = setup(&["sub/", "sub/xa", "sub/yb", "xold"], &[]);
        select(&mut t, "sub");
        press(&mut t, KeyCode::Enter); // the load is in flight
        press(&mut t, KeyCode::Char('f'));
        typ(&mut t, "x");
        pump(&mut t, |a| a.panes[0].cwd.ends_with("/sub"));
        assert_eq!(names(&t.app.panes[0]), ["..", "xa"]);
        press(&mut t, KeyCode::Enter);
        assert_eq!(t.app.panes[0].filter, "x");
    }

    #[test]
    fn filter_live_enter_keeps_esc_clears() {
        let mut t = setup(&["apple", "banana", "cherry"], &[]);
        press(&mut t, KeyCode::Char('f'));
        typ(&mut t, "AN1");
        assert_eq!(names(&t.app.panes[0]), [".."]);
        press(&mut t, KeyCode::Backspace);
        assert_eq!(names(&t.app.panes[0]), ["..", "banana"]);
        assert_eq!(t.app.count, 0); // digits in the bar are text
        press(&mut t, KeyCode::Enter);
        assert!(t.app.popup.is_none());
        assert_eq!(t.app.panes[0].filter, "AN");
        press(&mut t, KeyCode::Char('f'));
        assert_eq!(input(&t.app), ("Filter", "AN"));
        press(&mut t, KeyCode::Esc);
        assert_eq!(t.app.panes[0].filter, "");
        assert_eq!(names(&t.app.panes[0]).len(), 4);
    }

    #[test]
    fn arrows_pick_the_pane() {
        let mut t = setup(&[], &[]);
        press(&mut t, KeyCode::Right);
        assert_eq!(t.app.focus, Right);
        press(&mut t, KeyCode::Right);
        assert_eq!(t.app.focus, Right);
        press(&mut t, KeyCode::Left);
        assert_eq!(t.app.focus, Left);
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
    fn click_below_last_row_focuses_pane() {
        let mut t = setup(&[], &["a.txt"]);
        draw(&mut t);
        let r = t.app.hit.panes[1];
        down(&mut t, (r.x + 1, r.y + r.height - 1));
        assert_eq!(t.app.focus, Right);
        assert_eq!(t.app.panes[1].cursor, 0);
        assert!(t.app.drag.is_none());
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
    fn menu_items_show_their_own_key() {
        for (label, key, cmd) in MENU {
            let k = KeyEvent::new(
                KeyCode::Char(key.chars().next().unwrap()),
                KeyModifiers::NONE,
            );
            assert_eq!(key_to_command(k), Some(cmd), "{label}");
        }
    }

    #[test]
    fn right_click_menu_acts_on_the_clicked_row_or_the_marks() {
        let mut t = setup(&["a.txt", "b.txt", "c.txt"], &[]);
        draw(&mut t);
        let right = |t: &mut T, at| mouse(t, MouseEventKind::Down(MouseButton::Right), at);
        let (a, c) = (at(&t, Left, "a.txt"), at(&t, Left, "c.txt"));
        t.app.panes[0].marked = ["a.txt", "b.txt"].map(String::from).into();
        right(&mut t, a); // a marked row: the menu acts on the marks
        assert_eq!(t.app.panes[0].marked.len(), 2);
        assert!(matches!(t.app.popup, Some(Popup::Menu { cursor: 0, .. })));
        press(&mut t, KeyCode::Char('d')); // an item's key works with the menu open
        assert!(
            matches!(&t.app.popup, Some(Popup::ConfirmDelete { items, .. }) if items.len() == 2)
        );
        press(&mut t, KeyCode::Esc);

        right(&mut t, c); // an unmarked row: only that one
        assert!(t.app.panes[0].marked.is_empty());
        assert_eq!(t.app.panes[0].current().unwrap().name, "c.txt");
        draw(&mut t);
        let m = t.app.hit.menu;
        mouse(&mut t, MouseEventKind::Moved, (m.x + 2, m.y + 4));
        assert!(matches!(t.app.popup, Some(Popup::Menu { cursor: 4, .. })));
        down(&mut t, (m.x + 2, m.y + 4)); // Delete
        assert!(
            matches!(&t.app.popup, Some(Popup::ConfirmDelete { items, .. })
            if items.len() == 1 && items[0].name == "c.txt")
        );
    }

    #[test]
    fn menu_arrows_enter_and_a_click_elsewhere() {
        let mut t = setup(&["a.txt"], &[]);
        draw(&mut t);
        let a = at(&t, Left, "a.txt");
        mouse(&mut t, MouseEventKind::Down(MouseButton::Right), a);
        press(&mut t, KeyCode::Down);
        press(&mut t, KeyCode::Down);
        press(&mut t, KeyCode::Enter);
        assert_eq!(input(&t.app).0, "Move to");
        press(&mut t, KeyCode::Esc);

        mouse(&mut t, MouseEventKind::Down(MouseButton::Right), a);
        draw(&mut t);
        let new_dir = t.app.hit.toolbar[0];
        down(&mut t, (new_dir.x, new_dir.y)); // only closes the menu
        assert!(t.app.popup.is_none());
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
    }

    #[test]
    fn drop_on_title_without_motion_events_copies() {
        let mut t = setup(&["a.txt"], &[]);
        draw(&mut t);
        let from = at(&t, Left, "a.txt");
        let title = t.app.hit.boxes[1];
        // Some terminals report no motion: a press here and a release there is still a drag.
        down(&mut t, from);
        assert!(t.app.drag.as_ref().unwrap().at.is_none());
        mouse(
            &mut t,
            MouseEventKind::Up(MouseButton::Left),
            (title.x + 3, title.y),
        );
        pump(&mut t, |a| lists(a, Right, &["a.txt"]));
        assert_eq!(t.app.status, "Copied 1 item(s)");
    }

    #[test]
    fn drag_tracks_pointer() {
        let mut t = setup(&["a.txt"], &[]);
        draw(&mut t);
        let a = at(&t, Left, "a.txt");
        down(&mut t, a);
        mouse(&mut t, MouseEventKind::Drag(MouseButton::Left), (50, 4));
        assert_eq!(t.app.drag.as_ref().unwrap().at, Some(Position::new(50, 4)));
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

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(w, h)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }
    /// Writes `name` into `side`'s dir and waits for the pane to list it, the cursor on it.
    fn put(t: &mut T, side: Side, name: &str, bytes: &[u8]) {
        let dir = t.dirs[side.idx()].clone();
        std::fs::write(join(&dir, name), bytes).unwrap();
        t.ctl.load(&mut t.app, side, dir, Some(name.into()));
        pump(t, |a| lists(a, side, &[name]));
    }
    fn pv(a: &App) -> &Preview {
        match &a.popup {
            Some(Popup::Preview(p)) => p,
            _ => panic!("no preview popup"),
        }
    }
    fn text(lines: &[&str]) -> Option<Kind> {
        Some(Kind::Text(lines.iter().map(|l| l.to_string()).collect()))
    }

    #[test]
    fn enter_on_a_file_previews_it() {
        let mut t = setup(&["sub/"], &[]);
        put(&mut t, Left, "notes.txt", b"one\ntwo\n");
        press(&mut t, KeyCode::Enter);
        assert_eq!(pv(&t.app).kind, None);
        pump(&mut t, |a| pv(a).kind.is_some());
        assert_eq!(pv(&t.app).kind, text(&["one", "two"]));
        press(&mut t, KeyCode::Esc);
        select(&mut t, "sub");
        press(&mut t, KeyCode::Enter);
        pump(&mut t, |a| a.panes[0].cwd.ends_with("/sub"));
    }

    #[test]
    fn preview_swallows_keys_and_esc_closes() {
        let mut t = setup(&[], &[]);
        put(&mut t, Left, "notes.txt", b"one\ntwo\n");
        press(&mut t, KeyCode::Enter);
        pump(&mut t, |a| pv(a).kind.is_some());
        typ(&mut t, "dcq5");
        press(&mut t, KeyCode::Down);
        assert_eq!(pv(&t.app).scroll, 1); // still the preview: no ConfirmDelete either
        assert!(!t.app.quit && t.app.count == 0 && t.app.transfers.jobs.is_empty());
        press(&mut t, KeyCode::Esc);
        assert!(t.app.popup.is_none() && t.app.clear);
    }

    #[test]
    fn click_on_close_and_double_click() {
        let mut t = setup(&[], &[]);
        put(&mut t, Left, "notes.txt", b"one\n");
        draw(&mut t);
        press(&mut t, KeyCode::Enter);
        t.app.hit.close = ratatui::layout::Rect::new(10, 2, 3, 1); // no draw after: it resets
        down(&mut t, (11, 2));
        assert!(t.app.popup.is_none());
        let n = at(&t, Left, "notes.txt");
        down(&mut t, n);
        assert!(t.app.popup.is_none(), "single click only selects");
        down(&mut t, n);
        assert!(matches!(t.app.popup, Some(Popup::Preview(_))));
    }

    #[test]
    fn late_result_is_dropped() {
        let mut t = setup(&[], &[]);
        put(&mut t, Left, "notes.txt", b"one\n");
        press(&mut t, KeyCode::Enter);
        let first = pv(&t.app).seq;
        press(&mut t, KeyCode::Esc);
        press(&mut t, KeyCode::Enter);
        let second = pv(&t.app).seq;
        assert_ne!(first, second);
        let loaded = Loaded {
            kind: Kind::Text(vec!["stale".into()]),
            image: None,
        };
        let (path, temp) = ("/stale".into(), None);
        let late = AppEvent::Previewed {
            seq: first,
            loaded,
            path,
            temp,
        };
        t.ctl.handle(&mut t.app, late);
        assert_eq!((pv(&t.app).seq, &pv(&t.app).kind), (second, &None));
        pump(&mut t, |a| pv(a).kind.is_some());
        assert_eq!(pv(&t.app).kind, text(&["one"]));
    }

    #[test]
    fn remote_preview_uses_and_removes_a_temp_file() {
        let mut t = setup(&[], &[]);
        let fs = t._rt.block_on(crate::fs::test_remote());
        t.app.panes[1] = Pane::new(fs, t.dirs[1].clone());
        put(&mut t, Right, "pic.png", &png(40, 30));
        press(&mut t, KeyCode::Right);
        press(&mut t, KeyCode::Enter);
        pump(&mut t, |a| pv(a).kind.is_some());
        assert!(matches!(pv(&t.app).kind, Some(Kind::Image(_))));
        let temp = pv(&t.app).temp.as_ref().unwrap().path().to_owned();
        let name = temp.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("term-scp-preview-") && name.ends_with(".png"));
        assert!(temp.exists());
        press(&mut t, KeyCode::Esc);
        assert!(!temp.exists());

        // Closed mid-download: the task stops and takes its temp file with it.
        let left_over = || {
            std::fs::read_dir(std::env::temp_dir()).unwrap().any(|e| {
                let n = e.unwrap().file_name().to_string_lossy().into_owned();
                n.starts_with("term-scp-preview-") && n.ends_with(".t3abort")
            })
        };
        put(&mut t, Right, "big.t3abort", &vec![b'x'; 4 << 20]);
        press(&mut t, KeyCode::Enter);
        pump(&mut t, |a| pv(a).got.is_some_and(|done| done > 0));
        assert!(left_over());
        press(&mut t, KeyCode::Esc);
        let end = Instant::now() + Duration::from_millis(500);
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            if let Ok(ev) = t.rx.recv_timeout(left) {
                t.ctl.handle(&mut t.app, ev); // a late Previewed holds its temp file until here
            }
        }
        assert!(!left_over());
    }

    #[test]
    fn closing_a_preview_aborts_its_task() {
        // The test above can't tell: a download that ran on sends a late Previewed, which drops.
        let mut t = setup(&[], &[]);
        put(&mut t, Left, "notes.txt", b"one\n");
        press(&mut t, KeyCode::Enter);
        let stuck = t._rt.spawn(std::future::pending::<()>()).abort_handle();
        let Some(Popup::Preview(p)) = &mut t.app.popup else {
            panic!("no preview popup")
        };
        p.task = Some(stuck.clone());
        press(&mut t, KeyCode::Esc);
        let end = Instant::now() + Duration::from_secs(5);
        while !stuck.is_finished() {
            assert!(Instant::now() < end, "the task still runs");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn remote_over_limit_is_not_downloaded() {
        let mut t = setup(&[], &[]);
        let (fs, cwd) = (t._rt.block_on(crate::fs::test_remote()), t.dirs[1].clone());
        t.app.panes[1] = Pane::new(fs, cwd.clone());
        let huge = Entry {
            size: LIMIT + 1,
            ..file("huge.bin")
        };
        t.app.panes[1].set_listing(cwd, vec![huge], Some("huge.bin"));
        press(&mut t, KeyCode::Right);
        press(&mut t, KeyCode::Enter);
        assert_eq!(pv(&t.app).kind, Some(Kind::Message(TOO_BIG.into())));
        assert!(pv(&t.app).task.is_none());
    }

    #[test]
    fn remote_device_is_not_downloaded() {
        let mut t = setup(&[], &[]);
        let (fs, cwd) = (t._rt.block_on(crate::fs::test_remote()), t.dirs[1].clone());
        t.app.panes[1] = Pane::new(fs, cwd.clone());
        let zero = Entry {
            mode: 0o020666, // a char device, size 0, like /dev/zero
            ..file("zero")
        };
        t.app.panes[1].set_listing(cwd, vec![zero], Some("zero"));
        press(&mut t, KeyCode::Right);
        press(&mut t, KeyCode::Enter);
        assert_eq!(pv(&t.app).kind, Some(Kind::Message(NO_PREVIEW.into())));
        assert!(pv(&t.app).task.is_none());
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
