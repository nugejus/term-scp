# term-scp — design

Date: 2026-11-18 · Status: approved by user

## Intent

A WinSCP-style, two-pane file transfer tool that runs in the terminal.
`term-scp devbox22:~/proj` opens local (left) and remote (right) side by side;
files move between them with single keys or mouse drag-and-drop.

Why: the user moves files between their Mac and several SSH hosts (8 aliases in
`~/.ssh/config`). They want one general-purpose tool that is equally good at
**large transfers** (videos, datasets: background, accurate progress, full
throughput) and **fast browsing** (hopping directories and copying small
code/config files without waiting).

## Goals

- Works with every host the user's `ssh` already works with: aliases,
  User/Port/IdentityFile, agent, known_hosts, ProxyJump, password and key
  passphrase prompts.
- Never freezes the UI on network I/O; you can browse while a transfer runs.
- Large-file throughput close to `scp`/`sftp` (pipelined requests).
- Keyboard and mouse are both first-class.
- MVC structure where adding a new file operation (delete, rename, later
  edit/open) means adding one command, one key, and one backend method, with no
  restructuring.

## Non-goals (this round)

- Delete, rename (structure keeps them easy to add; see Extensibility).
- Quick file editing, opening files (user's "not yet" list).
- Local→local copies. Remote→remote (two hosts side by side) is not built
  now, but the structure must allow it (see Extensibility). Every copy goes
  to the other pane.
- Cancelling a running transfer (quitting cancels; add later).
- Dropping files from Finder into the terminal (user declined). Drag-and-drop
  is between the two panes only.
- Auto-reconnect after the SSH connection drops.
- Windows support.

## Anti-goals (would make it a failure even if it "works")

- Reimplementing SSH config parsing or authentication.
- Parsing `ls` output.
- Losing an existing destination file because an overwrite failed midway.
- ssh's stderr leaking into the TUI and corrupting the screen.

## Constraints

- Rust (edition 2024, toolchain 1.96 installed). UI text in English.
- Architecture: Model / View / Controller ("MCV" in the request = MVC).
- Requires system `ssh` (OpenSSH) and an SFTP subsystem on the server.
- The user's terminal is Ghostty with JetBrainsMono Nerd Font, so icons are
  Nerd Font glyphs.

## Approaches considered

| | Approach | Trade-off |
|---|---|---|
| **A (picked)** | Spawn system `ssh <args> <host> -s sftp`; speak SFTP over its stdin/stdout with the `russh-sftp` client | ssh handles config/auth/host keys exactly as today; structured listings; pipelined I/O. Needs `ssh` binary. |
| B | Pure-Rust SSH (`russh`/`ssh2`) | Own auth UI, no ssh binary, but must reimplement `~/.ssh/config`, agent, known_hosts (security-sensitive). Aliases work only as well as our parser. |
| C | Run `ssh host ls -la` + `scp` per action | Least code, but new connection per action (slow), fragile `ls` parsing, no progress from `scp` without a tty. Fails both goals. |

The user picked **A**.

Verified during design: `russh-sftp` 3.0.1 `SftpSession::new(stream)` accepts any
`AsyncRead + AsyncWrite + Unpin + Send` stream (does not depend on `russh`),
pipelines file reads/writes (`max_concurrent_reads/writes`, default 16), and all
methods take `&self`, so one `Arc<SftpSession>` can serve browsing and transfers
concurrently over a single connection. It also exposes `rename`, `remove_file`,
`remove_dir`, `canonicalize`, `set_metadata`. Requires tokio.

## Decided by the user

**CLI:** `term-scp host:path/to/dir [--options]`

**View**
1. Two main panes: left = local, right = remote.
2. Each pane is a bordered box; the top border shows
   `local:/absolute/path` and `host:/absolute/path`.
3. The focused pane's border is sky-blue + bold.
4. The cursor row is highlighted.
5. An icon before each name: folder, video, audio, csv/tsv, text, python, rust,
   c, c++, typescript, go, javascript, java, markdown, toml, yaml, json, config,
   html, j2, sh, other.
6. A top toolbar: New Dir, New File, Help, Move, Copy to, Change Dir, plus a
   progress bar for transfers.

**Controls**
- Mouse drag-and-drop; mouse-wheel scrolling.
- `c`: quick copy the selection to the other pane's current dir (local→remote
  or remote→local).
- `↑`/`↓`: previous/next file. `←`/`→`: parent dir / enter selected dir.
- `Tab`: switch pane.
- `C`: copy with rename; shows a text-input box.
- `n`: new dir in the focused pane's cwd (name popup).
- `N`: new file in the focused pane's cwd (name popup).
- `Shift+↑/↓`: select multiple files; marked rows use a toned-down highlight
  so the cursor highlight stays distinct.

Delete and rename are out this round, but must be easy to add later.

## Designer's calls (proposed; user may correct)

**Screen**

```
 [New Dir] [New File] [Move] [Copy to] [Change Dir] [Help]   ⇅ data.mp4 ███████░░░ 68% 12.3MB/s 2/5 +1
┌ local:/Users/sli/term-scp ────────────┐┏ devbox22:/home/sli/proj ━━━━━━━━━━━━━┓
│   ..                                  │┃   ..                                 ┃
│   src                                 │┃   data                               ┃
│   Cargo.toml          4K  11-18 12:30 │┃   train.py         12K  11-17 09:12  ┃  cursor: bright
│   README.md           2K  11-18 12:31 │┃   model.rs          8K  11-17 09:15  ┃  marked: muted
└───────────────────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
 2 marked · 20K                                      Permission denied: /etc/shadow
```

- Columns: icon, name, human size (blank for dirs), modified time.
  Columns drop right-to-left when the pane is narrow.
- Bottom status line: mark count + total size on the left, last message/error
  on the right.
- Hidden files shown. Sort: `..` first, then dirs, then files, each by name.
- Cursor highlight is shown in both panes; only the focused pane's is bright.
- Progress text: bar = bytes done / total across all running jobs, with %
  and combined speed. With one job running it shows that job's current file
  name and `done/total` files. With several it shows `N jobs` instead of a
  file name. `+N` counts queued jobs. Hidden when idle.
- `--no-icons` drops the glyphs for terminals without a Nerd Font.

**Selection**
- "The selection" = marked rows if any, else the cursor row. `..` is never part
  of a selection.
- `Shift+↑/↓` extends an anchor-based range (shrinks when reversing, like a GUI
  list). Plain arrows move the cursor and keep marks. `Esc` clears marks.
  Changing directory clears marks.

**Keys added beyond the request**
- `Enter` = `→`. `m` = Move. `g` = Change Dir. `?` = Help. `Ctrl+R` = refresh
  focused pane. `q` = quit (asks first if a transfer is running).
- Leaving a dir with `←` puts the cursor on the dir you came from.

**Mouse**
- Click a row: focus that pane, move the cursor there.
- Double-click a dir (or `..`): enter it.
- Wheel: moves the cursor 3 rows in the pane under the pointer.
- Click a toolbar button: same as its key.
- Drag: press on a row, move with the button held, release over the other
  pane → copy. Dragged set = the selection if the pressed row is marked,
  else just the pressed row. Release on a dir row → copy into that dir;
  anywhere else in that pane → its cwd. Release in the same pane → nothing.
  While dragging, the status line shows `Drop to copy N item(s)`.

**Operations**
- `c` / drag: copy selection to the other pane's cwd. Dirs copy recursively.
- `C` / toolbar Copy to: one popup, prefilled with
  `<other pane's cwd>/<name>` (single item) or `<other pane's cwd>`
  (multiple items, meaning destination dir). Editing the last segment renames;
  editing the path copies elsewhere on the other side. A leading `~` is
  expanded on that side.
- Move (`m` / toolbar): opens the same destination popup as Copy to
  (prefilled the same way). Pressing Enter in it is the confirmation that
  sources will be removed. It copies, then removes each source item only
  after that item copied successfully.
- New Dir / New File: popup, create in the focused pane's cwd, refresh, put
  the cursor on the new item. New file is created empty and fails if the name
  exists.
- Change Dir (`g` / toolbar): popup prefilled with the focused pane's cwd.
  Accepts absolute, relative (to cwd), or `~`-prefixed paths.
- Conflicts: when a destination file exists, a popup asks
  `Overwrite / Skip / Overwrite all / Cancel` (keys o/s/a/Esc).
  "Overwrite all" lasts for the rest of that job.
- Overwrite safety: write to `<dest>.term-scp-part`, then replace the
  destination. Local side: `rename` (atomic on POSIX). Remote side:
  `remove(dest)` then `rename(part, dest)`. `russh-sftp` 3.0.1 can't send
  `posix-rename@openssh.com`, and plain SFTP rename fails when the target
  exists. If the link drops between those two calls, the complete new
  content survives as `.term-scp-part`, so no data is lost. A failed
  transfer removes the `.part` file and never touches the original.
- Filenames uploaded from macOS are normalized to Unicode NFC
  (`unicode-normalization`). Finder often produces NFD names, so Hangul
  names would otherwise show as separated jamo on Linux and miss conflict
  checks against existing NFC names. Downloads keep names as-is.
- File permission bits are preserved on copy; mtime is not (same as plain
  `scp`). Symlinks are followed (their target is listed and copied).
- Transfers (user's choice: concurrent). Each `c`/`C`/Move/drag creates a
  job. Up to **4 jobs run at once**, each on **its own** SFTP channel over
  the shared SSH connection (see Connection), so a small copy starts
  immediately even while a multi-GB transfer runs. Further jobs wait in FIFO
  order. A job scans first (recursive walk for total bytes/files), then
  copies its items one by one (each file pipelined). When a job finishes,
  the pane(s) showing its destination or source dir refresh. Errors on an
  item are reported in the status line and the job continues with the next
  item; the job's summary says how many failed.
- Conflict prompts from concurrent jobs are queued and shown one at a time.
  Each prompt names its job and file, and only that job pauses until it is
  answered. "Overwrite all" and Cancel apply to that job only.

**Connection**
- Spawn `ssh [-P→-p port] [-i id] [-o opt]... <[user@]host> -s sftp` with
  stdin/stdout piped and stderr captured into a buffer.
- Handshake (SFTP INIT) and resolving the start path happen **before** the
  TUI enters raw mode / alternate screen, so ssh's own prompts (password,
  passphrase, new host key) appear in the normal terminal via `/dev/tty`.
  If the handshake fails, print ssh's captured stderr and exit non-zero.
- Browsing and transfers use **separate SFTP channels over one SSH
  connection**. The browse ssh is started with
  `-o ControlMaster=yes -o ControlPath=<private 0700 temp dir>/cm
  -o ControlPersist=no`. Each running transfer job opens its own channel:
  `ssh -o ControlMaster=no -o ControlPath=<same> <host> -s sftp`, closed
  when the job ends. You
  authenticate once. Each channel gets its own `sftp-server` process and
  SSH flow-control window, so a directory listing never queues behind
  megabytes of in-flight transfer reads. If the mux channel can't be opened,
  transfers fall back to the browse session. Transfer sessions raise the
  per-request timeout (`set_timeout`, default 10 s) so 16 pipelined reads
  don't time out on slow links. (Builder: keep the socket path under the
  ~104-byte unix socket limit; fall back to `/tmp` if `$TMPDIR` is too long.)
- After that, ssh stderr is never written to the terminal; anything new is
  surfaced in the status line. If the ssh process exits, the remote pane
  shows `disconnected` and remote operations report an error; local
  browsing keeps working.

**CLI**
```
term-scp [OPTIONS] <[user@]host:[path]>
  -P, --port <PORT>        passed to ssh as -p
  -i, --identity <FILE>    passed to ssh as -i
  -o, --ssh-option <OPT>   passed to ssh as -o (repeatable)
  -l, --local <DIR>        starting local dir (default: current dir)
      --no-icons           plain text, no Nerd Font glyphs
```
Empty remote path = remote home. A relative remote path resolves against
remote home. `[v6addr]:path` is accepted.

## Architecture (MVC)

```
main.rs        parse args, connect (pre-TUI), build Model, run loop, restore terminal
model.rs       App state: two Panes (cwd, entries, cursor, scroll, marks/anchor),
               focus, popup state, transfer status, status message. Pure data +
               small methods (move cursor, extend range, selection()). No I/O.
view.rs        ratatui rendering of Model → frame. Also records the screen Rects
               of panes, rows and toolbar buttons into the Model each frame so
               the controller can hit-test mouse events. No mutation otherwise.
controller.rs  crossterm events → `Command` → apply to Model / dispatch I/O.
               Key and mouse bindings both map to the same `Command` enum.
fs.rs          `enum Fs { Local, Remote(Arc<Remote>) }` with async methods:
               list, stat, mkdir, create_new, open_read, create_write,
               remove (file/dir), rename, canonicalize. One place per backend.
               `Remote` = one SSH host: its label (`devbox22`), ssh args,
               ControlPath, browse `SftpSession`, and `open_channel()` for
               transfer channels. Nothing in it assumes there is only one.
transfer.rs    job scheduler (≤4 running, FIFO queue) and per-job task: open own
               SFTP channel, scan → copy loop with progress events, conflict
               prompts round-trip to the UI (tagged by job id), .part + replace.
icons.rs       extension/name → (glyph, color).
```

- Runtime: tokio. UI loop on the main thread; network/disk work as tokio
  tasks that send results back over a channel (`Event::Listed`,
  `Event::Progress`, `Event::Conflict`, `Event::Done`, `Event::Error`...). The
  loop redraws on input, on worker events, and on a ~100 ms tick while a
  transfer runs.
- Crates: `ratatui` 0.30, `crossterm` 0.29 (mouse capture), `russh-sftp` 3.0,
  `tokio`, `clap` (derive), `unicode-normalization`. Nothing else unless it
  removes code.

## Extensibility

Adding delete or rename later:
1. Add `Command::Delete` / `Command::Rename` variant.
2. Bind a key and (optionally) a toolbar button in the controller's tables.
3. Call `Fs::remove` / `Fs::rename` (already exist: Move needs remove, and
   `.part` replace needs rename). Delete reuses the confirm-popup type the
   conflict prompt already uses; rename reuses the text-input popup.

Remote → remote later (user asked to keep this open):
- A `Pane` owns its `Fs`; nothing hard-codes "left = local, right =
  remote". The border label comes from the pane's `Fs` (`local:` or
  `<host>:`), and "copy" always means "to the other pane".
- The transfer engine copies `(src: &Fs, src_path) → (dst: &Fs, dst_path)`
  by streaming reads into writes. There is no local/remote branch in the
  copy loop. Remote→remote then works unchanged, with data streaming
  through this machine. The only side-specific rules (NFC normalization,
  local atomic rename vs. remote remove+rename) key off the `Fs` variant.
- What's left for later: accept two `host:path` arguments in the CLI and
  connect both before the TUI starts.

No stubs or placeholder commands are written now.

## Verification

- Unit tests (pure, no I/O): `host:path` parsing (user@, empty path, IPv6),
  icon mapping, selection model (anchor range grow/shrink, `..` excluded,
  marks cleared on cd), Copy-to destination resolution (single vs multiple,
  `~`), human size formatting.
- Integration tests against a **real SFTP server without sshd**: the remote
  backend's spawn command is injectable, and tests spawn
  `/usr/libexec/sftp-server` (present on macOS; `/usr/lib/openssh/sftp-server`
  on Linux) on temp dirs. Covers list, mkdir, create_new, recursive
  upload/download with byte-for-byte compare, conflict paths, overwrite
  safety (failed transfer leaves the original intact), Move removes sources,
  two concurrent jobs both complete correctly, NFC normalization on upload.
  (The ControlMaster path itself is exercised in the manual run, since the
  test harness has no sshd. Tests inject a channel factory that spawns
  another `sftp-server`.)
- Manual: run against one of the user's devbox aliases; drag-and-drop and
  wheel in Ghostty.

## Left to the builder

- Exact colors (beyond sky-blue focus and bright-vs-muted highlights), glyph
  choices per type, column widths, popup sizes.
- Buffer sizes and pipeline depth (start with russh-sftp defaults; measure
  against `scp` on a large file).
- Module boundaries inside each file; whether `fs.rs` methods are inherent
  async fns on the enum (suggested) or something else.
- Throttling progress events (aim ≤ 10/s).
