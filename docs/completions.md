# Tab completion

Nothing installs these for you: run the step for your shell once, then open a new shell. zsh, bash and nushell complete the options, hosts from `~/.ssh/config`, remote folders after `HOST:`, local folders after `-l`, and files after `-i`. Hidden folders show once you type the `.`. The listing uses the `-P`, `-i` and `-o` already on the line, gives up after about 3 s when the host is unreachable, and never asks for a password, so it needs a key or a running agent.

The paths below are relative to the repository root.

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
