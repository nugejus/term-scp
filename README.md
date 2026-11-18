# term-scp

Two-pane file manager for the terminal: your machine on the left, an SSH host on the right, or two SSH hosts side by side. Copy and move files between them. On a window taller than it is wide, the panes stack: left on top, right below.

## Prerequisites

- macOS or Linux
- Rust 1.88+
- OpenSSH `ssh` on your `PATH`, and SFTP enabled on the host (it is by default)
- A Nerd Font in your terminal for the file icons, or `--no-icons`

## Installation

```bash
cargo install --path .
```

### Tab completion

Nothing installs these for you: run the step for your shell once, then open a new shell. zsh, bash and nushell complete the options, hosts from `~/.ssh/config`, remote folders after `HOST:`, local folders after `-l`, and files after `-i`. Hidden folders show once you type the `.`. The listing uses the `-P`, `-i` and `-o` already on the line, gives up after about 3 s when the host is unreachable, and never asks for a password, so it needs a key or a running agent.

**zsh:**

```bash
mkdir -p ~/.zfunc && cp completions/_term-scp ~/.zfunc/
```

Then add this to `~/.zshrc`, before `compinit`:

```zsh
fpath=(~/.zfunc $fpath)
```

**bash** (3.2 or newer). Add this to `~/.bashrc`. On macOS, Terminal starts login shells, which read `~/.bash_profile` instead, so add it there:

```bash
source /path/to/term-scp/completions/term-scp.bash
```

**nushell.** Copy the script into nushell's autoload folder; it loads in every new nu:

```nu
let d = ($nu.user-autoload-dirs | first); mkdir $d; cp completions/term-scp.nu $d
```

## Usage

```bash
term-scp HOST:DIR                                 # local pane starts in the current folder
term-scp HOST:DIR -l ~/Downloads                  # local pane starts in another folder
term-scp HOST:DIR HOST2:DIR2                      # two hosts, no local pane
term-scp HOST:DIR [-P PORT] [-i KEY] [-o OPT]...  # passed to ssh as -p / -i / -o
```

`HOST` is anything `ssh` accepts; an IPv6 address goes in brackets: `[::1]:/tmp`. A relative `DIR`, or one starting with `~`, is under the remote home; `HOST:` alone opens it. `-o` takes any ssh option except the ones term-scp sets itself (see [Connection](#connection)).

With two hosts, `-P`, `-i` and `-o` apply to both; put anything for one host only in `~/.ssh/config`. `-l` has no use there and is refused. Files copied between the hosts pass through your machine; nothing is stored on it.

| Key                         | Action                                   |
|-----------------------------|------------------------------------------|
| `↑` / `↓`                   | Previous / next file                     |
| `Shift` `↑` / `↓`           | Select a range; going back shrinks it    |
| `15` `↓`                    | 15 files down; any number, also with `↑` and `Shift` |
| `PgUp` / `PgDn`             | Page up / down                           |
| `Home` / `End`              | First / last file                        |
| `Backspace`                 | Parent folder                            |
| `Enter`                     | Open the folder                          |
| `←` / `→`                   | Left / right pane                        |
| `Tab`                       | Switch pane                              |
| `Esc`                       | Clear the marks                          |
| `c`                         | Copy the selection into the other pane's folder |
| `Shift` `C`                 | Copy to a path you type, e.g. a new name (see [Copy to and Move](#copy-to-and-move)) |
| `m`                         | Move to a path you type                  |
| `n`                         | New folder                               |
| `Shift` `N`                 | New empty file                           |
| `g`                         | Change folder; the line starts with the current one |
| `d`                         | Delete the selection; asks first         |
| `r`                         | Rename the file under the cursor         |
| `f`                         | Filter the list by name (see [Delete, rename and filter](#delete-rename-and-filter)) |
| `Ctrl` `R`                  | Refresh the focused pane                 |
| `?`                         | Show these keys                          |
| `q` / `Ctrl` `C`            | Quit; asks first while transfers run     |

The selection is the marked files, or the file under the cursor when nothing is marked. `..` is never part of it.
- Moving the cursor without `Shift` (`↑` / `↓`, `PgUp` / `PgDn`, `Home` / `End`) clears the marks, and so does opening another folder. A copy and `Ctrl` `R` keep them.
- The bottom line shows how many files are marked in the focused pane and their size (folders not counted), and the last message or error. The next key clears the message.

The toolbar buttons do what their keys do. Everything is also clickable; double-click opens a folder, and the wheel moves the cursor in the pane under the pointer without focusing it. Drag a file onto the other pane to copy it into that pane's folder, or onto a folder row other than `..` to copy into that folder. Its dimmed name follows the pointer. Dragging a marked file takes all marked files.

## Copy to and Move

`Shift` `C` and `m` open a line with the destination filled in: `<other pane's folder>/<name>` for one file, the other pane's folder for several. The box is a third of the screen wide; a long path wraps.
- Edit the last part to copy under a new name.
- Edit the rest to copy somewhere else on the other side. `~` is the other side's home.
- A path that ends in `/`, or is an existing folder with another name, puts the files inside it. That folder must already exist; term-scp doesn't create it.
- `Ctrl` `U` clears the line. `Enter` confirms; `Esc` or an empty line cancels.

Folders are copied with everything in them. A folder copied onto an existing folder of the same name is merged into it.

Move deletes a source only after its copy succeeded, and a folder only if everything in it was moved. A symlink to a folder is copied as its contents, and only the link is removed.

When a file is already there, you are asked:

```
[o] Overwrite  [s] Skip  [a] Overwrite all  [Esc] Cancel job
```

- The answer applies to that transfer only. `a` holds for the rest of it.
- Other transfers keep running. Their questions wait their turn.

## Delete, rename and filter

- `d` asks `Delete "name"? [y/N]` (or `Delete 3 items?`); only `y` deletes. Folders go with everything in them. A symlink is removed, never what it points to. A file that fails doesn't stop the rest; the bottom line shows the last error.
- `r` opens a line with the name. A name that already exists is refused, never replaced. A path (`../x`, `~/x`) moves the file on the same side.
- `f` opens a bar at the top of the pane. The list narrows as you type to names that contain the text, ignoring case; it is plain text, not a regex or a wildcard.
  - `Enter` keeps the filter; the bar stays to show it. `f` again edits it.
  - `Esc` in the bar clears it, and so does opening another folder.
  - Marks on files the filter hides are dropped, so a copy or delete only takes what you see.
- The number you type before `↑` / `↓` shows in the bottom line; any other key drops it.

## Transfers

- Up to 4 transfers run at once, each on its own SFTP channel; more wait in line, shown as `+N` on the progress bar. You can keep browsing while they run.
- The progress bar shows the file, the percentage, the speed and files done/total. With several transfers it shows `N jobs` and their combined percentage and speed.
- A file is written as `<name>.term-scp-part` and renamed into place only when complete, so a failed copy leaves the file it replaces as it was. On the host the old file is deleted just before the rename; if that rename fails, the new copy stays as `<name>.term-scp-part`.
- Two transfers never write the same file at once. The second fails that file with `being written by another transfer`.
- File permissions are kept; modification times are not, as with plain `scp`.
- Symlinks are followed. A link back to a parent folder fails with `symlink loop`. FIFOs, sockets, devices and broken links are not copied; each fails with `not a regular file`.
- Names uploaded from macOS are converted to NFC, so Hangul and other composed names don't show up as separated letters on Linux. Downloads keep their names.
- A failed file doesn't stop the rest of the transfer. The summary counts the failures and shows the last error.

## Connection

term-scp runs your `ssh`, so `~/.ssh/config`, the agent and `known_hosts` apply as is.
- The login happens before the screen opens, so password, passphrase and new-host-key prompts work as usual.
- You log in once per host, one host after the other; browsing and every transfer share that connection.
- `ssh` runs with the options `scp` uses (no forwarding, no `RemoteCommand`, no tty), so a host whose config runs a command on login still works.
- Messages from `ssh` show in the bottom line. If the connection dies, the remote pane shows `[disconnected]`; start term-scp again to reconnect.

## Not yet

- Opening or editing files
- Cancelling a single transfer. Quitting cancels all of them and leaves their `.term-scp-part` files behind.
- Dropping files in from Finder, or pasting. Don't: the text arrives as keys, so a `c` in it starts a copy and a `q` quits.
