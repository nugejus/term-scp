# File Preview Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enter / double-click on a file opens a preview popup (text, image, audio, video) closed by Esc or a ✕ click, with Space play/stop through ffplay.

**Architecture:** A new UI-free `src/preview.rs` classifies a file on disk (image → text → broken image → media via ffprobe → message) and owns the ffplay child. The controller opens `Popup::Preview`, downloads remote files to a temp file in a tokio task, runs classification in `spawn_blocking`, and receives the result as an `AppEvent`. The view draws it with ratatui-image's `StatefulImage`.

**Tech Stack:** Rust 2024, ratatui 0.30.2, ratatui-image 11.1.0, image 0.25, tempfile 3, tokio, ffprobe/ffmpeg/ffplay (optional, on PATH).

**Spec:** `docs/superpowers/specs/2026-11-22-file-preview-design.md`

## Global Constraints

- `ratatui-image = { version = "11.1.0", default-features = false, features = ["crossterm", "image-defaults"] }`, `image = "0.25"`; `tempfile = "3.27"` moves from `[dev-dependencies]` to `[dependencies]`. No other new crates.
- `LIMIT = 50 * 1024 * 1024` bytes. Remote files and local text/images over it are not read; local audio/video has no limit.
- Exact UI copy (English): `Too big to preview (over 50 MB).` · `No preview for this file type.` · `Audio/video preview needs ffmpeg.` · `Can't read: {err}` · `Can't decode image: {err}` · `Loading…` · `Install ffmpeg to play` · Help line `("Enter", "Open dir / preview file")`.
- ffmpeg tools are optional: a missing binary is never an error dialog or a panic.
- A child process never touches the terminal: stdin, stdout, stderr are `Stdio::null()` (ffmpeg's stdout is piped where its output is read).
- Everything a preview owns (task, temp file, player) goes when the popup is dropped.
- Do not commit. The user commits on request. Leave work in the tree.
- `cargo test`, `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` pass at the end of every task.
- Tests that need ffmpeg/ffprobe return early when `Command::new("ffprobe").arg("-version")` can't spawn. Tests never make a sound and never open a window.

## Review Focus

1. A remote preview closed mid-download: the download task stops and no `term-scp-preview-*` file is left in the temp dir. Pinned in Task 3.
2. Keys typed while a preview is open must not leak to the panes (e.g. `d`, `c`, `q`, digits): the popup swallows them. Pinned in Task 3.
3. A file whose extension lies (a `.png` that is text, a `.txt` that is PNG bytes): classification goes by content, not by name. Pinned in Task 1 (`classify_bytes` never sees the name).
4. A tiny terminal (e.g. 20×6): drawing a preview must not panic, and ✕ must still be inside the frame. Pinned in Task 4.
5. A late result for a preview that was closed and replaced: it must not fill the new popup. Pinned in Task 3.

---

### Task 1: Dependencies and the pure classification core

**Files:**
- Modify: `Cargo.toml`
- Create: `src/preview.rs`
- Modify: `src/main.rs` (add `#[allow(dead_code)] // until Task 3 uses it` + `mod preview;` only)

**Interfaces:**
- Produces (all `pub` in `src/preview.rs`):
  - `pub const LIMIT: u64 = 50 * 1024 * 1024;`
  - `pub const TOO_BIG: &str`, `NO_PREVIEW: &str`, `NEEDS_FFMPEG: &str` with the Global Constraints copy.
  - `#[derive(Debug, Default, PartialEq)] pub struct Probe { pub format: String, pub duration: Option<f64>, pub video: Option<(String, u32, u32)>, pub audio: Option<(String, u32, u32)>, pub cover: bool }`. `video` is the first stream with `codec_type=video` and `disposition:attached_pic=0` (codec, width, height). `audio` is the first `codec_type=audio` stream (codec, sample_rate, channels). `cover` is true if any stream has `attached_pic=1`.
  - `impl Probe { pub fn info(&self) -> String }`: the parts that exist, joined by ` · `: `"{codec} {w}×{h}"`, `"{codec} {rate} Hz {ch} ch"`, then the duration as `m:ss` (or `h:mm:ss` from one hour).
  - `pub fn fmt_secs(s: f64) -> String`: `0:42`, `12:05`, `1:02:03` (floor).
  - `pub fn parse_probe(out: &str) -> Option<Probe>`: parses ffprobe `-of compact` output (one `section|key=value|…` line per stream and one `format|…` line). `None` when there is neither `video` nor `audio`, or the format is `image2` or ends with `_pipe`.
  - `pub enum Kind { Text(Vec<String>), Image(String), Media(Probe), Message(String) }`. `Image(info)` carries `"{w}×{h} · {FORMAT}"` (`format!("{fmt:?}").to_uppercase()`, e.g. `PNG`).
  - `pub struct Loaded { pub kind: Kind, pub image: Option<image::DynamicImage> }`. `image` is the picture for `Kind::Image`, or the thumbnail for `Kind::Media`, already shrunk to fit 1600×1600 (never enlarged).
  - `pub fn classify_bytes(bytes: &[u8]) -> Option<Loaded>`: steps 1–3 of the spec's classification. `None` means "go on to the media check".
  - `pub fn text_lines(bytes: &[u8]) -> Vec<String>`: `from_utf8_lossy`, split on `\n`, strip one trailing `\r` per line, tab → 4 spaces, other `char::is_control` dropped; a trailing newline does not add an empty last line.

- [ ] **Step 1: Cargo.toml.** Apply the dependency lines from Global Constraints. Run `cargo build`. Expected: builds (pulls ratatui-image without pkg-config).
- [ ] **Step 2: Write the failing tests** in `src/preview.rs` `mod tests`:

```rust
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(w, h).write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

#[test]
fn classify_by_content() {
    let l = classify_bytes(&png(40, 30)).unwrap();
    assert!(matches!(&l.kind, Kind::Image(i) if i == "40×30 · PNG"));
    assert_eq!(l.image.as_ref().map(|i| (i.width(), i.height())), Some((40, 30)));

    let big = classify_bytes(&png(3200, 100)).unwrap();
    assert_eq!(big.image.map(|i| (i.width(), i.height())), Some((1600, 50)));

    assert!(matches!(classify_bytes(b"hello\r\nworld\n").unwrap().kind,
        Kind::Text(ref v) if v == &["hello", "world"]));
    assert!(matches!(classify_bytes(b"P1 is a text line").unwrap().kind, Kind::Text(_)));
    let half = png(40, 30);
    assert!(matches!(classify_bytes(&half[..half.len() / 2]).unwrap().kind,
        Kind::Message(ref m) if m.starts_with("Can't decode image: ")));
    assert!(classify_bytes(b"\0\x01\x02binary").is_none());
}

#[test]
fn text_lines_cleanup() {
    assert_eq!(text_lines(b"a\tb\x07c\n\nd"), ["a    bc", "", "d"]);
}

#[test]
fn probe_parsing() {
    let mp3 = "stream|codec_name=mp3|codec_type=audio|sample_rate=44100|channels=2|disposition:attached_pic=0\n\
               stream|codec_name=png|codec_type=video|width=64|height=48|disposition:attached_pic=1\n\
               format|format_name=mp3|duration=125.500000\n";
    let p = parse_probe(mp3).unwrap();
    assert_eq!((p.video, p.cover), (None, true));
    assert_eq!(p.audio, Some(("mp3".into(), 44100, 2)));
    assert_eq!(p.info(), "mp3 44100 Hz 2 ch · 2:05");
    let mp4 = "stream|codec_name=h264|codec_type=video|width=320|height=240|disposition:attached_pic=0\n\
               format|format_name=mov,mp4,m4a,3gp,3g2,mj2|duration=1.000000\n";
    assert_eq!(parse_probe(mp4).unwrap().info(), "h264 320×240 · 0:01");
    let png = "stream|codec_name=png|codec_type=video|width=64|height=48|disposition:attached_pic=0\n\
               format|format_name=png_pipe|duration=N/A\n";
    assert_eq!(parse_probe(png), None);
    assert_eq!(parse_probe(""), None);
    assert_eq!(fmt_secs(3723.9), "1:02:03");
}
```

- [ ] **Step 3:** `cargo test preview::` → FAIL (items not defined).
- [ ] **Step 4: Implement** the Interfaces above. Image recognition is `image::guess_format`; decode with `image::load_from_memory_with_format`. Shrink with `DynamicImage::thumbnail(1600, 1600)` only when a side exceeds 1600. Text check: no `0` byte in the first 8 KiB.
- [ ] **Step 5:** `cargo test preview::` → PASS; clippy and fmt clean.

### Task 2: Running the ffmpeg tools and loading a file

**Files:**
- Modify: `src/preview.rs`

**Interfaces:**
- Consumes: everything from Task 1.
- Produces:
  - `pub fn probe(path: &Path) -> std::io::Result<Option<Probe>>`: runs `ffprobe -v error -show_entries format=format_name,duration:stream=codec_type,codec_name,width,height,sample_rate,channels:stream_disposition=attached_pic -of compact <path>`. `Err` only when ffprobe can't be spawned; a non-zero exit is `Ok(None)`.
  - `pub fn thumbnail(path: &Path, p: &Probe) -> Option<DynamicImage>`: real video → `ffmpeg -v error -ss <10% of duration, 0 if unknown> -i <path> -frames:v 1 -f image2pipe -c:v png -`; cover only → same without `-ss` and with `-an`; neither → `None`. Decoded with `image::load_from_memory`, shrunk like Task 1.
  - `pub fn load(path: &Path, read: bool) -> Loaded`: blocking. `read` = the bytes may be read (size ≤ LIMIT). Order: if `read`: `std::fs::read` (error → `Message("Can't read: {e}")`), `classify_bytes`. Then `probe`: `Ok(Some(p))` → `Media(p)` + `thumbnail`. Otherwise: `read == false` → `Message(TOO_BIG)`; ffprobe missing → `Message("{NO_PREVIEW} {NEEDS_FFMPEG}")`; else `Message(NO_PREVIEW)`.
  - `pub struct Player { child: std::process::Child, pub started: std::time::Instant }`, `impl Drop for Player` → `kill()` then `wait()`, ignoring errors.
  - `impl Player { pub fn spawn(path: &Path, p: &Probe, title: &str) -> std::io::Result<Player>; pub fn finished(&mut self) -> bool }`: `ffplay -autoexit -loglevel quiet -window_title <title> [-nodisp when p.video is None] <path>`, all three stdio null. `finished` = `try_wait` returned `Some` (or an error).
  - Ruling (plan vs spec): the spec names `tokio::process::Command` + `kill_on_drop`; a `std::process::Child` with this `Drop` gives the same guarantee, needs no runtime context to spawn, and reaps the child synchronously. Same behavior, fewer moving parts.

- [ ] **Step 1: Write the failing tests** (each returns early without ffprobe; build fixtures with `ffmpeg -v error -y -f lavfi -i …` into a `tempfile::tempdir()`):
  - `load_media`: `sine=frequency=440:duration=1` → `a.wav` → `load(&wav, true)` gives `Kind::Media(p)` with `p.audio.is_some()`, `p.video.is_none()`, `image.is_none()`. `testsrc=duration=1:size=320x240:rate=10` with `-pix_fmt yuv420p` → `v.mp4` → `Media` with `p.video == Some(("h264".into(), 320, 240))` and a thumbnail of 320×240.
  - `load_limits`: `a.wav` extended with `File::set_len(51 * 1024 * 1024)` → `load(&p, false)` is `Media`. A 51 MB `set_len` file of zeros → `load(&p, false)` is `Message(TOO_BIG)`. A PNG file → `load(&p, true)` is `Image`. A missing path → `Message` starting `Can't read: `.
  - `player_stops_on_drop`: spawn on a **silent** 5 s WAV (`anullsrc=r=44100:cl=mono` with `-t 5`, so `cargo test` never makes a sound; skip without ffplay), keep its pid (`Player` may expose `pub fn id(&self) -> u32` for this), drop it, and assert `kill -0 <pid>` fails afterwards.
- [ ] **Step 2:** `cargo test preview::` → FAIL.
- [ ] **Step 3: Implement** the Interfaces.
- [ ] **Step 4:** `cargo test preview::` → PASS; clippy, fmt clean.

### Task 3: The preview popup state and controller flow

**Files:**
- Modify: `src/model.rs`, `src/controller.rs`, `src/view.rs` (only the two `hit` resets), `src/main.rs` (only: remove Task 1's `#[allow(dead_code)]`)

**Interfaces:**
- Consumes: `preview::{LIMIT, TOO_BIG, Kind, Loaded, Player, load}`.
- Produces in `src/model.rs`:
  - `pub struct Preview { pub name: String, pub size: u64, pub seq: u64, pub kind: Option<Kind> /* None while loading */, pub got: Option<(u64, u64)> /* remote download: (done, total) */, pub image: Option<ratatui_image::protocol::StatefulProtocol>, pub scroll: usize, pub path: Option<PathBuf> /* the bytes on disk, for ffplay */, pub temp: Option<tempfile::NamedTempFile>, pub player: Option<Player>, pub no_player: bool /* ffplay failed to spawn */, pub task: Option<tokio::task::AbortHandle> }`
  - `impl Drop for Preview` → aborts `task`.
  - `Popup::Preview(Box<Preview>)`.
  - `App` gains `pub picker: ratatui_image::picker::Picker` (initialised to `Picker::halfblocks()` in `App::new`, so no call site changes) and `pub clear: bool` (main does a full redraw when set).
  - `Hit` gains `pub close: Rect` (the ✕ cells) and `pub preview: Rect` (the body; its height is the page size). Both reset at the start of every `draw` like `hit.menu` (the reset lines go in `view::draw` in this task; drawing comes in Task 4).
- Produces in `src/controller.rs`:
  - `AppEvent::PreviewProgress { seq: u64, done: u64 }` and `AppEvent::Previewed { seq: u64, loaded: Loaded, path: PathBuf, temp: Option<NamedTempFile> }`.
  - `Controller` gains `next_preview: u64`.
  - `fn preview(&mut self, app: &mut App, side: Side, e: Entry)`: opens the popup (Loading) and spawns the task; stores `abort_handle()`. Local: `path = join(cwd, name)`, `spawn_blocking(load(path, e.size <= LIMIT))`. Remote over LIMIT: the popup gets `kind = Some(Kind::Message(TOO_BIG.into()))` at once, no task. Remote otherwise: `got = Some((0, e.size))`; `fs.open_read` → `tempfile::Builder::new().prefix("term-scp-preview-").suffix(<".ext" or "">).tempfile()`, copy in 256 KiB chunks sending `PreviewProgress` after each, then `spawn_blocking(load(temp_path, true))`. A read error → `Previewed` with `Kind::Message("Can't read: {e}")`.
  - `Command::Enter` on a non-dir, non-`..` entry calls `preview`.
  - `on_key` with `Popup::Preview(p)`: `Esc` → drop it, `app.clear = true`. `' '` on `Kind::Media` → toggle: `player.take()` if some, else `Player::spawn(path, probe, &name)`; `Err` → `no_player = true`. `Up/Down` ±1, `PageUp/PageDown` ± `app.hit.preview.height.max(1)`, `Home` → 0, `End` → `usize::MAX` (the view clamps). Every other key: nothing; the popup is put back.
  - `on_mouse` with a preview open: left `Down` inside `hit.close` → close as Esc; `ScrollUp/ScrollDown` inside `hit.preview` → ∓/± 3 lines; everything else ignored.
  - `handle(Previewed)` / `handle(PreviewProgress)`: only when the open popup is a preview with the same `seq`; otherwise the event is dropped (and with it the temp file). `Previewed` sets `kind`, `path`, `temp`, `got = None`, `image = loaded.image.map(|i| app.picker.new_resize_protocol(i))`.
  - `tick`: a preview's `player` that `finished()` → `None`.
- [ ] **Step 1: Write the failing tests** in `controller.rs` `mod tests` (use `setup`, `pump`, `press`; write file contents with `std::fs::write` into `t.dirs[0]`, then `ctl.reload` + `pump` so the pane lists them):
  - `enter_on_a_file_previews_it`: `notes.txt` = `"one\ntwo\n"`; Enter → popup is `Preview` with `kind == None`; pump until `kind.is_some()`; `Kind::Text` == `["one", "two"]`. Enter on a dir still lists the dir.
  - `preview_swallows_keys_and_esc_closes`: with a text preview open, press `d`, `c`, `q`, `5`, `Down` → popup still `Preview`, `app.quit == false`, no `ConfirmDelete`, `app.count == 0`, `scroll == 1`; Esc → `popup.is_none()` and `app.clear`.
  - `click_on_close_and_double_click`: with a preview open, set `t.app.hit.close = Rect::new(10, 2, 3, 1)` by hand (don't `draw` afterwards: `draw` resets it), left click at (11, 2) → closed. A double-click (two left Downs within 400 ms on a file row; reuse the existing `double_click_enters_dir` pattern) opens a `Preview`.
  - `late_result_is_dropped`: open a preview, Esc, open another; feed the first preview's `Previewed` by sending a hand-built `AppEvent::Previewed { seq: <first seq>, … }` through `ctl.handle` → the open preview still has its own `seq` and its kind is unchanged by that event.
  - `remote_preview_uses_and_removes_a_temp_file`: make the right pane remote: `t.app.panes[1] = Pane::new(t._rt.block_on(crate::fs::test_remote()), t.dirs[1].clone())` (`test_remote` runs the local `sftp-server`, so remote paths are the tempdir's own paths), then `reload` + `pump`. With `pic.png` (Task 1's `png(40, 30)` bytes; copy the helper) there: focus Right, Enter, pump until loaded → `temp` is `Some`, its file name starts with `term-scp-preview-` and ends with `.png`, the file exists; Esc → it no longer exists. Second case: a 4 MB file `big.t3abort`; Enter, then Esc right away; then for 500 ms keep feeding whatever arrives on `t.rx` to `ctl.handle` (a late `Previewed` holds the temp file until it is handled); afterwards no file in `std::env::temp_dir()` starts with `term-scp-preview-` and ends with `.t3abort`.
  - `remote_over_limit_is_not_downloaded`: an `Entry` with `size = LIMIT + 1` on a remote pane (set the listing by hand with `set_listing`) → after Enter, `kind == Some(Message(TOO_BIG))` immediately and `task.is_none()`.
- [ ] **Step 2:** `cargo test controller::` → the new tests FAIL.
- [ ] **Step 3: Implement** the Interfaces.
- [ ] **Step 4:** `cargo test` (all) → PASS; clippy, fmt clean.

### Task 4: Drawing the preview

**Files:**
- Modify: `src/view.rs`

**Interfaces:**
- Consumes: `Popup::Preview`, `Preview`, `Hit.close`, `Hit.preview`, `preview::{Kind, fmt_secs}`, `human_size`.
- Produces: `fn draw_preview(f: &mut Frame, p: &mut Preview) -> (Rect /* close */, Rect /* body */)`, called from `draw`'s popup match; `draw` stores both into `app.hit`.
- Layout (values to use verbatim):
  - Outer rect: `full.centered(Constraint::Percentage(80), Constraint::Percentage(80))`, never smaller than what fits (`.intersection(full)`), same greyed backdrop and `frame()` as `popup()`.
  - Titles: left `" {name} "` in `ACCENT`; right `Line::from(" ✕ ").right_aligned()` in `ACCENT`. The ✕ hit rect is the 3 cells of that right title on the top border row.
  - Inside the border: the last row is the footer (`Color::DarkGray`); the rest is the body.
  - Loading: `Loading…`, or `Loading… {human_size(done)} / {human_size(total)}` when `got` is set, centered in the body.
  - Text: clamp `p.scroll` to `len.saturating_sub(body.height)` and write it back; lines `[scroll .. scroll + body.height]`, no wrap. Footer: `lines {a}–{b} of {n} · ↑↓ PgUp PgDn scroll · Esc close` (`a` 1-based; an empty file shows `empty · Esc close`).
  - Image: `StatefulImage::default().resize(Resize::Fit(Some(FilterType::Triangle)))`, centered in the body using `proto.size_for(resize.clone(), body.as_size())`. Footer: `{info} · {human_size(size)} · Esc close`.
  - Media: the thumbnail (if any) fills the body minus its last row, centered the same way; the last body row is `probe.info()`. Footer: `Space play · Esc close`; while playing `▶ {fmt_secs(elapsed)} / {fmt_secs(duration)} · Space stop · Esc close` (just `▶ {elapsed}` without a duration); `no_player` → `Install ffmpeg to play · Esc close`.
  - Message: the message centered in the body, `human_size(size)` on the next row. Footer: `Esc close`.
  - Help: the `Enter` row becomes `("Enter", "Open dir / preview file")`.
- [ ] **Step 1: Write the failing tests** in `view.rs` `mod tests` (build a `Preview` by hand; image state via `Picker::halfblocks().new_resize_protocol(...)`):
  - `preview_close_mark_is_top_right`: 80×24, text preview → the row at the popup's top contains `✕`, `app.hit.close` has width 3, its middle cell holds `✕`, and `hit.close.right() == outer.right() - 1` (just left of `┓`).
  - `text_preview_scrolls_and_clamps`: 100 lines `"line {i}"`, 80×24 → first body row shows `line 0`; set `scroll = 10` → shows `line 10`; `scroll = usize::MAX` → after draw `scroll == 100 - body.height` and the footer says `of 100`.
  - `image_and_media_previews_draw`: a halfblocks image preview and a media preview with a thumbnail and `Probe` info draw without panic, the media row shows `probe.info()`, and the footer shows `Space play`.
  - `tiny_terminal_preview`: 20×6 for each kind → no panic; `hit.close` is inside the frame.
  - `help_mentions_preview`: the Help popup shows `Open dir / preview file`.
- [ ] **Step 2:** `cargo test view::` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4:** `cargo test` → PASS; clippy, fmt clean.

### Task 5: Terminal wiring, README, real-terminal check

**Files:**
- Modify: `src/main.rs`, `README.md`

**Interfaces:**
- Consumes: `App.picker`, `App.clear`.
- [ ] **Step 1: main.rs.** Right after `let mut terminal = ratatui::init();` and before `EnableMouseCapture` and the input thread: `let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());`; after `App::new(...)`: `app.picker = picker;`. In the loop, before `terminal.draw`: `if std::mem::take(&mut app.clear) { let _ = terminal.clear(); }`.
- [ ] **Step 2: README.md.** A short "Preview" section beside the existing key docs: Enter / double-click on a file; Esc or ✕ closes; what each kind shows; Space plays audio/video with ffplay (video in its own window); the 50 MB rule (remote files, local text/images); ffmpeg optional. Update any README key table that lists Enter.
- [ ] **Step 3: pty check** (the existing approach: drain, don't sleep; check results on disk/state, not by regex over redraw diffs): run the binary in a pty against `tests/fake-ssh`, Enter on a text file, then Esc; the process stays up and responds (e.g. `q` quits with exit 0).
- [ ] **Step 4: Real terminal.** Open the app in a new Ghostty window on a scratch dir holding a PNG, a text file, a WAV and an MP4; screenshot each preview (`screencapture`) and look at it: the image is a real picture (not half blocks), it is gone after Esc, Space on the WAV plays and Space again stops (`pgrep ffplay` appears/disappears), the MP4 opens an ffplay window, closing the popup kills it.
- [ ] **Step 5:** `cargo test`, clippy, fmt → clean.
