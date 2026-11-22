# Preview

`Enter` or a double-click on a file opens a popup that shows what it is, so you can tell before copying it. `Esc` or a click on the `✕` at its top right closes it. What a file is goes by its bytes, not its name.
- Text: the lines, cut at the popup's width; `↑` `↓` `PgUp` `PgDn` `Home` `End` and the wheel scroll.
- Images (PNG, JPEG, GIF, WebP, …): the picture, its size and format. Ghostty, kitty, iTerm2, WezTerm and sixel terminals show the real picture; other terminals show it in half blocks.
- Audio and video: the duration, codecs, resolution or sample rate, and a frame of the video or its cover art. `Space` plays it with `ffplay` and `Space` again stops it. Audio plays behind the popup; a video opens in ffplay's own window. Closing the popup stops it.
- Anything else: `No preview for this file type.`

A file on the host is downloaded to a temp file first, with its progress shown, and the temp file is removed when the popup closes. Files over 50 MB on the host aren't downloaded, and local text and images over 50 MB aren't read: both show `Too big to preview (over 50 MB).` Local audio and video play at any size.

`ffmpeg` is optional (`brew install ffmpeg`, `apt install ffmpeg`). Without it, text and images still preview; audio and video show `Audio/video preview needs ffmpeg.`
