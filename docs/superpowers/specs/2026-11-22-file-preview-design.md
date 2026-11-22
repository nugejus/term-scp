# File preview — design

Date: 2026-11-22 · Status: draft

## Intent

Enter or a double-click on a file opens a popup that shows what the file is,
so the user can tell before copying it. Audio and video also get a simple
play/stop.

Why (user's words): "복사 전에 무슨 파일인지만 확인하면 돼. 간단한 재생 기능도
넣어주면 좋을 것 같아" — identification first, playback as a bonus.

## Goals

- Enter on a file, or a double-click on one, opens the preview. Esc, or a
  click on the ✕ in the popup's top-right corner, closes it.
- Kinds: text-like files, images, audio, video. The 50 MB limit covers what
  has to be downloaded or loaded into memory: remote files, and local text
  and images. Local audio/video has no size limit (user's call: ffprobe and
  ffplay read it straight from disk).
- Works in both panes, local or remote (remote files are downloaded first).
- The UI never freezes: download, decode and ffprobe run off the UI thread,
  with a loading state that shows download progress.
- Images use the terminal's graphics protocol when it has one (Ghostty →
  kitty), unicode half blocks otherwise.
- Audio/video: an info line (duration, codecs, resolution or sample
  rate/channels) and a thumbnail (a video frame, or embedded cover art).
  Space plays/stops through `ffplay`.

## Non-goals

- Video drawn inside the terminal; seek, pause, volume. Space is start/stop.
- Text editing, syntax highlighting, line wrap, horizontal scroll, non-UTF-8
  encodings (shown lossy).
- Previews of directories, of files over the limit, or of other formats
  (PDF, archives…): those get a one-line message, not a renderer.
- A "Preview" item in the right-click menu (Enter/double-click only).
- Bundling or installing ffmpeg.

## Anti-goals (a failure even if it technically works)

- The UI freezing while a 50 MB remote file downloads or a big image decodes.
- `ffplay` output or image escape codes corrupting the TUI, or an image left
  on screen after the popup closes.
- A temp copy of a remote file left behind after the popup closes or the app
  quits normally.
- Sound still playing after the popup closes or the app quits.

## Constraints

- ratatui is 0.30.2 → `ratatui-image = { version = "11.1.0", default-features
  = false, features = ["crossterm"] }` (drops `chafa-dyn`, which needs
  pkg-config/libchafa at build time) and `image = "0.25"` with its default
  formats minus `avif` (encoder-only in 0.25; it doubled the build). Final
  review call.
- `tempfile` moves from dev-dependencies to dependencies (already in the tree).
- ffmpeg/ffprobe/ffplay are optional at runtime, found on PATH. Without them,
  text and images still work.
- `Picker::from_query_stdio()` writes to the terminal and reads stdin, so it
  must run after `ratatui::init()` and before the input thread starts
  (`src/main.rs:136-141`). An error falls back to `Picker::halfblocks()`.
- 50 MB = 50 × 1024 × 1024 bytes, the same 1024 base `human_size` uses.
- UI copy in English, like the rest of the app. macOS and Linux.

## Approaches

- **A — ratatui-image + ffmpeg tools (chosen).** Real images in Ghostty,
  iTerm2, kitty, sixel terminals; half blocks elsewhere. ffprobe for media
  info, ffmpeg for one thumbnail frame, ffplay for playback (audio plays
  behind the popup; video opens ffplay's own window). Costs two new crates.
- B — ffmpeg only, no new crates: every image decoded by ffmpeg into half
  blocks we draw ourselves. Low resolution even in Ghostty, and no image
  preview without ffmpeg.
- C — text in a popup, everything else handed to `open`/`xdg-open`. Shortest,
  but leaves the terminal; not a preview popup.

User picked A: identification is mostly about how good the thumbnail looks,
and images must still work where ffmpeg is missing.

## Design

### Flow

1. `Command::Enter` on an entry that is not a dir and not `..`
   (`src/controller.rs:495`, today `_ => {}`) starts a preview of it.
   Double-click already applies `Command::Enter`, so it needs no change.
   The popup always opens, in a Loading state, and a task starts.
   - Remote and `size > LIMIT`: nothing is downloaded; the result is
     `Too big to preview (over 50 MB).` right away.
   - Local and `size > LIMIT`: the bytes aren't read; only the Media check
     (below) runs. Media → a normal media preview. Anything else → `Too big
     to preview (over 50 MB).`
2. The task:
   - Local pane: works on the file in place (`join(cwd, name)`).
   - Remote pane: streams `fs.open_read` into a `NamedTempFile` (prefix
     `term-scp-preview-`, suffix = the original extension) in the OS temp dir,
     reporting bytes done as it goes.
   - Then classifies the file in `spawn_blocking` and sends the result back.
   - Events carry a sequence number; one that doesn't match the open preview
     is dropped (and so is its temp file).
3. Classification, first match wins:
   1. **Image**: `image` recognises the format from the bytes and decodes it.
      Keep the dimensions and format name; downscale the pixels to at most
      1600 px on the long edge before handing them to the UI (see Terminal
      graphics).
   2. **Text**: no NUL byte in the first 8 KiB. `from_utf8_lossy`, CRLF → LF,
      tab → 4 spaces, other control characters dropped. (A text file that
      happens to start like an image header, e.g. `P1`, lands here.)
   3. **Broken image**: `image` recognised the format but decoding failed →
      `Can't decode image: <err>`. (Builder-check call: without this, a
      corrupt PNG would reach ffprobe, which sees a png "video" stream and
      would offer Space play.)
   4. **Media**: ffprobe is on PATH and finds an audio or video stream, and
      the container isn't one of ffmpeg's still-image demuxers (`image2`,
      `*_pipe`), so a big local picture is never offered as playable, nor
      one of its text-art demuxers (`tty`, `bin`, `xbin`, `adf`, `idf`, picked
      by extension: a big `.txt`, any `.bin`), and ffprobe's `probe_score` is
      over 25 (its own "misdetection possible" line: an ordinary binary is
      guessed as raw mpeg4 at 5). Final review call; mp3 clips under ~0.15 s
      score 25 and show no preview.
      Keep duration, codec names, resolution or sample rate/channels, and
      whether there's a real video stream (a stream with the `attached_pic`
      disposition is cover art, not video). Thumbnail: one frame via
      `ffmpeg … -frames:v 1 -f image2pipe -c:v png -` (a real video is seeked
      a little in so it isn't a black first frame; cover art isn't seeked),
      decoded with `image`. No frame → no thumbnail, not an error.
   5. **Nothing**: `No preview for this file type.` If ffprobe is missing it
      adds `Audio/video preview needs ffmpeg.`
   A read or download error shows `Can't read: <err>` in the popup.

### Popup

- Large and centered (most of the screen; exact margins are the builder's),
  over the same greyed backdrop and accent frame as the other popups.
- Title on the left of the top border is the file name; ✕ sits at the
  right end of the top border: `┏ clip.mp4 ━━━━━━━━━━━━━━ ✕ ┓`. Its cells
  are a mouse hit target (`app.hit` gets the rect, reset every draw like
  `hit.menu`).
- Body by state:
  - Loading: `Loading…`, plus `12.3 MB / 48.0 MB` for a remote download.
  - Text: the lines from the scroll offset, cut at the popup width.
    Footer: position (`lines 1–40 of 1,234`) and `↑↓ PgUp PgDn scroll · Esc close`.
  - Image: the picture fitted inside the body, aspect kept. Footer:
    `1920×1080 · PNG · 2.3 MB · Esc close`.
  - Media: thumbnail (if any) above the info line. Footer:
    `Space play · Esc close`, or `▶ 0:12 / 0:42 · Space stop · Esc close`
    while playing, or `Install ffmpeg to play` without ffplay.
  - Nothing/error: the message and the file size.
- Keys: Esc closes; Space plays/stops (media only); ↑ ↓ PgUp PgDn Home End
  scroll text. Every other key does nothing and the popup stays.
- Mouse: left click on ✕ closes; the wheel over the popup scrolls text;
  everything else is ignored, as with the other popups.

### Playback

- Space on a media preview with ffplay on PATH spawns `ffplay -autoexit
  -loglevel quiet -window_title <name> <path>`, plus `-nodisp` when there is
  no real video stream. stdin, stdout and stderr are null so it can't touch
  the terminal. `tokio::process::Command` with `kill_on_drop(true)`, owned
  by the preview.
- Space again stops it (kill). Closing the popup or quitting drops it, which
  kills it. When ffplay ends on its own, `Controller::tick` sees it
  (`try_wait`) and the footer goes back to `Space play`.
- Elapsed time comes from an `Instant` taken at spawn; the main loop already
  redraws every 100 ms.

### Terminal graphics

- `App` gains the `Picker`. main queries it as above; tests use
  `Picker::halfblocks()`.
- An image becomes a `StatefulProtocol` (`picker.new_resize_protocol`) held in
  the preview and drawn with `StatefulImage`, `Resize::Fit` with a smoothing
  filter (the default is nearest-neighbour).
- That resize + encode runs inside `render`, on the UI thread: a one-off
  hitch when the image first shows and on each terminal resize (kitty
  base64-encodes RGBA, iTerm2 PNG-encodes, sixel quantises). Builder-check
  call: accept it, bounded by the 1600 px cap (Fit never upscales, so the cap
  bounds the encode), and mark it `ponytail:` with ratatui-image's
  `ThreadProtocol` as the upgrade if the hitch is noticeable. The download,
  decode and ffprobe stay off the UI thread as stated in Goals.
- Kitty (Ghostty, kitty) draws through unicode placeholders, so the image goes
  away as soon as its cells are overwritten. Sixel and iTerm2 paint pixels
  that ratatui's diff may not overwrite, so closing a preview that showed an
  image forces one full redraw (`terminal.clear()` before the next draw).

### Lifetimes and cleanup

Everything a preview owns lives in its popup state and goes when the popup is
dropped (Esc, ✕, opening another preview, quit):
- the task's abort handle → a download in progress stops;
- the temp file (`NamedTempFile`) → deleted;
- the ffplay child (`kill_on_drop`) → killed.
A `kill -9` or a crash can leave a temp file in the OS temp dir; accepted.

### Code layout

- `src/preview.rs` (new): `LIMIT`, the loaded-content types, classification,
  ffprobe output parsing, thumbnail grab, the ffplay command. No UI.
- `src/model.rs`: `Popup::Preview` (boxed state), `App.picker`, the ✕ hit rect.
- `src/controller.rs`: Enter on a file; two new `AppEvent`s (download progress,
  result); preview keys and mouse; ffplay exit check in `tick`.
- `src/view.rs`: drawing the preview; Help line `Enter` → `Open dir / preview file`.
- `src/main.rs`: the picker query; the full-redraw flag.
- `README.md`: a short Preview section.

## Success criteria (tests)

- Classification: a PNG made in the test → Image with its dimensions;
  `"hello\nworld"` → Text with 2 lines; a PNG cut in half → Broken image;
  `"P1 is a text line"` → Text; bytes with a NUL and no media streams →
  Nothing.
- The limit: a remote file over it → `Too big…` with no download; a local
  file over it that isn't media (a sparse 51 MB file of zeros) → `Too big…`;
  a local WAV padded past 51 MB with `set_len` (skipped without ffmpeg) →
  Media.
- ffprobe output parsing from a fixed sample string.
- With ffmpeg on PATH (skipped otherwise): a 1 s sine WAV → Media, audio only,
  no thumbnail; a 1 s `testsrc` MP4 → Media with video and a thumbnail.
- Controller: Enter on a file opens a Loading preview, then the result lands;
  Esc closes; a click on the ✕
  rect closes; a double-click on a file opens a preview; Enter on a dir still
  opens the dir.
- Remote (`fs::test_remote`): the temp file exists while the preview is
  open and is gone after it closes.
- View: ✕ drawn at the right end of the preview's top border; a text preview
  shows its first lines and scrolls.
- Manual: a real image in Ghostty, and audio playing and stopping.

## Left to the builder

Exact popup margins; info-line wording; the seek offset for video thumbnails;
chunk size and how often download progress is reported; how the ffprobe
output is requested and parsed.
