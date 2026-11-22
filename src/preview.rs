use image::DynamicImage;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::Instant;

pub const LIMIT: u64 = 50 * 1024 * 1024;
pub const TOO_BIG: &str = "Too big to preview (over 50 MB).";
pub const NO_PREVIEW: &str = "No preview for this file type.";
pub const NEEDS_FFMPEG: &str = "Audio/video preview needs ffmpeg.";

#[derive(Debug, Default, PartialEq)]
pub struct Probe {
    pub duration: Option<f64>,
    pub video: Option<(String, u32, u32)>,
    pub audio: Option<(String, u32, u32)>,
    pub cover: bool,
}

impl Probe {
    pub fn info(&self) -> String {
        let mut parts = Vec::new();
        if let Some((c, w, h)) = &self.video {
            parts.push(format!("{c} {w}×{h}"));
        }
        if let Some((c, rate, ch)) = &self.audio {
            parts.push(format!("{c} {rate} Hz {ch} ch"));
        }
        if let Some(d) = self.duration {
            parts.push(fmt_secs(d));
        }
        parts.join(" · ")
    }
}

pub fn fmt_secs(s: f64) -> String {
    let s = s as u64;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// ffprobe `-of compact`: one `stream|key=value|…` line per stream, one `format|…` line.
pub fn parse_probe(out: &str) -> Option<Probe> {
    let (mut p, mut format, mut score) = (Probe::default(), "", 0);
    for line in out.lines() {
        let (section, rest) = line.split_once('|').unwrap_or((line, ""));
        let get = |k: &str| {
            rest.split('|')
                .find_map(|f| f.strip_prefix(k)?.strip_prefix('='))
        };
        let num = |k: &str| get(k).and_then(|v| v.parse().ok()).unwrap_or(0);
        let codec = || get("codec_name").unwrap_or("").to_string();
        match section {
            "format" => {
                format = get("format_name").unwrap_or("");
                score = num("probe_score");
                p.duration = get("duration").and_then(|d| d.parse().ok());
            }
            "stream" => {
                let cover = get("disposition:attached_pic") == Some("1");
                p.cover |= cover;
                match get("codec_type") {
                    Some("video") if !cover && p.video.is_none() => {
                        p.video = Some((codec(), num("width"), num("height")))
                    }
                    Some("audio") if p.audio.is_none() => {
                        p.audio = Some((codec(), num("sample_rate"), num("channels")))
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    // Not media: ffmpeg's still-image demuxers (a big picture), its text-art ones (picked by
    // extension: a big .txt, any .bin), and a weak guess (≤ 25, AVPROBE_SCORE_RETRY: an
    // ordinary binary read as raw mpeg4). Real media scores 26 and up.
    let still = matches!(format, "image2" | "tty" | "bin" | "xbin" | "adf" | "idf")
        || format.ends_with("_pipe");
    (!still && score > 25 && (p.video.is_some() || p.audio.is_some())).then_some(p)
}

#[derive(Debug, PartialEq)]
pub enum Kind {
    Text(Vec<String>),
    Image(String),
    Media(Probe),
    Message(String),
}

pub struct Loaded {
    pub kind: Kind,
    pub image: Option<DynamicImage>,
}

/// Caps what the UI thread resizes and encodes; never enlarges.
fn fit(img: DynamicImage) -> DynamicImage {
    if img.width().max(img.height()) > 1600 {
        img.thumbnail(1600, 1600)
    } else {
        img
    }
}

/// Image, text, broken image, by content alone. `None`: try the media check.
pub fn classify_bytes(bytes: &[u8]) -> Option<Loaded> {
    let decoded =
        image::guess_format(bytes).map(|f| (f, image::load_from_memory_with_format(bytes, f)));
    if let Ok((f, Ok(img))) = decoded {
        let info = format!(
            "{}×{} · {}",
            img.width(),
            img.height(),
            format!("{f:?}").to_uppercase()
        );
        return Some(Loaded {
            kind: Kind::Image(info),
            image: Some(fit(img)),
        });
    }
    let kind = if !bytes[..bytes.len().min(8192)].contains(&0) {
        Kind::Text(text_lines(bytes))
    } else if let Ok((_, Err(e))) = decoded {
        Kind::Message(format!("Can't decode image: {e}"))
    } else {
        return None;
    };
    Some(Loaded { kind, image: None })
}

pub fn text_lines(bytes: &[u8]) -> Vec<String> {
    // `lines` splits on \n, drops one \r before it and adds no empty last line.
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|l| l.replace('\t', "    ").replace(char::is_control, ""))
        .collect()
}

/// stdout is read; stdin and stderr stay off the terminal.
fn run(cmd: &mut Command) -> io::Result<Output> {
    cmd.stdin(Stdio::null()).stderr(Stdio::null()).output()
}

/// `Err` only when ffprobe can't be spawned.
pub fn probe(path: &Path) -> io::Result<Option<Probe>> {
    let out = run(Command::new("ffprobe")
        .args(["-v", "error", "-show_entries"])
        .arg("format=format_name,duration,probe_score:stream=codec_type,codec_name,width,height,sample_rate,channels:stream_disposition=attached_pic")
        .args(["-of", "compact"])
        .arg(path))?;
    Ok(out
        .status
        .success()
        .then(|| parse_probe(&String::from_utf8_lossy(&out.stdout)))
        .flatten())
}

/// A frame a little into a real video (so it isn't the black first one), or the cover art.
pub fn thumbnail(path: &Path, p: &Probe) -> Option<DynamicImage> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error"]);
    if p.video.is_some() {
        cmd.arg("-ss")
            .arg((p.duration.unwrap_or(0.0) / 10.0).to_string());
        cmd.arg("-i").arg(path);
    } else if p.cover {
        cmd.arg("-i").arg(path).arg("-an");
    } else {
        return None;
    }
    cmd.args(["-frames:v", "1", "-f", "image2pipe", "-c:v", "png", "-"]);
    image::load_from_memory(&run(&mut cmd).ok()?.stdout)
        .ok()
        .map(fit)
}

/// Blocking. The bytes are loaded only up to LIMIT; media is probed from disk either way.
pub fn load(path: &Path) -> Loaded {
    let msg = |m: String| Loaded {
        kind: Kind::Message(m),
        image: None,
    };
    // A FIFO, tty or device would block or read forever; transfer skips them too.
    // The size is fresh here: the listing's may be stale (a log that grew since).
    let read = match std::fs::metadata(path) {
        Err(e) => return msg(format!("Can't read: {e}")),
        Ok(m) if !m.is_file() => return msg(NO_PREVIEW.into()),
        Ok(m) => m.len() <= LIMIT,
    };
    if read {
        match std::fs::read(path) {
            Err(e) => return msg(format!("Can't read: {e}")),
            Ok(bytes) => {
                if let Some(l) = classify_bytes(&bytes) {
                    return l;
                }
            }
        }
    }
    match probe(path) {
        Ok(Some(p)) => Loaded {
            image: thumbnail(path, &p),
            kind: Kind::Media(p),
        },
        _ if !read => msg(TOO_BIG.into()),
        Err(_) => msg(format!("{NO_PREVIEW} {NEEDS_FFMPEG}")),
        Ok(None) => msg(NO_PREVIEW.into()),
    }
}

/// ffplay behind the popup; dropping it stops the sound.
pub struct Player {
    child: Child,
    pub started: Instant,
}

impl Player {
    pub fn spawn(path: &Path, p: &Probe, title: &str) -> io::Result<Player> {
        let mut cmd = Command::new("ffplay");
        cmd.args(["-autoexit", "-loglevel", "quiet", "-window_title", title]);
        if p.video.is_none() {
            cmd.arg("-nodisp");
        }
        let child = cmd
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Player {
            child,
            started: Instant::now(),
        })
    }

    pub fn finished(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }

    #[cfg(test)]
    pub fn id(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(w, h)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn classify_by_content() {
        let l = classify_bytes(&png(40, 30)).unwrap();
        assert!(matches!(&l.kind, Kind::Image(i) if i == "40×30 · PNG"));
        assert_eq!(
            l.image.as_ref().map(|i| (i.width(), i.height())),
            Some((40, 30))
        );

        let big = classify_bytes(&png(3200, 100)).unwrap();
        assert_eq!(big.image.map(|i| (i.width(), i.height())), Some((1600, 50)));

        assert!(matches!(classify_bytes(b"hello\r\nworld\n").unwrap().kind,
            Kind::Text(ref v) if v == &["hello", "world"]));
        assert!(matches!(
            classify_bytes(b"P1 is a text line").unwrap().kind,
            Kind::Text(_)
        ));
        let half = png(40, 30);
        assert!(
            matches!(classify_bytes(&half[..half.len() / 2]).unwrap().kind,
            Kind::Message(ref m) if m.starts_with("Can't decode image: "))
        );
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
                   format|format_name=mp3|duration=125.500000|probe_score=51\n";
        let p = parse_probe(mp3).unwrap();
        assert_eq!((&p.video, p.cover), (&None, true));
        assert_eq!(p.audio, Some(("mp3".into(), 44100, 2)));
        assert_eq!(p.info(), "mp3 44100 Hz 2 ch · 2:05");
        let mp4 = "stream|codec_name=h264|codec_type=video|width=320|height=240|disposition:attached_pic=0\n\
                   format|format_name=mov,mp4,m4a,3gp,3g2,mj2|duration=1.000000|probe_score=100\n";
        assert_eq!(parse_probe(mp4).unwrap().info(), "h264 320×240 · 0:01");
        let png = "stream|codec_name=png|codec_type=video|width=64|height=48|disposition:attached_pic=0\n\
                   format|format_name=png_pipe|duration=N/A|probe_score=99\n";
        assert_eq!(parse_probe(png), None);
        // A big .txt (ffmpeg's tty demuxer goes by extension) and an ordinary binary (a weak guess).
        let txt = "stream|codec_name=ansi|codec_type=video|width=640|height=400|disposition:attached_pic=0\n\
                   format|format_name=tty|duration=10490.68|probe_score=51\n";
        assert_eq!(parse_probe(txt), None);
        let bin = "stream|codec_name=mpeg4|codec_type=video|width=0|height=0|disposition:attached_pic=0\n\
                   format|format_name=m4v|duration=N/A|probe_score=5\n";
        assert_eq!(parse_probe(bin), None);
        assert_eq!(parse_probe(""), None);
        assert_eq!(fmt_secs(3723.9), "1:02:03");
    }

    fn has(tool: &str) -> bool {
        Command::new(tool).arg("-version").output().is_ok()
    }

    /// `ffmpeg -v error -y -f lavfi -i <src> <args…> <dir>/<name>`.
    fn lavfi(dir: &Path, src: &str, args: &[&str], name: &str) -> std::path::PathBuf {
        let out = dir.join(name);
        let ok = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", src])
            .args(args)
            .arg(&out)
            .status()
            .unwrap()
            .success();
        assert!(ok, "ffmpeg {src}");
        out
    }

    #[test]
    fn load_media() {
        if !has("ffprobe") {
            return eprintln!("skipped: no ffprobe");
        }
        let t = tempfile::tempdir().unwrap();
        let wav = lavfi(t.path(), "sine=frequency=440:duration=1", &[], "a.wav");
        let l = load(&wav);
        assert!(l.image.is_none());
        let Kind::Media(p) = l.kind else {
            panic!("{:?}", l.kind)
        };
        assert!(p.audio.is_some() && p.video.is_none());

        let src = "testsrc=duration=1:size=320x240:rate=10";
        let mp4 = lavfi(t.path(), src, &["-pix_fmt", "yuv420p"], "v.mp4");
        let l = load(&mp4);
        assert_eq!(l.image.map(|i| (i.width(), i.height())), Some((320, 240)));
        let Kind::Media(p) = l.kind else {
            panic!("{:?}", l.kind)
        };
        assert_eq!(p.video, Some(("h264".into(), 320, 240)));
    }

    #[test]
    fn load_limits() {
        let t = tempfile::tempdir().unwrap();
        let big = 51 * 1024 * 1024;
        let zeros = t.path().join("zeros.bin");
        std::fs::File::create(&zeros).unwrap().set_len(big).unwrap();
        assert_eq!(load(&zeros).kind, Kind::Message(TOO_BIG.into()));
        let img = t.path().join("p.png");
        std::fs::write(&img, png(4, 4)).unwrap();
        assert!(matches!(load(&img).kind, Kind::Image(_)));
        let gone = load(&t.path().join("gone")).kind;
        assert!(matches!(&gone, Kind::Message(m) if m.starts_with("Can't read: ")));
        let fifo = t.path().join("fifo");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(load(&fifo).kind, Kind::Message(NO_PREVIEW.into()));

        if !has("ffprobe") {
            return eprintln!("skipped: no ffprobe");
        }
        // A real WAV over LIMIT: zero padding past the data chunk makes ffprobe crawl for seconds.
        let wav = lavfi(t.path(), "sine=duration=600", &["-ac", "1"], "big.wav");
        assert!(std::fs::metadata(&wav).unwrap().len() > LIMIT);
        assert!(matches!(load(&wav).kind, Kind::Media(_)));
    }

    #[test]
    fn player_stops_on_drop() {
        if !has("ffprobe") || !has("ffplay") {
            return eprintln!("skipped: no ffplay");
        }
        let t = tempfile::tempdir().unwrap();
        // Silence, so `cargo test` never makes a sound; no video, so no window.
        let src = "anullsrc=r=44100:cl=mono";
        let wav = lavfi(t.path(), src, &["-t", "5"], "s.wav");
        let p = probe(&wav).unwrap().unwrap();
        let player = Player::spawn(&wav, &p, "test").unwrap();
        let pid = player.id().to_string();
        let t0 = Instant::now();
        drop(player);
        // Killed, not waited out: the clip is 5 s.
        assert!(t0.elapsed() < std::time::Duration::from_secs(2));
        let alive = Command::new("kill")
            .args(["-0", &pid])
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!alive.success());
    }
}
