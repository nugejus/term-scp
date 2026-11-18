mod controller;
mod fs;
mod icons;
mod model;
mod transfer;
mod view;
use clap::Parser;
use controller::{AppEvent, Controller};
use fs::{Fs, Remote, SshOpts, sftp_err};
use model::{App, Pane, Side};
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;
use std::io::stdout;
use std::process::ExitCode;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(version, about = "Two-pane SFTP file manager")]
struct Cli {
    /// [user@]host:[path]
    target: String,
    /// A second [user@]host:[path]: two hosts side by side, no local pane
    target2: Option<String>,
    #[arg(short = 'P', long)]
    port: Option<u16>,
    #[arg(short = 'i', long)]
    identity: Option<String>,
    /// Extra ssh option (repeatable)
    #[arg(short = 'o', long = "ssh-option")]
    ssh_option: Vec<String>,
    #[arg(short = 'l', long, conflicts_with = "target2")]
    local: Option<String>,
    #[arg(long)]
    no_icons: bool,
}

#[derive(Debug)]
struct Target {
    host: String,
    path: String,
}

fn parse_target(s: &str) -> Result<Target, String> {
    let (mut depth, mut split) = (false, None);
    for (i, c) in s.char_indices() {
        match c {
            '[' => depth = true,
            ']' => depth = false,
            ':' if !depth => {
                split = Some(i);
                break;
            }
            _ => {}
        }
    }
    let i = split.ok_or("expected [user@]host:[path]")?;
    let host = s[..i].replace(['[', ']'], "");
    Ok(Target {
        host,
        path: s[i + 1..].to_string(),
    })
}

/// SFTP resolves relative paths against the remote home.
fn remote_start(path: &str) -> String {
    match path {
        "" | "~" => ".".into(),
        p => p.strip_prefix("~/").unwrap_or(p).into(),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err((code, msg)) => {
            eprintln!("{msg}");
            ExitCode::from(code)
        }
    }
}

/// Errors return (never `exit`), so `Drop for Remote` kills ssh and removes the control dir.
fn run() -> Result<(), (u8, String)> {
    let cli = Cli::parse();
    let target = |s: &str| parse_target(s).map_err(|e| (2, format!("term-scp: {s}: {e}")));
    let t = target(&cli.target)?;
    // An scp habit, `term-scp host:dir ~/Downloads`, lands here.
    let t2 = cli
        .target2
        .as_deref()
        .map(target)
        .transpose()
        .map_err(|(code, e)| (code, format!("{e}\n(a local folder goes after -l)")))?;
    // Before connecting: a bad -l must fail without an ssh login.
    let dir = cli.local.unwrap_or_else(|| ".".into());
    let local = match t2 {
        Some(_) => String::new(),
        None => std::fs::canonicalize(&dir)
            .map_err(|e| (1, format!("term-scp: {dir}: {e}")))?
            .to_string_lossy() // ponytail: lossy non-UTF-8 local names
            .into_owned(),
    };
    let rt = tokio::runtime::Runtime::new().map_err(|e| (1, format!("term-scp: {e}")))?;
    let opts = SshOpts {
        port: cli.port,
        identity: cli.identity,
        options: cli.ssh_option,
    };
    // Before raw mode: ssh's password, passphrase and host-key prompts need a normal terminal.
    // One host after the other, so two hosts' prompts never mix.
    let connect = |t: &Target| -> Result<(Fs, String), (u8, String)> {
        let remote = rt
            .block_on(Remote::connect_ssh(&t.host, &opts))
            .map_err(|e| (1, format!("term-scp: cannot connect to {}:\n{e}", t.host)))?;
        let cwd = rt
            .block_on(remote.sftp.canonicalize(remote_start(&t.path)))
            .map_err(|e| {
                (
                    1,
                    format!("term-scp: {}:{}: {}", t.host, t.path, sftp_err(e)),
                )
            })?;
        Ok((Fs::Remote(Arc::new(remote)), cwd))
    };
    let ((lfs, lcwd), (rfs, rcwd)) = match &t2 {
        None => ((Fs::Local, local), connect(&t)?),
        Some(t2) => (connect(&t)?, connect(t2)?),
    };

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        hook(info);
    }));
    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableMouseCapture);

    let (tx, rx) = mpsc::channel();
    let input = tx.clone();
    std::thread::spawn(move || {
        while let Ok(e) = event::read() {
            if input.send(AppEvent::Input(e)).is_err() {
                break;
            }
        }
    });

    let left = Pane::new(lfs, lcwd.clone());
    let right = Pane::new(rfs, rcwd.clone());
    let mut app = App::new(left, right, !cli.no_icons);
    let mut ctl = Controller::new(rt.handle().clone(), tx);
    ctl.load(&mut app, Side::Left, lcwd, None);
    ctl.load(&mut app, Side::Right, rcwd, None);

    while !app.quit {
        if terminal.draw(|f| view::draw(f, &mut app)).is_err() {
            break; // the terminal is gone
        }
        if let Ok(ev) = rx.recv_timeout(Duration::from_millis(100)) {
            ctl.handle(&mut app, ev);
            for ev in rx.try_iter() {
                ctl.handle(&mut app, ev);
            }
        }
        // Every iteration: progress events alone would starve a timeout-only tick.
        ctl.tick(&mut app, Instant::now());
    }

    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    {
        let _g = rt.enter();
        drop(app);
        drop(ctl);
    }
    // Drops in-flight tasks, and with them their `Arc<Remote>`s: `Drop for Remote` removes the control dir.
    rt.shutdown_timeout(Duration::from_secs(1));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_alias_and_path() {
        let t = parse_target("devbox22:~/proj").unwrap();
        assert_eq!((t.host.as_str(), t.path.as_str()), ("devbox22", "~/proj"));
    }
    #[test]
    fn keeps_user_in_host() {
        assert_eq!(parse_target("me@h:/x").unwrap().host, "me@h");
    }
    #[test]
    fn empty_path_is_allowed() {
        assert_eq!(parse_target("h:").unwrap().path, "");
    }
    #[test]
    fn ipv6_brackets_are_stripped() {
        let t = parse_target("me@[::1]:/tmp").unwrap();
        assert_eq!((t.host.as_str(), t.path.as_str()), ("me@::1", "/tmp"));
    }
    #[test]
    fn missing_colon_is_an_error() {
        assert_eq!(
            parse_target("devbox22").unwrap_err(),
            "expected [user@]host:[path]"
        );
    }
    #[test]
    fn second_host_is_optional_and_excludes_local_dir() {
        let one = Cli::try_parse_from(["term-scp", "a:/x", "-l", "/tmp"]).unwrap();
        assert_eq!((one.target2, one.local.as_deref()), (None, Some("/tmp")));
        let two = Cli::try_parse_from(["term-scp", "a:/x", "b:/y", "-P", "22"]).unwrap();
        assert_eq!(two.target2.as_deref(), Some("b:/y"));
        assert!(Cli::try_parse_from(["term-scp", "a:/x", "b:/y", "-l", "/tmp"]).is_err());
        assert!(Cli::try_parse_from(["term-scp", "a:/x", "b:/y", "c:/z"]).is_err());
    }

    /// A HOME with `~/.ssh/config`, folders `proj/deep`, `sub1`, `sub2`, `.hid`, `sp ace`, `it's`,
    /// `dol$x`, a file, and `bin/ssh`: a fake that logs its options to `ssh.log` and runs the
    /// remote command in HOME, as `ssh [opts] host cmd...` would.
    fn completion_home() -> (tempfile::TempDir, String) {
        let t = tempfile::tempdir().unwrap();
        let h = std::fs::canonicalize(t.path()).unwrap();
        let h = h.to_string_lossy().into_owned();
        for d in [
            ".ssh",
            "bin",
            "proj/deep",
            "sub1",
            "sub2",
            ".hid",
            "sp ace",
            "it's",
            "dol$x",
        ] {
            std::fs::create_dir_all(format!("{h}/{d}")).unwrap();
        }
        std::fs::write(format!("{h}/f.txt"), "").unwrap();
        let config = "Host lab42 other\nHost *.x\n  HostName y\nhost lab9\n";
        std::fs::write(format!("{h}/.ssh/config"), config).unwrap();
        let ssh = "#!/bin/sh\nwhile [ $# -gt 0 ]; do case $1 in\n\
                   -o | -p | -i) printf '%s %s\\n' \"$1\" \"$2\" >> \"$HOME/ssh.log\"; shift 2 ;;\n\
                   -*) shift ;; *) break ;; esac; done\n\
                   shift\ncd \"$HOME\" && exec sh -c \"$*\"\n";
        std::fs::write(format!("{h}/bin/ssh"), ssh).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let exe = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(format!("{h}/bin/ssh"), exe).unwrap();
        (t, h)
    }

    fn run_shell(home: &str, prog: &str, args: &[&str], line: &str) -> Vec<String> {
        let path = format!("{home}/bin:{}", std::env::var("PATH").unwrap());
        let out = std::process::Command::new(prog)
            .args(args)
            .env("LINE", line)
            .env("HOME", home)
            .env("PATH", path)
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{err}");
        let mut v: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        v.retain(|l| !l.is_empty());
        v.sort();
        v
    }

    /// Completes `line` (the cursor at its end) with the bash script.
    fn bash_complete(home: &str, line: &str) -> Vec<String> {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/completions/term-scp.bash");
        // what bash sets before calling the function; the script reads only these two
        let cmd = format!(
            r#"source '{script}'; COMP_LINE=$LINE; COMP_POINT=${{#COMP_LINE}};
            _term_scp; printf '%s\n' "${{COMPREPLY[@]}}""#
        );
        run_shell(home, "bash", &["-c", &cmd], line)
    }

    #[test]
    fn bash_completion_hosts_flags_and_folders() {
        let (_t, h) = completion_home();
        let c = |line: &str| bash_complete(&h, line);
        assert_eq!(c("term-scp lab"), ["lab42:", "lab9:"]);
        assert_eq!(c("term-scp -P 22 lab"), ["lab42:", "lab9:"]); // 22 is no HOST:DIR
        assert_eq!(c("term-scp --l"), ["--local "]);
        assert!(c("term-scp a:/x b:/y --l").is_empty()); // -l and a second host exclude each other
        assert!(c("term-scp a:/x b:/y lab").is_empty());
        assert!(c("term-scp -l /tmp a:/x lab").is_empty());
        assert_eq!(
            c(&format!("term-scp a:/x -l {h}/su")),
            [format!("{h}/sub1/"), format!("{h}/sub2/")]
        );
        assert_eq!(
            c(&format!("term-scp a:/x --identity={h}/f")),
            [format!("{h}/f.txt")]
        );
        // bash replaces only what follows the ':' it split at
        assert_eq!(c("term-scp lab9:pr"), ["proj/"]);
        assert_eq!(c("term-scp lab9:proj/"), ["proj/deep/"]);
        assert_eq!(c("term-scp lab9:~/proj/"), ["~/proj/deep/"]);
        assert_eq!(
            c(&format!("term-scp lab9:{h}/su")),
            [format!("{h}/sub1/"), format!("{h}/sub2/")]
        );
        // Quoted for the shell, and typed escapes are understood.
        assert_eq!(c("term-scp lab9:sp"), ["sp\\ ace/"]);
        assert_eq!(c("term-scp lab9:sp\\ ace/"), Vec::<String>::new()); // inside: no subfolders
        assert_eq!(c("term-scp lab9:it"), ["it\\'s/"]);
        assert_eq!(c("term-scp lab9:do"), ["dol\\$x/"]);
        assert_eq!(c("term-scp a:/x -l ~/sp"), ["~/sp\\ ace/"]);
        assert_eq!(c("term-scp a:/x -l ~/su"), ["~/sub1/", "~/sub2/"]);
        // Hidden folders only once a '.' is typed.
        assert!(!c("term-scp lab9:").iter().any(|x| x.starts_with('.')));
        assert_eq!(c("term-scp lab9:.h"), [".hid/"]);
        // The line's ssh options reach the listing.
        let _ = std::fs::remove_file(format!("{h}/ssh.log"));
        c("term-scp -P 2222 --identity=~/k -o ProxyJump=j lab9:");
        let log = std::fs::read_to_string(format!("{h}/ssh.log")).unwrap();
        for want in ["-p 2222", "-i ~/k", "-o ProxyJump=j"] {
            assert!(log.contains(want), "{log}");
        }
    }

    #[test]
    fn nu_completion_hosts_and_folders() {
        if std::process::Command::new("nu")
            .arg("--version")
            .output()
            .is_err()
        {
            return eprintln!("skipped: no nushell");
        }
        let (_t, h) = completion_home();
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/completions/term-scp.nu");
        let nu = |line: &str| {
            let cmd = format!(
                "source '{script}'; nu-complete term-scp target '{line}' | get completions | str join (char nl)"
            );
            run_shell(&h, "nu", &["--no-config-file", "-c", &cmd], line)
        };
        assert_eq!(nu("term-scp lab"), ["lab42:", "lab9:"]);
        assert_eq!(nu("term-scp lab9:pr"), ["lab9:proj/"]);
        assert_eq!(nu("term-scp lab9:~/proj/"), ["lab9:~/proj/deep/"]);
        assert_eq!(nu("term-scp lab9:sp"), ["`lab9:sp ace/`"]); // quoted, as nu quotes paths
        assert_eq!(nu("term-scp lab9:.h"), ["lab9:.hid/"]);
        assert!(nu("term-scp a:/x -l /tmp lab").is_empty()); // -l leaves room for one host
        assert!(nu("term-scp a:/x b:/y lab").is_empty());
        let _ = std::fs::remove_file(format!("{h}/ssh.log"));
        nu("term-scp -P 2222 -o ProxyJump=j lab9:");
        let log = std::fs::read_to_string(format!("{h}/ssh.log")).unwrap();
        assert!(
            log.contains("-p 2222") && log.contains("-o ProxyJump=j"),
            "{log}"
        );
    }

    #[test]
    fn remote_start_maps_home_forms() {
        assert_eq!(remote_start(""), ".");
        assert_eq!(remote_start("~"), ".");
        assert_eq!(remote_start("~/a/b"), "a/b");
        assert_eq!(remote_start("/abs"), "/abs");
        assert_eq!(remote_start("rel"), "rel");
    }
}
