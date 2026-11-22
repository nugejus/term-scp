# Transfers and connection

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
