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
#[command(version, about = "Two-pane local | remote SFTP terminal tool")]
struct Cli {
    /// [user@]host:[path]
    target: String,
    #[arg(short = 'P', long)]
    port: Option<u16>,
    #[arg(short = 'i', long)]
    identity: Option<String>,
    /// Extra ssh option (repeatable)
    #[arg(short = 'o', long = "ssh-option")]
    ssh_option: Vec<String>,
    #[arg(short = 'l', long)]
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
    let t = parse_target(&cli.target).map_err(|e| (2, format!("term-scp: {e}")))?;
    // Before connecting: a bad -l must fail without an ssh login.
    let dir = cli.local.unwrap_or_else(|| ".".into());
    let local = std::fs::canonicalize(&dir)
        .map_err(|e| (1, format!("term-scp: {dir}: {e}")))?
        .to_string_lossy() // ponytail: lossy non-UTF-8 local names
        .into_owned();
    let rt = tokio::runtime::Runtime::new().map_err(|e| (1, format!("term-scp: {e}")))?;
    let opts = SshOpts {
        port: cli.port,
        identity: cli.identity,
        options: cli.ssh_option,
    };
    // Before raw mode: ssh's password, passphrase and host-key prompts need a normal terminal.
    let remote = rt
        .block_on(Remote::connect_ssh(&t.host, &opts))
        .map_err(|e| (1, format!("term-scp: cannot connect to {}:\n{e}", t.host)))?;
    let remote_cwd = rt
        .block_on(remote.sftp.canonicalize(remote_start(&t.path)))
        .map_err(|e| {
            (
                1,
                format!("term-scp: {}:{}: {}", t.host, t.path, sftp_err(e)),
            )
        })?;

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

    let left = Pane::new(Fs::Local, local.clone());
    let right = Pane::new(Fs::Remote(Arc::new(remote)), remote_cwd.clone());
    let mut app = App::new(left, right, !cli.no_icons);
    let mut ctl = Controller::new(rt.handle().clone(), tx);
    ctl.load(&mut app, Side::Left, local, None);
    ctl.load(&mut app, Side::Right, remote_cwd, None);

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
    fn remote_start_maps_home_forms() {
        assert_eq!(remote_start(""), ".");
        assert_eq!(remote_start("~"), ".");
        assert_eq!(remote_start("~/a/b"), "a/b");
        assert_eq!(remote_start("/abs"), "/abs");
        assert_eq!(remote_start("rel"), "rel");
    }
}
