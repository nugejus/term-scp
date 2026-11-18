use crate::icons::icon;
use crate::model::{App, InputKind, MENU, Popup, Side, TOOLBAR, Transfers};
use chrono::{Datelike, Local, TimeZone};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, Padding, Paragraph, Row, Table, TableState,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The Key table from Global Constraints: (key, help text).
const HELP: [(&str, &str); 22] = [
    ("↑ / ↓", "Move cursor"),
    ("Shift+↑ / Shift+↓", "Select range"),
    ("15 ↓ / 15 ↑", "Move 15 rows (any number)"),
    ("PgUp / PgDn / Home / End", "Page / first / last"),
    ("Backspace", "Parent dir"),
    ("Enter", "Open dir"),
    ("← / →", "Left / right pane"),
    ("Tab / BackTab", "Switch pane"),
    ("Esc", "Clear selection"),
    ("c", "Copy to other pane"),
    ("C", "Copy to… (rename / path)"),
    ("m", "Move to…"),
    ("n", "New dir"),
    ("N", "New file"),
    ("g", "Change dir"),
    ("d", "Delete"),
    ("r", "Rename"),
    ("f", "Filter by name"),
    ("?", "Help"),
    ("Ctrl+R", "Refresh"),
    ("q", "Quit"),
    ("Ctrl+C", "Quit"),
];

pub fn draw(f: &mut Frame, app: &mut App) {
    app.hit.toolbar.clear();
    app.hit.menu = Rect::default();
    let [top, mid, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(f.area());

    let mut x = top.x;
    for (label, _) in TOOLBAR {
        let text = format!("[{label}]");
        let r = Rect::new(x, top.y, text.width() as u16, top.height).intersection(top);
        f.render_widget(Span::styled(text, Modifier::BOLD), r);
        app.hit.toolbar.push(r);
        x = x.saturating_add(r.width).saturating_add(1);
    }
    let rest = Rect {
        x,
        width: top.right().saturating_sub(x),
        ..top
    };
    // Ruling R7: under 16 cells the progress line moves to the status line.
    let progress_in_toolbar = rest.width >= 16;
    if progress_in_toolbar {
        let progress = progress_line(&app.transfers, rest.width as usize);
        f.render_widget(Line::from(progress).right_aligned(), rest);
    }

    // A cell is about twice as tall as wide: on a portrait screen the panes stack, left on top.
    let area = f.area();
    let dir = match area.height * 2 > area.width {
        true => Direction::Vertical,
        false => Direction::Horizontal,
    };
    let halves: [Rect; 2] = Layout::new(dir, [Constraint::Fill(1); 2]).areas(mid);
    app.hit.boxes = halves;
    for side in [Side::Left, Side::Right] {
        app.hit.panes[side.idx()] = draw_pane(f, app, side, halves[side.idx()]);
    }

    let right = match &app.drag {
        Some(d) if d.at.is_some() && !d.items.is_empty() => {
            format!("Drop to copy {} item(s)", d.items.len())
        }
        _ if app.count > 0 => app.count.to_string(), // no message can hide a pending count
        _ => app.status.clone(),
    };
    let p = &app.panes[app.focus.idx()];
    let left = if !progress_in_toolbar && app.transfers.running() > 0 {
        let full = bottom.width as usize;
        let w = full.saturating_sub(right.width() + 1).max(16).min(full);
        progress_line(&app.transfers, w)
    } else if p.marked.is_empty() {
        String::new()
    } else {
        let sum = p
            .entries
            .iter()
            .filter(|e| !e.is_dir && p.marked.contains(&e.name));
        let sum = sum.map(|e| e.size).sum();
        format!("{} marked · {}", p.marked.len(), human_size(sum))
    };
    let gap = (left.width() as u16).saturating_add(1);
    f.render_widget(Line::from(left), bottom);
    let r = Rect {
        x: bottom.x.saturating_add(gap),
        width: bottom.width.saturating_sub(gap),
        ..bottom
    };
    f.render_widget(Line::from(right).right_aligned(), r);
    draw_ghost(f, app);

    let full = f.area().width as usize;
    match &app.popup {
        Some(Popup::Input {
            kind: InputKind::Filter,
            ..
        }) => {} // a bar in the pane, drawn by draw_pane
        Some(Popup::Input {
            kind,
            title,
            value,
            cursor,
        }) => {
            // A third of the screen whatever the cursor does; long text wraps, read from its start.
            let inner = (full / 3).saturating_sub(4);
            let mut lines = wrap_input(value, *cursor, inner);
            let mut hints = vec!["Enter confirm", "Esc cancel"];
            if matches!(kind, InputKind::CopyTo { .. }) {
                hints.push("existing dir → inside");
            }
            let hints = pack(&hints, inner).into_iter();
            lines.extend(hints.map(|h| Line::styled(h, Color::DarkGray)));
            popup(f, title, lines, inner);
        }
        Some(Popup::Help) => {
            let lines = HELP
                .iter()
                .map(|(k, h)| Line::from(format!("{k:<24}  {h}")));
            popup(f, "Help", lines.collect(), 0);
        }
        Some(Popup::ConfirmQuit) => {
            let q = "Transfers are running. Quit and cancel them? [y/N]";
            popup(f, "Quit", vec![Line::from(q)], 0);
        }
        Some(Popup::ConfirmDelete { items, .. }) => {
            let what = match &items[..] {
                // Cut to fit, so `? [y/N]` always shows.
                [one] => format!("\"{}\"", fit_left(&one.name, full.saturating_sub(20))),
                v => format!("{} items", v.len()),
            };
            let mut lines = vec![Line::from(format!("Delete {what}? [y/N]"))];
            if items.iter().any(|e| e.is_dir) {
                let note = "Folders go with everything in them.";
                lines.push(Line::styled(note, Color::DarkGray));
            }
            popup(f, "Delete", lines, 0);
        }
        &Some(Popup::Menu { at, cursor }) => app.hit.menu = draw_menu(f, at, cursor),
        None => {
            if let Some(c) = app.conflicts.front() {
                let head = format!("Job {}: file exists: ", c.job);
                let path = fit_left(&c.path, full.saturating_sub(4 + head.width()));
                let opts = "[o] Overwrite  [s] Skip  [a] Overwrite all  [Esc] Cancel job";
                popup(
                    f,
                    "Conflict",
                    vec![Line::from(head + &path), Line::from(opts)],
                    0,
                );
            }
        }
    }
}

fn draw_pane(f: &mut Frame, app: &mut App, side: Side, area: Rect) -> Rect {
    let (focused, icons) = (app.focus == side, app.icons);
    let p = &mut app.panes[side.idx()];
    let (border, kind, cursor) = if focused {
        let border = Style::new()
            .fg(Color::LightCyan)
            .add_modifier(Modifier::BOLD);
        let cursor = Style::new()
            .bg(Color::LightCyan)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD);
        (border, BorderType::Thick, cursor)
    } else {
        (
            Style::new().fg(Color::DarkGray),
            BorderType::Plain,
            Style::new().bg(Color::DarkGray),
        )
    };
    let block = Block::bordered().border_type(kind).border_style(border);
    let inner = block.inner(area);
    // The filter's bar on the top row: the line being typed, or the filter in force.
    let label = Span::styled("Filter: ", Style::new().fg(Color::Yellow).bold());
    let bar = match &app.popup {
        Some(Popup::Input {
            kind: InputKind::Filter,
            value,
            cursor,
            ..
        }) if focused => {
            // One row: the end of the text before the cursor, the cursor cell, then the rest.
            let chars: Vec<char> = value.chars().collect();
            let before: String = chars[..*cursor].iter().collect();
            let at = chars.get(*cursor).map_or(" ".into(), char::to_string);
            let after: String = chars.iter().skip(*cursor + 1).collect();
            let w = (inner.width as usize).saturating_sub(label.width() + at.width());
            Some(Line::from(vec![
                label,
                Span::raw(fit_left(&before, w)),
                Span::styled(at, Modifier::REVERSED),
                Span::raw(after),
                Span::styled("  Enter keep · Esc clear", Color::DarkGray),
            ]))
        }
        _ if p.filter.is_empty() => None,
        _ => Some(Line::from(vec![label, Span::raw(p.filter.clone())])),
    };
    let rows_area = match bar {
        None => inner,
        Some(bar) => {
            let [top, rest] =
                Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
            f.render_widget(bar, top);
            rest
        }
    };

    let mut suffix = vec![];
    if p.loading.is_some() {
        suffix.push(Span::raw(" (loading)"));
    }
    if !p.fs.alive() {
        suffix.push(Span::styled(" [disconnected]", Color::Red));
    }
    let sw: usize = suffix.iter().map(Span::width).sum();
    let path = format!("{}:{}", p.fs.label(), p.cwd);
    let mut title = vec![
        Span::raw(" "),
        Span::raw(fit_left(
            &path,
            (inner.width as usize).saturating_sub(2 + sw),
        )),
    ];
    title.extend(suffix);
    title.push(Span::raw(" "));
    f.render_widget(block.title(Line::from(title)), area);

    let now = Local::now().timestamp();
    let marked = Style::new().bg(Color::Indexed(24)).fg(Color::White);
    let rows = p.entries.iter().map(|e| {
        let mut name = vec![];
        if icons {
            let (g, c) = icon(&e.name, e.is_dir);
            name.push(Span::styled(format!("{g} "), c));
        }
        name.push(Span::raw(e.name.as_str()));
        let size = if e.is_dir {
            String::new()
        } else {
            human_size(e.size)
        };
        let row = Row::new([
            Cell::from(Line::from(name)),
            Cell::from(Line::from(size).right_aligned()),
            Cell::from(fmt_mtime(e.mtime, now)),
        ]);
        if p.marked.contains(&e.name) {
            row.style(marked)
        } else {
            row
        }
    });
    let n = match inner.width {
        0..24 => 1,
        24..40 => 2,
        _ => 3,
    };
    let widths = [
        Constraint::Fill(1),
        Constraint::Length(6),
        Constraint::Length(11),
    ];
    let table = Table::new(rows, &widths[..n]).row_highlight_style(cursor);
    let mut state = TableState::default()
        .with_offset(p.offset)
        .with_selected(Some(p.cursor));
    f.render_stateful_widget(table, rows_area, &mut state);
    p.offset = state.offset();
    rows_area
}

/// Centered bordered box sized to its lines (at least `min_w` inner columns), clamped to the frame.
fn popup(f: &mut Frame, title: &str, lines: Vec<Line>, min_w: usize) {
    let w = lines
        .iter()
        .map(Line::width)
        .max()
        .unwrap_or(0)
        .max(min_w)
        .max(title.width())
        + 4;
    let full = f.area();
    let w = u16::try_from(w).unwrap_or(u16::MAX).min(full.width);
    let h = u16::try_from(lines.len() + 2)
        .unwrap_or(u16::MAX)
        .min(full.height);
    let r = full.centered(Constraint::Length(w), Constraint::Length(h));
    // Greyed-out backdrop and an accent frame: the popup must not blend into the panes.
    let backdrop = Style::new()
        .fg(Color::DarkGray)
        .bg(Color::Reset)
        .remove_modifier(Modifier::all());
    f.buffer_mut().set_style(full, backdrop);
    let block = frame().title(Span::styled(format!(" {title} "), ACCENT));
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(lines).block(block), r);
}

const ACCENT: Style = Style::new()
    .fg(Color::LightCyan)
    .add_modifier(Modifier::BOLD);

/// The accent frame of every popup.
fn frame() -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(ACCENT)
        .padding(Padding::horizontal(1))
}

/// The right-click menu beside the pointer, moved back on screen near an edge. Returns its rows.
fn draw_menu(f: &mut Frame, at: Position, cursor: usize) -> Rect {
    let lines: Vec<Line> = MENU
        .iter()
        .enumerate()
        .map(|(i, (label, key, _))| {
            let line = Line::from(format!("{label:<10}  {key}"));
            match i == cursor {
                true => line.style(Modifier::REVERSED),
                false => line,
            }
        })
        .collect();
    let full = f.area();
    let w = (lines[0].width() as u16 + 4).min(full.width);
    let h = (lines.len() as u16 + 2).min(full.height);
    let x = at.x.saturating_add(1).min(full.right().saturating_sub(w));
    let y = at.y.min(full.bottom().saturating_sub(h));
    let r = Rect::new(x, y, w, h);
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(lines).block(frame()), r);
    r.inner(Margin::new(1, 1))
}

/// The dragged items' name at the pointer, dimmed over whatever is beneath (its background
/// shows through): the closest a terminal gets to a translucent drag image.
fn draw_ghost(f: &mut Frame, app: &App) {
    let Some(d) = &app.drag else { return };
    let (Some(at), Some(e)) = (d.at, d.items.first()) else {
        return;
    };
    let mut text = match app.icons {
        true => format!("{} {}", icon(&e.name, e.is_dir).0, e.name),
        false => e.name.clone(),
    };
    if d.items.len() > 1 {
        text += &format!(" +{}", d.items.len() - 1);
    }
    let area = f.area();
    let x = at.x.saturating_add(1); // beside the pointer, not under it
    if x < area.right() && at.y < area.bottom() {
        let style = Style::new()
            .fg(Color::Gray)
            .add_modifier(Modifier::DIM | Modifier::ITALIC);
        let ghost = Span::styled(text, style);
        f.buffer_mut().set_span(x, at.y, &ghost, area.right() - x);
    }
}

/// `value` hard-wrapped to `width` columns from its start, the cell at char `cursor` reversed.
/// One trailing cell, so a cursor at the end shows.
fn wrap_input(value: &str, cursor: usize, width: usize) -> Vec<Line<'static>> {
    let (mut lines, mut line, mut w) = (vec![], vec![], 0);
    for (i, c) in value.chars().chain([' ']).enumerate() {
        let cw = c.width().unwrap_or(0);
        if w + cw > width && w > 0 {
            lines.push(Line::from(std::mem::take(&mut line)));
            w = 0;
        }
        let style = match i == cursor {
            true => Style::new().add_modifier(Modifier::REVERSED),
            false => Style::new(),
        };
        line.push(Span::styled(c.to_string(), style));
        w += cw;
    }
    lines.push(Line::from(line));
    lines
}

/// `parts` joined with " · ", a new line wherever the next one would pass `width`.
fn pack(parts: &[&str], width: usize) -> Vec<String> {
    let mut lines: Vec<String> = vec![];
    for p in parts {
        match lines.last_mut() {
            Some(l) if l.width() + 3 + p.width() <= width => *l += &format!(" · {p}"),
            _ => lines.push(p.to_string()),
        }
    }
    lines
}

pub fn human_size(n: u64) -> String {
    if n < 1024 {
        return format!("{n}B");
    }
    let (mut v, mut unit) = (n as f64, 'B');
    for u in ['K', 'M', 'G', 'T', 'P', 'E'] {
        if v < 1024.0 {
            break;
        }
        v /= 1024.0;
        unit = u;
    }
    let s = if v < 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.0}")
    };
    format!("{}{unit}", s.strip_suffix(".0").unwrap_or(&s))
}

pub fn fmt_mtime(secs: Option<i64>, now: i64) -> String {
    let at = |s: i64| Local.timestamp_opt(s, 0).single();
    let (Some(t), Some(n)) = (secs.and_then(at), at(now)) else {
        return String::new();
    };
    if t.year() == n.year() {
        t.format("%m-%d %H:%M").to_string()
    } else {
        format!("{:<11}", t.format("%Y-%m-%d").to_string())
    }
}

pub fn fit_left(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.into();
    }
    if width == 0 {
        return String::new();
    }
    let (mut w, mut start) = (1, s.len()); // 1 = the ellipsis
    for (i, c) in s.char_indices().rev() {
        w += c.width().unwrap_or(0);
        if w > width {
            break;
        }
        start = i;
    }
    format!("…{}", &s[start..])
}

pub fn progress_line(t: &Transfers, width: usize) -> String {
    let n = t.running();
    if n == 0 {
        return String::new();
    }
    let (done, total) = t.totals();
    let pct = (done.min(total) as f64 * 100.0 / total.max(1) as f64) as usize;
    let bar = "█".repeat(pct / 10) + &"░".repeat(10 - pct / 10);
    let speed = human_size(t.speed as u64);
    let plus = match t.queued() {
        0 => String::new(),
        q => format!(" +{q}"),
    };
    let (label, files) = match t.jobs.values().find(|j| j.started) {
        Some(j) if n == 1 => (
            j.current.clone(),
            format!(" {}/{}", j.done_files, j.total_files),
        ),
        _ => (format!("{n} jobs"), String::new()),
    };
    // Narrow: drop speed, then file counts, then the queued count; the bar and % stay.
    let tails = [
        format!("{bar} {pct}% {speed}/s{files}{plus}"),
        format!("{bar} {pct}%{files}{plus}"),
        format!("{bar} {pct}%{plus}"),
        format!("{bar} {pct}%"),
    ];
    let tail = tails.iter().find(|s| s.width() + 2 <= width);
    let tail = tail.unwrap_or(&tails[3]);
    let room = width.saturating_sub(tail.width() + 3);
    let fits = if n == 1 {
        room >= 4
    } else {
        room >= label.width()
    };
    let s = if fits {
        format!("⇅ {} {tail}", fit_left(&label, room))
    } else {
        format!("⇅ {tail}")
    };
    let mut w = 0;
    s.chars()
        .take_while(|c| {
            w += c.width().unwrap_or(0);
            w <= width
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{Entry, Fs};
    use crate::model::{ConflictPrompt, Drag, InputKind, JobProgress, Pane};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    const LONG: &str = "/a/very/long/path/that/does/not/fit/in/the/pane/deep";

    fn file(name: &str, size: u64) -> Entry {
        Entry {
            name: name.into(),
            is_dir: false,
            size,
            mtime: None,
            mode: 0o100644,
        }
    }
    fn app(cwd: &str, entries: Vec<Entry>) -> App {
        let mut l = Pane::new(Fs::Local, cwd.into());
        l.set_listing(cwd.into(), entries, None);
        let mut r = Pane::new(Fs::Local, "/".into());
        r.set_listing("/".into(), vec![], None);
        App::new(l, r, true)
    }
    fn render(app: &mut App, w: u16, h: u16) -> Buffer {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        t.backend().buffer().clone()
    }
    /// Row text as displayed: skips the cells hidden behind wide chars.
    fn row(b: &Buffer, y: u16) -> String {
        let (mut s, mut x) = (String::new(), 0);
        while x < b.area.width {
            let sym = b[(x, y)].symbol();
            s += sym;
            x += sym.width().max(1) as u16;
        }
        s
    }
    fn job(current: &str) -> JobProgress {
        JobProgress {
            started: true,
            current: current.into(),
            done_bytes: 50,
            total_bytes: 100,
            done_files: 1,
            total_files: 2,
            last_error: None,
        }
    }

    #[test]
    fn human_size_cases() {
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(1023), "1023B");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(4096), "4K");
        assert_eq!(human_size(12_884_901_888), "12G");
    }

    #[test]
    fn fmt_mtime_same_year_vs_older() {
        let now = Local.with_ymd_and_hms(2026, 6, 15, 12, 0, 0).unwrap();
        let same = Local.with_ymd_and_hms(2026, 3, 4, 5, 6, 0).unwrap();
        let old = Local.with_ymd_and_hms(2024, 3, 4, 5, 6, 0).unwrap();
        let now = now.timestamp();
        assert_eq!(fmt_mtime(Some(same.timestamp()), now), "03-04 05:06");
        assert_eq!(fmt_mtime(Some(old.timestamp()), now), "2024-03-04 ");
        assert_eq!(fmt_mtime(None, now), "");
    }

    #[test]
    fn fit_left_wide_chars() {
        let s = fit_left("/home/한글폴더", 8);
        assert!(s.width() <= 8, "{s}");
        assert!(s.starts_with('…') && s.ends_with("폴더"), "{s}");
        assert_eq!(fit_left("abc", 5), "abc");
    }

    #[test]
    fn progress_line_single_and_multi() {
        let mut t = Transfers::default();
        assert_eq!(progress_line(&t, 80), "");
        t.jobs.insert(1, job("a.mp4"));
        let s = progress_line(&t, 200);
        assert!(
            s.contains("a.mp4") && s.contains("50%") && s.contains("1/2"),
            "{s}"
        );
        assert!(progress_line(&t, 10).width() <= 10);
        t.jobs.insert(2, job("b"));
        t.jobs.insert(3, JobProgress::default());
        let s = progress_line(&t, 200);
        assert!(s.contains("2 jobs") && s.contains("+1"), "{s}");
    }

    #[test]
    fn narrow_progress_keeps_bar_and_percent() {
        let mut t = Transfers::default();
        t.jobs.insert(1, job("train_data_2024.mp4"));
        let s = progress_line(&t, 22);
        assert!(s.width() <= 22 && s.contains('%') && s.contains('█'), "{s}");
        let s = progress_line(&t, 40);
        assert!(
            s.contains("…") && s.ends_with("1/2") && s.contains(".mp4"),
            "{s}"
        );
        t.jobs.get_mut(&1).unwrap().done_bytes = 99;
        let s = progress_line(&t, 80);
        assert!(s.contains("99%") && s.matches('█').count() == 9, "{s}");
    }

    #[test]
    fn narrow_terminal_shows_progress_in_status_line() {
        let mut a = app("/d", vec![]);
        a.transfers.jobs.insert(1, job("a.mp4"));
        let b = render(&mut a, 60, 10);
        let r = row(&b, 9);
        assert!(r.contains('%') && r.contains('█'), "{r}");
    }

    #[test]
    fn offset_written_back() {
        let names: Vec<String> = (0..30).map(|i| format!("f{i:02}")).collect();
        let mut a = app("/d", names.iter().map(|n| file(n, 1)).collect());
        a.panes[0].cursor_to(29);
        render(&mut a, 80, 12);
        assert!(a.panes[0].offset > 0);
    }

    #[test]
    fn long_title_keeps_tail() {
        let b = render(&mut app(LONG, vec![]), 60, 10);
        let r = row(&b, 1);
        assert!(r.contains('…') && r.contains("deep"), "{r}");
    }

    #[test]
    fn title_suffix_survives_long_path() {
        let mut a = app(LONG, vec![]);
        a.panes[0].loading = Some(("/x".into(), None));
        let r = row(&render(&mut a, 60, 10), 1);
        assert!(r.contains("deep") && r.contains("(loading)"), "{r}");
    }

    #[test]
    fn focused_border_is_lightcyan_bold() {
        let mut a = app("/d", vec![file("a", 1)]);
        let b = render(&mut a, 80, 12);
        let c = &b[(0, 1)];
        assert_eq!(c.fg, Color::LightCyan);
        assert!(c.modifier.contains(Modifier::BOLD));
        let r = a.hit.panes[1];
        assert_eq!(b[(r.x - 1, r.y - 1)].fg, Color::DarkGray);
    }

    #[test]
    fn filter_bar_sits_on_top_of_the_pane() {
        let mut a = app("/d", vec![file("apple", 1), file("banana", 1)]);
        a.panes[0].set_filter("an");
        let b = render(&mut a, 80, 12);
        assert!(row(&b, 2).contains("Filter: an"), "{}", row(&b, 2));
        assert!(row(&b, 3).contains(".."), "{}", row(&b, 3));
        assert!(row(&b, 4).contains("banana") && !row(&b, 5).contains("apple"));
        assert_eq!(a.hit.panes[0].y, 3); // clicks map to rows below the bar
        let value = "an".to_string();
        a.popup = Some(Popup::Input {
            kind: InputKind::Filter,
            title: "Filter".into(),
            value,
            cursor: 1,
        });
        let b = render(&mut a, 80, 12);
        let r = row(&b, 2);
        assert!(r.contains("Filter: an") && r.contains("Esc clear"), "{r}");
        let x = r[..r.find("an").unwrap()].chars().count() as u16 + 1; // the cursor on the n
        assert!(b[(x, 2)].modifier.contains(Modifier::REVERSED));
        assert!(!row(&b, 5).contains("┏"), "no centered popup");
        let value = "abcdefghijklmnopqrstuvwxyz0123456789".to_string();
        let cursor = value.chars().count();
        a.popup = Some(Popup::Input {
            kind: InputKind::Filter,
            title: "Filter".into(),
            value,
            cursor,
        });
        let r = row(&render(&mut a, 80, 12), 2);
        assert!(r.contains("Filter: …") && r.contains("789"), "{r}"); // the end stays in view
    }

    #[test]
    fn pending_count_beats_the_message() {
        let mut a = app("/d", vec![]);
        a.status = "Copied 1 item(s)".into();
        a.count = 15;
        let b = render(&mut a, 80, 12);
        assert!(row(&b, 11).trim_end().ends_with("15"), "{}", row(&b, 11));
    }

    #[test]
    fn delete_popup_names_the_item_and_warns_for_folders() {
        let mut a = app("/d", vec![]);
        let dir = Entry {
            is_dir: true,
            ..file("stuff", 0)
        };
        let dir_name = "/d".to_string();
        a.popup = Some(Popup::ConfirmDelete {
            dir: dir_name.clone(),
            items: vec![file("a.txt", 1)],
        });
        let b = render(&mut a, 80, 12);
        let ((_, y), _) = popup_top(&b, "Delete");
        assert!(row(&b, y + 1).contains("Delete \"a.txt\"? [y/N]"));
        assert!(!row(&b, y + 2).contains("Folders"));
        a.popup = Some(Popup::ConfirmDelete {
            dir: dir_name,
            items: vec![file("a.txt", 1), dir],
        });
        let b = render(&mut a, 80, 12);
        let ((_, y), _) = popup_top(&b, "Delete");
        assert!(row(&b, y + 1).contains("Delete 2 items? [y/N]"));
        assert!(row(&b, y + 2).contains("Folders go with everything in them."));
        a.popup = Some(Popup::ConfirmDelete {
            dir: "/d".into(),
            items: vec![file(&"x".repeat(200), 1)],
        });
        let b = render(&mut a, 80, 12);
        let ((_, y), _) = popup_top(&b, "Delete");
        let r = row(&b, y + 1);
        assert!(
            r.contains("Delete \"…x") && r.contains("x\"? [y/N] ┃"),
            "{r}"
        );
    }

    /// The popup's top-left corner and width, found by its title.
    fn popup_top(b: &Buffer, title: &str) -> ((u16, u16), usize) {
        let head = format!("┏ {title} ");
        let y = (0..b.area.height)
            .find(|&y| row(b, y).contains(&head))
            .unwrap();
        let r: Vec<char> = row(b, y).chars().collect();
        let l = r.iter().rposition(|&c| c == '┏').unwrap();
        let rr = r.iter().rposition(|&c| c == '┓').unwrap();
        ((l as u16, y), rr - l + 1)
    }

    #[test]
    fn input_popup_is_a_third_wide_and_reads_from_the_start() {
        let mut a = app("/d", vec![]);
        let value = format!("{LONG}/report.txt");
        let n = value.chars().count();
        let mut b = Buffer::empty(Rect::default());
        for cursor in [n, 0, n / 2] {
            a.popup = Some(Popup::Input {
                kind: InputKind::NewDir { dir: "/".into() },
                title: "Copy to".into(),
                value: value.clone(),
                cursor,
            });
            b = render(&mut a, 90, 24);
            assert_eq!(popup_top(&b, "Copy to").1, 30, "cursor {cursor}");
        }
        let text: String = (0..24).map(|y| row(&b, y)).collect();
        assert!(text.contains("┃ /a/very/long"), "{text}");
        assert!(text.contains("report.txt"), "{text}");
        assert!(!text.contains('…'), "{text}");
    }

    #[test]
    fn popup_greys_out_the_panes() {
        let mut a = app("/d", vec![file("a", 1)]);
        a.popup = Some(Popup::Help);
        let b = render(&mut a, 80, 24);
        assert_eq!(b[(0, 1)].fg, Color::DarkGray, "focused border dimmed");
        let (corner, _) = popup_top(&b, "Help");
        assert_eq!(b[corner].fg, Color::LightCyan);
    }

    #[test]
    fn ghost_follows_pointer() {
        let mut a = app("/d", vec![]);
        a.icons = false;
        a.drag = Some(Drag {
            from: Side::Left,
            items: vec![file("x.txt", 5), file("y.txt", 5)],
            src_cwd: "/d".into(),
            at: Some(ratatui::layout::Position::new(50, 5)),
        });
        let b = render(&mut a, 80, 12);
        let at: String = row(&b, 5).chars().skip(51).collect();
        assert!(at.starts_with("x.txt +1"), "{}", row(&b, 5));
        assert!(b[(51, 5)].modifier.contains(Modifier::DIM));
        assert!(row(&b, 11).contains("Drop to copy 2 item(s)"));
    }

    #[test]
    fn hit_cache_filled() {
        let mut a = app("/d", vec![]);
        let mut t = Terminal::new(TestBackend::new(80, 12)).unwrap();
        t.draw(|f| draw(f, &mut a)).unwrap();
        t.draw(|f| draw(f, &mut a)).unwrap();
        assert_eq!(a.hit.toolbar.len(), 6);
        assert!(a.hit.panes[1].x > a.hit.panes[0].x);
    }

    #[test]
    fn menu_stays_on_screen_near_the_edge() {
        let mut a = app("/d", vec![]);
        a.popup = Some(Popup::Menu {
            at: ratatui::layout::Position::new(79, 11),
            cursor: 0,
        });
        let b = render(&mut a, 80, 12);
        let m = a.hit.menu;
        assert_eq!((m.right() + 1, m.bottom() + 1), (80, 12)); // pushed back in, frame included
        assert_eq!(m.height as usize, MENU.len().min(10));
        assert!(row(&b, m.y).contains("Copy"), "{}", row(&b, m.y));
    }

    #[test]
    fn portrait_screen_stacks_the_panes() {
        let mut a = app("/d", vec![file("a.txt", 1)]);
        render(&mut a, 40, 21); // 21 rows look taller than 40 columns
        let [top, bottom] = a.hit.boxes;
        assert_eq!((top.x, top.width), (bottom.x, 40));
        assert_eq!(bottom.y, top.bottom());
        render(&mut a, 40, 20); // square: side by side again
        assert!(a.hit.boxes[1].x > a.hit.boxes[0].x);
    }

    #[test]
    fn wide_names_keep_columns_aligned() {
        let long = "한글이름이아주긴파일이름.txt";
        let mut a = app("/d", vec![file(long, 2048), file("a.txt", 2048)]);
        let b = render(&mut a, 60, 10);
        let pane = a.hit.panes[0];
        let end_of_2k = |y: u16| {
            (pane.x + 1..pane.right())
                .find(|&x| b[(x - 1, y)].symbol() == "2" && b[(x, y)].symbol() == "K")
        };
        let ys = |pat: &str| {
            (0..b.area.height)
                .find(|&y| row(&b, y).contains(pat))
                .unwrap()
        };
        let (yl, ys_) = (ys("한글"), ys("a.txt"));
        assert!(!row(&b, yl).contains("txt"), "{}", row(&b, yl));
        assert!(end_of_2k(yl).is_some());
        assert_eq!(end_of_2k(yl), end_of_2k(ys_));
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        for (w, h) in [(20, 5), (1, 1)] {
            for state in 0..6 {
                let mut a = app(LONG, vec![file("x.txt", 5), file("한글.txt", 9)]);
                match state {
                    0 => {}
                    1 => {
                        a.transfers.jobs.insert(1, job("a.mp4"));
                    }
                    2 => a.popup = Some(Popup::Help),
                    3 => {
                        a.popup = Some(Popup::Input {
                            kind: InputKind::NewDir { dir: "/".into() },
                            title: "New dir".into(),
                            value: "v".repeat(50),
                            cursor: 50,
                        })
                    }
                    4 => {
                        let (reply, _rx) = tokio::sync::oneshot::channel();
                        let path = LONG.repeat(4);
                        a.conflicts.push_back(ConflictPrompt {
                            job: 1,
                            path,
                            reply,
                        });
                    }
                    _ => {
                        let items = vec![file("x.txt", 5); 3];
                        a.drag = Some(Drag {
                            from: Side::Left,
                            items,
                            src_cwd: "/d".into(),
                            at: Some(ratatui::layout::Position::new(w - 1, h - 1)),
                        });
                    }
                }
                render(&mut a, w, h);
            }
        }
    }
}
