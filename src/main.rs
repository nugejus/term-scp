#![allow(dead_code)] // ponytail: until Task 10 wires main
mod fs;
mod icons;
use clap::Parser;

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

fn main() {
    let _cli = Cli::parse();
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
