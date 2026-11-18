use crate::icons::icon;
use crate::model::{App, InputKind, Popup, Side, TOOLBAR, Transfers};
use chrono::{Datelike, Local, TimeZone};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, Padding, Paragraph, Row, Table, TableState,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The Key table from Global Constraints: (key, help text).
const HELP: [(&str, &str); 17] = [
    ("↑ / ↓", "Move cursor"),
    ("Shift+↑ / Shift+↓", "Select range"),
    ("PgUp / PgDn / Home / End", "Page / first / last"),
    ("←", "Parent dir"),
    ("→ / Enter", "Open dir"),
    ("Tab / BackTab", "Switch pane"),
    ("Esc", "Clear selection"),
    ("c", "Copy to other pane"),
    ("C", "Copy to… (rename / path)"),
    ("m", "Move to…"),
    ("n", "New dir"),
    ("N", "New file"),
    ("g", "Change dir"),
    ("?", "Help"),
    ("Ctrl+R", "Refresh"),
    ("q", "Quit"),
    ("Ctrl+C", "Quit"),
];

pub fn draw(f: &mut Frame, app: &mut App) {
    app.hit.toolbar.clear();
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

    let halves: [Rect; 2] = Layout::horizontal([Constraint::Fill(1); 2]).areas(mid);
    for side in [Side::Left, Side::Right] {
        app.hit.panes[side.idx()] = draw_pane(f, app, side, halves[side.idx()]);
    }

    let right = match &app.drag {
        Some(d) if d.active => format!("Drop to copy {} item(s)", d.items.len()),
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

    let full = f.area().width as usize;
    match &app.popup {
        Some(Popup::Input {
            kind,
            title,
            value,
            cursor,
        }) => {
            let b = value
                .char_indices()
                .nth(*cursor)
                .map_or(value.len(), |(i, _)| i);
            let (before, rest) = value.split_at(b);
            let mut chars = rest.chars();
            let at = chars.next().map_or(" ".into(), String::from);
            let avail = full.min(64).saturating_sub(4 + at.width());
            let value = Line::from(vec![
                Span::raw(fit_left(before, avail)),
                Span::styled(at, Modifier::REVERSED),
                Span::raw(chars.as_str()),
            ]);
            let hint = match kind {
                InputKind::CopyTo { .. } => {
                    "Enter confirm · Esc cancel · existing dir = put inside"
                }
                _ => "Enter confirm · Esc cancel",
            };
            let hint = Line::styled(hint, Color::DarkGray);
            popup(f, title, vec![value, hint], 60);
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
    f.render_stateful_widget(table, inner, &mut state);
    p.offset = state.offset();
    inner
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
    let block = Block::bordered()
        .title(format!(" {title} "))
        .padding(Padding::horizontal(1));
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(lines).block(block), r);
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
    fn hit_cache_filled() {
        let mut a = app("/d", vec![]);
        let mut t = Terminal::new(TestBackend::new(80, 12)).unwrap();
        t.draw(|f| draw(f, &mut a)).unwrap();
        t.draw(|f| draw(f, &mut a)).unwrap();
        assert_eq!(a.hit.toolbar.len(), 6);
        assert!(a.hit.panes[1].x > a.hit.panes[0].x);
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
                            active: true,
                        });
                    }
                }
                render(&mut a, w, h);
            }
        }
    }
}
