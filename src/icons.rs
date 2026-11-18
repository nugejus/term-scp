use ratatui::style::Color;

/// Nerd Font glyph + colour for a file name, by lowercase extension.
pub fn icon(name: &str, is_dir: bool) -> (&'static str, Color) {
    if is_dir {
        return ("\u{f07b}", Color::LightBlue);
    }
    let lower = name.to_lowercase();
    let ext = lower.rsplit_once('.').map_or("", |(_, e)| e);
    match ext {
        "mp4" | "mkv" | "mov" | "avi" | "webm" => ("\u{f03d}", Color::Magenta),
        "mp3" | "wav" | "flac" | "ogg" | "m4a" => ("\u{f001}", Color::LightMagenta),
        "csv" | "tsv" => ("\u{f1c3}", Color::Green),
        "txt" | "log" => ("\u{f15c}", Color::Gray),
        "py" => ("\u{e73c}", Color::Yellow),
        "rs" => ("\u{e7a8}", Color::LightRed),
        "c" | "h" => ("\u{e61e}", Color::Blue),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => ("\u{e61d}", Color::LightBlue),
        "ts" | "tsx" => ("\u{e628}", Color::Cyan),
        "go" => ("\u{e626}", Color::LightCyan),
        "js" | "jsx" | "mjs" | "cjs" => ("\u{e74e}", Color::LightYellow),
        "java" => ("\u{e738}", Color::Red),
        "md" => ("\u{e73e}", Color::White),
        "toml" => ("\u{e6b2}", Color::LightRed),
        "yaml" | "yml" => ("\u{e6a8}", Color::LightGreen),
        "json" => ("\u{e60b}", Color::Yellow),
        "conf" | "cfg" | "ini" | "env" => ("\u{e615}", Color::DarkGray),
        "html" | "htm" => ("\u{e736}", Color::Rgb(255, 135, 0)),
        "j2" => ("\u{e66f}", Color::Rgb(180, 80, 80)),
        "sh" | "bash" | "zsh" => ("\u{f489}", Color::Green),
        _ if matches!(
            name,
            ".gitignore" | ".editorconfig" | "Dockerfile" | "Makefile"
        ) =>
        {
            ("\u{e615}", Color::DarkGray)
        }
        _ => ("\u{f016}", Color::Gray),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_category_distinct() {
        let names = [
            "x.mp4", "x.mp3", "x.csv", "x.txt", "x.py", "x.rs", "x.c", "x.cpp", "x.ts", "x.go",
            "x.js", "x.java", "x.md", "x.toml", "x.yaml", "x.json", "x.conf", "x.html", "x.j2",
            "x.sh", "x.bin",
        ];
        let mut set: HashSet<&str> = names.iter().map(|n| icon(n, false).0).collect();
        set.insert(icon("d", true).0);
        assert_eq!(set.len(), 22);
    }

    #[test]
    fn aliases_and_case() {
        assert_eq!(icon("A.PY", false), icon("a.py", false));
        assert_eq!(icon("x.tsv", false), icon("x.csv", false));
        assert_eq!(icon("Makefile", false), icon("x.conf", false));
        assert_ne!(icon("src", true), icon("src", false));
    }
}
