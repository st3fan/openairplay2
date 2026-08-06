//! Cover art on terminals that can draw pixels.
//!
//! Two protocols cover the terminals people actually use: the **Kitty
//! graphics protocol** (Ghostty, Kitty, WezTerm) and **iTerm2 inline images**
//! (iTerm2, WezTerm, Konsole). Everything else gets no image and the display
//! falls back to text, which is why [`detect`] must fail *closed* — emitting
//! graphics escapes at a terminal that doesn't understand them dumps
//! base64 across the user's screen.
//!
//! The two protocols differ in what they accept: iTerm2 takes the image file
//! as-is (JPEG included), Kitty takes PNG or raw pixels, so a JPEG is decoded
//! here first. Both scale the image into a box measured in cells, so nothing
//! is resampled.
//!
//! Inside **tmux** none of that reaches the terminal unaided: tmux forwards an
//! escape sequence only when it is wrapped in its DCS passthrough envelope, so
//! [`Graphics`] carries tmux-ness alongside the protocol and wraps what it
//! emits. See [`passthrough`].
//!
//! Everything in this module is a pure function over bytes and placement,
//! except [`probe_kitty`] and [`cell_size`], which touch the terminal.

use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use log::debug;

/// Which terminal-graphics protocol to speak, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Protocol {
    /// Kitty graphics (`APC _G … ST`): Ghostty, Kitty, WezTerm.
    Kitty,
    /// iTerm2 inline images (`OSC 1337 File=…`): iTerm2, WezTerm, Konsole.
    ITerm2,
    /// No image support — text only.
    #[default]
    None,
}

impl Protocol {
    /// Parse the `--tui-images` argument.
    pub fn parse(value: &str) -> Option<Protocol> {
        match value {
            "kitty" => Some(Protocol::Kitty),
            "iterm2" => Some(Protocol::ITerm2),
            "none" => Some(Protocol::None),
            _ => None,
        }
    }
}

/// What this terminal can draw, and how the bytes have to travel to reach it:
/// a protocol plus whether we are inside tmux.
///
/// The two always travel together — a protocol whose escapes are not wrapped
/// for tmux is as good as no protocol at all — so they are one value rather
/// than two arguments that could disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Graphics {
    protocol: Protocol,
    /// Wrap every escape in tmux's passthrough envelope.
    tmux: bool,
}

impl Graphics {
    pub fn new(protocol: Protocol, tmux: bool) -> Graphics {
        Graphics { protocol, tmux }
    }

    /// What [`detect`] makes of the environment, carrying the tmux fact.
    pub fn detect(
        env: impl Fn(&str) -> Option<String>,
        probe: Option<bool>,
        tmux: bool,
    ) -> Graphics {
        Graphics::new(detect(env, probe), tmux)
    }

    /// Can this terminal draw an image at all? The artwork box is only worth
    /// reserving if a picture will land in it.
    pub fn draws(self) -> bool {
        self.protocol != Protocol::None
    }

    /// The escape sequence that draws `image` at `placement`, or `None` if the
    /// terminal can't draw images or the payload isn't usable.
    pub fn draw(self, content_type: &str, image: &[u8], placement: Placement) -> Option<Vec<u8>> {
        if placement.cols == 0 || placement.rows == 0 || image.is_empty() {
            return None;
        }
        match self.protocol {
            Protocol::Kitty => kitty_draw(content_type, image, placement, self.tmux),
            Protocol::ITerm2 => Some(iterm2_draw(image, placement, self.tmux)),
            Protocol::None => None,
        }
    }

    /// The escape sequence that removes a previously drawn image. iTerm2
    /// images are part of the text grid and disappear when the cells are
    /// redrawn, so only Kitty needs an explicit delete.
    pub fn clear(self) -> Option<Vec<u8>> {
        match self.protocol {
            Protocol::Kitty => Some(escape(
                format!("\x1b_Ga=d,d=I,i={ARTWORK_ID}\x1b\\").into_bytes(),
                self.tmux,
            )),
            Protocol::ITerm2 | Protocol::None => None,
        }
    }
}

impl fmt::Display for Graphics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let protocol = match self.protocol {
            Protocol::Kitty => "kitty",
            Protocol::ITerm2 => "iterm2",
            Protocol::None => "none",
        };
        f.write_str(protocol)?;
        if self.tmux {
            f.write_str(" (wrapped for tmux)")?;
        }
        Ok(())
    }
}

/// Are we inside tmux?
///
/// `$TMUX` is the signal that matters: `TERM` is whatever `default-terminal`
/// says, which on this project's own workstation is `xterm-256color` and names
/// no multiplexer at all. The `TERM` prefixes are a backstop for a pane that
/// inherited an environment without `$TMUX`.
///
/// `screen` is included deliberately even though GNU screen's passthrough
/// envelope is *not* tmux's: under real screen the wrapped sequence is
/// discarded and the user gets no artwork, which is the right way to be wrong.
pub fn under_tmux(env: impl Fn(&str) -> Option<String>) -> bool {
    let var = |name: &str| env(name).filter(|v| !v.is_empty());
    var("TMUX").is_some()
        || var("TERM").is_some_and(|term| term.starts_with("tmux") || term.starts_with("screen"))
}

/// Wrap one escape sequence in tmux's DCS passthrough envelope: `ESC P tmux ;`,
/// the sequence with **every** `ESC` inside it doubled, then `ESC \`.
///
/// tmux forwards nothing else — an unwrapped graphics escape is swallowed,
/// which is why forcing the protocol cannot work around a tmux that was never
/// detected. It also requires `set -g allow-passthrough on`, which is the
/// user's half and cannot be arranged from here.
fn passthrough(inner: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(inner.len() + 16);
    out.extend_from_slice(b"\x1bPtmux;");
    for &byte in inner {
        if byte == 0x1b {
            out.push(0x1b);
        }
        out.push(byte);
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

/// One escape sequence, wrapped for tmux when we are inside one.
fn escape(bytes: Vec<u8>, tmux: bool) -> Vec<u8> {
    if tmux {
        passthrough(&bytes)
    } else {
        bytes
    }
}

/// Decide from the environment which protocol a terminal speaks.
///
/// `probe` is the result of [`probe_kitty`] when one was run: `Some(true)`
/// settles it outright. The environment lookups are a fallback for terminals
/// that don't answer the query (and for pipes, where no probe is possible).
///
/// Inside tmux the table needs no new rules — tmux's own `TERM` and
/// `TERM_PROGRAM` (`tmux`) match nothing here — but it does get quieter: all
/// that is left is whatever leaked into the tmux server's environment. The
/// probe, wrapped for passthrough, is what actually sees out of a pane.
///
/// Pure so every terminal in the table is a test case; `env` is any lookup,
/// which is [`std::env::var`] in practice.
fn detect(env: impl Fn(&str) -> Option<String>, probe: Option<bool>) -> Protocol {
    if probe == Some(true) {
        return Protocol::Kitty;
    }
    let var = |name: &str| env(name).filter(|v| !v.is_empty());
    let term = var("TERM").unwrap_or_default();
    let program = var("TERM_PROGRAM").unwrap_or_default();

    // Kitty graphics. Ghostty reports itself both ways depending on how the
    // terminfo was installed, so check TERM and TERM_PROGRAM.
    if term.contains("kitty")
        || term.contains("ghostty")
        || var("KITTY_WINDOW_ID").is_some()
        || var("GHOSTTY_RESOURCES_DIR").is_some()
        || var("GHOSTTY_BIN_DIR").is_some()
        || program.eq_ignore_ascii_case("ghostty")
        || program.eq_ignore_ascii_case("kitty")
        // WezTerm speaks both; prefer Kitty, which needs no cursor dance.
        || program.eq_ignore_ascii_case("WezTerm")
    {
        return Protocol::Kitty;
    }

    if program.eq_ignore_ascii_case("iTerm.app")
        || var("LC_TERMINAL").is_some_and(|v| v.eq_ignore_ascii_case("iTerm2"))
        || var("KONSOLE_VERSION").is_some()
    {
        return Protocol::ITerm2;
    }

    Protocol::None
}

/// Ask the terminal whether it speaks the Kitty graphics protocol, by
/// transmitting a 1×1 image with `a=q` and waiting briefly for the reply.
///
/// Must run **before** the TUI event loop takes the terminal over, and
/// returns `None` when the answer is inconclusive (not a terminal, or no
/// reply at all) so [`detect`] can fall back to the environment.
///
/// The reply is read straight from the file descriptor rather than through
/// crossterm's event machinery: `crossterm::event::poll` parses whatever is
/// pending into its own buffer, so a subsequent read of stdin finds nothing
/// on the fd and blocks until the next keystroke. That is not hypothetical —
/// it made the display render nothing until a key was pressed, on exactly
/// the terminals that answer the query.
pub fn probe_kitty(timeout: Duration, tmux: bool) -> Option<bool> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        return None;
    }
    let _raw = RawMode::enable().ok()?;

    let mut stdout = io::stdout();
    stdout.write_all(&probe_query(tmux)).ok()?;
    stdout.flush().ok()?;

    let deadline = Instant::now() + timeout;
    let mut reply = Vec::new();
    while let Some(byte) = read_byte_before(deadline) {
        reply.push(byte);
        if ends_with_device_attributes(&reply) {
            break;
        }
    }
    debug!(
        "kitty graphics probe reply: {:?}",
        String::from_utf8_lossy(&reply)
    );
    if reply.is_empty() {
        return None; // said nothing at all: let the environment decide
    }
    Some(kitty_supported(&reply))
}

/// What the probe writes: the graphics query, then a Device Attributes
/// request. Every terminal answers DA1, so its reply marks the end of the
/// answer — without it we would wait out the whole timeout on terminals that
/// ignore the first query, which is a visible pause before the display appears.
///
/// Under tmux **both** go inside one passthrough envelope. Leaving the DA1
/// request outside would race: tmux answers it itself, immediately, and the
/// outer terminal's `OK` would arrive after we had already stopped listening.
/// Answered by the same terminal, in order, DA1 still means "nothing more is
/// coming" — and if tmux forwards none of it, the probe reads nothing, returns
/// `None`, and the environment decides as before.
fn probe_query(tmux: bool) -> Vec<u8> {
    let query = format!(
        "\x1b_Gi={PROBE_ID},s=1,v=1,a=q,t=d,f=24;{}\x1b\\\x1b[c",
        STANDARD.encode([0u8, 0, 0])
    );
    escape(query.into_bytes(), tmux)
}

/// Does this reply contain the graphics protocol's `OK` for our query?
fn kitty_supported(reply: &[u8]) -> bool {
    let reply = String::from_utf8_lossy(reply);
    reply
        .split("\x1b_G")
        .skip(1)
        .any(|answer| answer.contains(&format!("i={PROBE_ID}")) && answer.contains("OK"))
}

/// Has the Device Attributes answer (`ESC [ ? … c`) arrived? That is the end
/// of everything the terminal has to say about our queries.
fn ends_with_device_attributes(reply: &[u8]) -> bool {
    if !reply.ends_with(b"c") {
        return false;
    }
    let reply = String::from_utf8_lossy(reply);
    reply
        .rfind("\x1b[?")
        .is_some_and(|start| !reply[start..].contains("\x1b\\"))
}

/// Read one byte from the terminal, giving up at `deadline`. Uses `poll(2)`
/// on the file descriptor directly — see [`probe_kitty`] for why not
/// crossterm.
fn read_byte_before(deadline: Instant) -> Option<u8> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return None;
    }
    let mut fds = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one initialized pollfd, and the count matches.
    let ready = unsafe { libc::poll(&mut fds, 1, remaining.as_millis() as libc::c_int) };
    if ready <= 0 {
        return None;
    }
    let mut byte = 0u8;
    // SAFETY: reading one byte into a byte we own.
    let read = unsafe {
        libc::read(
            libc::STDIN_FILENO,
            std::ptr::addr_of_mut!(byte).cast::<libc::c_void>(),
            1,
        )
    };
    (read == 1).then_some(byte)
}

/// Image id for the probe, distinct from the one the display uses.
const PROBE_ID: u32 = 7331;
/// Image id the now-playing artwork lives under.
const ARTWORK_ID: u32 = 7332;
/// Kitty wants base64 payloads split into chunks of at most 4096 bytes.
const CHUNK: usize = 4096;

/// Where an image goes: a box in character cells, 1-based like the cursor
/// positioning escape itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub col: u16,
    pub row: u16,
    pub cols: u16,
    pub rows: u16,
}

/// Kitty: PNG goes over as-is (`f=100`); anything else is decoded to RGB
/// (`f=24`), which in practice means the JPEG every sender sends.
///
/// Each chunk is its own escape and so gets its own tmux envelope, which keeps
/// every passthrough small. The cursor move in front of them is deliberately
/// *not* wrapped: its coordinates are the pane's, and only tmux can translate
/// those — passed through, the artwork would land wherever those coordinates
/// happen to point on the outer terminal's screen.
fn kitty_draw(
    content_type: &str,
    image: &[u8],
    placement: Placement,
    tmux: bool,
) -> Option<Vec<u8>> {
    let (format_keys, payload) = if is_png(content_type, image) {
        ("f=100".to_string(), image.to_vec())
    } else {
        let rgb = decode_jpeg(image)?;
        (format!("f=24,s={},v={}", rgb.width, rgb.height), rgb.pixels)
    };

    let mut out = cursor_to(placement);
    let encoded = STANDARD.encode(&payload);
    let mut chunks = encoded.as_bytes().chunks(CHUNK).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = u8::from(chunks.peek().is_some());
        let mut unit = if first {
            // a=T transmit and display, C=1 leave the cursor alone, q=2 stay
            // quiet, c/r scale the image into the box we laid out.
            first = false;
            format!(
                "\x1b_Ga=T,i={ARTWORK_ID},{format_keys},c={},r={},C=1,q=2,m={more};",
                placement.cols, placement.rows
            )
            .into_bytes()
        } else {
            format!("\x1b_Gm={more};").into_bytes()
        };
        unit.extend_from_slice(chunk);
        unit.extend_from_slice(b"\x1b\\");
        out.extend_from_slice(&escape(unit, tmux));
    }
    Some(out)
}

/// iTerm2: the file bytes as-is, whatever the format, scaled into the box.
///
/// One OSC carries the whole image, so under tmux it becomes a single large
/// passthrough — fine for the artwork senders actually send, and the only
/// place where a truly enormous image could outgrow tmux's buffer.
fn iterm2_draw(image: &[u8], placement: Placement, tmux: bool) -> Vec<u8> {
    let mut unit = format!(
        "\x1b]1337;File=inline=1;size={};width={};height={};preserveAspectRatio=1;\
         doNotMoveCursor=1:",
        image.len(),
        placement.cols,
        placement.rows
    )
    .into_bytes();
    unit.extend_from_slice(STANDARD.encode(image).as_bytes());
    unit.push(0x07); // BEL terminates OSC 1337

    // The cursor move stays outside the envelope — see kitty_draw.
    let mut out = cursor_to(placement);
    out.extend_from_slice(&escape(unit, tmux));
    out
}

/// Park the cursor at the top-left of the box before drawing.
fn cursor_to(placement: Placement) -> Vec<u8> {
    format!("\x1b[{};{}H", placement.row, placement.col).into_bytes()
}

fn is_png(content_type: &str, image: &[u8]) -> bool {
    content_type.eq_ignore_ascii_case("image/png") || image.starts_with(b"\x89PNG\r\n\x1a\n")
}

/// Decoded pixels, always three bytes per pixel — what Kitty's `f=24` wants.
struct Rgb {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// Decode a JPEG to RGB. Grayscale is expanded; anything unsupported (or
/// corrupt) returns `None` and the display simply shows no artwork.
fn decode_jpeg(image: &[u8]) -> Option<Rgb> {
    use jpeg_decoder::PixelFormat;

    let mut decoder = jpeg_decoder::Decoder::new(image);
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let pixels = match info.pixel_format {
        PixelFormat::RGB24 => pixels,
        PixelFormat::L8 => pixels.iter().flat_map(|&v| [v, v, v]).collect(),
        other => {
            debug!("artwork: unsupported JPEG pixel format {other:?}");
            return None;
        }
    };
    Some(Rgb {
        width: info.width as u32,
        height: info.height as u32,
        pixels,
    })
}

/// The terminal's cell size in pixels, as `(width, height)`, when the
/// terminal reports one. Used to keep square cover art square.
pub fn cell_size() -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: ws is a valid winsize for the kernel to fill in.
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if rc != 0 || ws.ws_xpixel == 0 || ws.ws_ypixel == 0 || ws.ws_col == 0 || ws.ws_row == 0 {
        return None;
    }
    Some((ws.ws_xpixel / ws.ws_col, ws.ws_ypixel / ws.ws_row))
}

/// How many times taller than wide a cell is; the fallback matches the usual
/// terminal font.
pub const DEFAULT_CELL_ASPECT: f32 = 2.0;

/// Cell aspect (height ÷ width) from the terminal, or the default.
pub fn cell_aspect() -> f32 {
    match cell_size() {
        Some((w, h)) if w > 0 => h as f32 / w as f32,
        _ => DEFAULT_CELL_ASPECT,
    }
}

/// Raw mode just for the probe: the reply must not be line-buffered or
/// echoed. Restores the previous settings on drop.
struct RawMode;

impl RawMode {
    fn enable() -> io::Result<RawMode> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(RawMode)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn detects_kitty_family_terminals() {
        for env in [
            vec![("TERM", "xterm-kitty")],
            vec![("TERM", "xterm-ghostty")],
            vec![("TERM", "xterm-256color"), ("TERM_PROGRAM", "ghostty")],
            vec![("TERM", "xterm-256color"), ("KITTY_WINDOW_ID", "1")],
            vec![
                ("TERM", "xterm-256color"),
                ("GHOSTTY_RESOURCES_DIR", "/usr/share/ghostty"),
            ],
            vec![("TERM", "xterm-256color"), ("TERM_PROGRAM", "WezTerm")],
        ] {
            assert_eq!(
                detect(env_of(&env), None),
                Protocol::Kitty,
                "should be Kitty: {env:?}"
            );
        }
    }

    #[test]
    fn detects_iterm2_family_terminals() {
        for env in [
            vec![("TERM_PROGRAM", "iTerm.app")],
            vec![("LC_TERMINAL", "iTerm2")],
            vec![("KONSOLE_VERSION", "220400")],
        ] {
            assert_eq!(
                detect(env_of(&env), None),
                Protocol::ITerm2,
                "should be iTerm2: {env:?}"
            );
        }
    }

    #[test]
    fn unknown_terminals_get_no_images() {
        // Failing closed matters: escapes at a terminal that can't read them
        // spray base64 over the screen.
        for env in [
            vec![("TERM", "xterm-256color")],
            vec![("TERM", "screen")],
            vec![("TERM", "dumb")],
            vec![],
        ] {
            assert_eq!(
                detect(env_of(&env), None),
                Protocol::None,
                "should be None: {env:?}"
            );
        }
    }

    #[test]
    fn a_kitty_reply_is_recognized_and_anything_else_is_not() {
        // What Ghostty/Kitty answer, followed by the DA1 reply.
        let ok = b"\x1b_Gi=7331;OK\x1b\\\x1b[?62;c";
        assert!(kitty_supported(ok));
        // A terminal that only answers DA1 has no graphics support.
        assert!(!kitty_supported(b"\x1b[?62;c"));
        // An error answer is not support either.
        assert!(!kitty_supported(b"\x1b_Gi=7331;ENOTSUPPORTED\x1b\\"));
        // Somebody else's image id is not our answer.
        assert!(!kitty_supported(b"\x1b_Gi=99;OK\x1b\\"));
        assert!(!kitty_supported(b""));
    }

    #[test]
    fn the_device_attributes_reply_ends_the_wait() {
        // Without this the probe waits out its whole timeout on every
        // terminal that ignores the graphics query — a visible pause before
        // the display appears.
        assert!(ends_with_device_attributes(b"\x1b[?62;22c"));
        assert!(ends_with_device_attributes(
            b"\x1b_Gi=7331;OK\x1b\\\x1b[?62;22c"
        ));
        // Partial replies keep the loop going.
        assert!(!ends_with_device_attributes(b"\x1b[?62;22"));
        assert!(!ends_with_device_attributes(b"\x1b_Gi=7331;OK\x1b\\"));
        // A 'c' inside a graphics answer is not the DA1 terminator.
        assert!(!ends_with_device_attributes(b"\x1b_Gi=7331;abc\x1b\\"));
        assert!(!ends_with_device_attributes(b""));
    }

    #[test]
    fn a_positive_probe_settles_it() {
        // Even a terminal we'd otherwise write off: it answered the query.
        assert_eq!(
            detect(env_of(&[("TERM", "xterm-256color")]), Some(true)),
            Protocol::Kitty
        );
        // A negative probe is not proof — some terminals stay silent — so
        // the environment still decides.
        assert_eq!(
            detect(env_of(&[("TERM", "xterm-kitty")]), Some(false)),
            Protocol::Kitty
        );
    }

    #[test]
    fn tmux_is_recognized_by_its_own_variable_first() {
        // TERM is whatever default-terminal says; on this project's own
        // workstation that is xterm-256color, which names no multiplexer.
        for env in [
            vec![("TMUX", "/tmp/tmux-1000/default,1234,0")],
            vec![
                ("TMUX", "/tmp/tmux-1000/default,1234,0"),
                ("TERM", "xterm-256color"),
            ],
            vec![("TERM", "tmux-256color")],
            vec![("TERM", "screen-256color")],
        ] {
            assert!(under_tmux(env_of(&env)), "should be tmux: {env:?}");
        }
        for env in [
            vec![("TERM", "xterm-kitty")],
            vec![("TERM", "xterm-256color")],
            vec![("TMUX", "")], // unset, in the shape env(3) hands it over
            vec![],
        ] {
            assert!(!under_tmux(env_of(&env)), "should not be tmux: {env:?}");
        }
    }

    #[test]
    fn the_passthrough_envelope_doubles_every_escape_inside_it() {
        // The delete escape has one ESC at each end, so it catches both a
        // missed doubling and a doubled terminator.
        assert_eq!(
            String::from_utf8(passthrough(b"\x1b_Ga=d,d=I,i=7332\x1b\\")).unwrap(),
            "\x1bPtmux;\x1b\x1b_Ga=d,d=I,i=7332\x1b\x1b\\\x1b\\"
        );
        assert_eq!(
            String::from_utf8(passthrough(b"plain")).unwrap(),
            "\x1bPtmux;plain\x1b\\"
        );
    }

    /// Undo [`passthrough`] wherever it appears in a byte stream, leaving
    /// everything outside an envelope alone. The tmux path is only correct if
    /// it is the plain path plus envelopes, and this is what proves it.
    fn unwrap_passthrough(bytes: &[u8]) -> Vec<u8> {
        const OPEN: &[u8] = b"\x1bPtmux;";
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if !bytes[i..].starts_with(OPEN) {
                out.push(bytes[i]);
                i += 1;
                continue;
            }
            i += OPEN.len();
            loop {
                assert!(i < bytes.len(), "unterminated envelope");
                match (bytes[i], bytes.get(i + 1)) {
                    (0x1b, Some(0x1b)) => {
                        out.push(0x1b); // a doubled ESC is one ESC of content
                        i += 2;
                    }
                    (0x1b, Some(b'\\')) => {
                        i += 2; // the envelope's own terminator
                        break;
                    }
                    (0x1b, other) => panic!("undoubled ESC inside envelope, before {other:?}"),
                    (byte, _) => {
                        out.push(byte);
                        i += 1;
                    }
                }
            }
        }
        out
    }

    fn placement() -> Placement {
        Placement {
            col: 5,
            row: 3,
            cols: 20,
            rows: 10,
        }
    }

    fn kitty(tmux: bool) -> Graphics {
        Graphics::new(Protocol::Kitty, tmux)
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake";

    #[test]
    fn kitty_transmits_png_untouched() {
        let out = kitty(false).draw("image/png", PNG, placement()).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("\x1b[3;5H"), "cursor first: {text:?}");
        assert!(text.contains("\x1b_Ga=T,i=7332,f=100,c=20,r=10,C=1,q=2,m=0;"));
        assert!(text.contains(&STANDARD.encode(PNG)), "payload as-is");
        assert!(text.ends_with("\x1b\\"));
    }

    /// A PNG big enough to need several chunks.
    fn big_png() -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend(std::iter::repeat_n(0xab, 8 * 1024));
        v
    }

    #[test]
    fn kitty_chunks_large_payloads() {
        // Two chunks: the first says m=1 (more coming), the last m=0.
        let big = big_png();
        let out = kitty(false).draw("image/png", &big, placement()).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains(",m=1;"), "first chunk continues");
        assert!(text.contains("\x1b_Gm=0;"), "last chunk ends the transfer");
        let payload: String = text
            .split("\x1b\\")
            // Only the graphics escapes carry payload; the cursor move that
            // precedes the first one has a ';' of its own.
            .filter_map(|part| part.rsplit_once("\x1b_G").map(|(_, cmd)| cmd))
            .filter_map(|cmd| cmd.split_once(';').map(|(_, data)| data))
            .collect();
        assert_eq!(payload, STANDARD.encode(&big), "chunks must reassemble");
    }

    /// An 8×8 JPEG and the same image as PNG, so the decode path is exercised
    /// with something a decoder actually accepts.
    const JPEG: &[u8] = include_bytes!("../tests/data/tiny.jpg");
    const REAL_PNG: &[u8] = include_bytes!("../tests/data/tiny.png");

    #[test]
    fn kitty_decodes_jpeg_to_raw_pixels() {
        // Kitty takes PNG or pixels, never JPEG — senders send JPEG.
        let out = kitty(false).draw("image/jpeg", JPEG, placement()).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("f=24,s=8,v=8,"),
            "must declare RGB and the decoded size: {}",
            &text[..80.min(text.len())]
        );
        let payload: Vec<u8> = STANDARD
            .decode(
                text.split("\x1b\\")
                    .filter_map(|part| part.rsplit_once("\x1b_G").map(|(_, cmd)| cmd))
                    .filter_map(|cmd| cmd.split_once(';').map(|(_, data)| data))
                    .collect::<String>(),
            )
            .unwrap();
        assert_eq!(payload.len(), 8 * 8 * 3, "three bytes per pixel");
    }

    #[test]
    fn kitty_sends_a_real_png_without_decoding_it() {
        let out = kitty(false)
            .draw("image/png", REAL_PNG, placement())
            .unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("f=100,"), "PNG goes over as a file");
        assert!(text.contains(&STANDARD.encode(REAL_PNG)));
    }

    #[test]
    fn png_is_recognized_by_its_magic_even_if_mislabelled() {
        // Some senders label everything image/jpeg; the bytes decide.
        let out = kitty(false)
            .draw("image/jpeg", REAL_PNG, placement())
            .unwrap();
        assert!(String::from_utf8_lossy(&out).contains("f=100,"));
    }

    #[test]
    fn kitty_drops_artwork_it_cannot_decode() {
        // Not a PNG and not a decodable JPEG: no escape at all, rather than
        // a half-written image.
        assert!(kitty(false)
            .draw("image/jpeg", b"not a jpeg", placement())
            .is_none());
    }

    #[test]
    fn iterm2_sends_the_file_as_is() {
        let jpeg = b"\xff\xd8\xff\xe0 pretend jpeg";
        let out = Graphics::new(Protocol::ITerm2, false)
            .draw("image/jpeg", jpeg, placement())
            .unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("\x1b[3;5H"));
        assert!(text.contains(&format!(
            "\x1b]1337;File=inline=1;size={};width=20;height=10;preserveAspectRatio=1;\
             doNotMoveCursor=1:",
            jpeg.len()
        )));
        assert!(text.contains(&STANDARD.encode(jpeg)));
        assert!(text.ends_with('\u{7}'), "OSC ends with BEL");
    }

    #[test]
    fn nothing_is_drawn_without_a_protocol_or_a_box() {
        assert!(Graphics::new(Protocol::None, false)
            .draw("image/png", PNG, placement())
            .is_none());
        let empty = Placement {
            cols: 0,
            ..placement()
        };
        assert!(kitty(false).draw("image/png", PNG, empty).is_none());
        assert!(kitty(false).draw("image/png", b"", placement()).is_none());
    }

    #[test]
    fn only_kitty_needs_an_explicit_delete() {
        assert_eq!(
            kitty(false).clear().map(String::from_utf8),
            Some(Ok("\x1b_Ga=d,d=I,i=7332\x1b\\".to_string()))
        );
        assert!(Graphics::new(Protocol::ITerm2, false).clear().is_none());
        assert!(Graphics::new(Protocol::None, false).clear().is_none());
    }

    #[test]
    fn under_tmux_the_escapes_are_the_same_bytes_in_envelopes() {
        // The whole tmux path in one assertion: unwrap the envelopes and what
        // is left must be exactly what a bare terminal gets. Anything else —
        // a chunk boundary moved, a keyword dropped — shows up here.
        for (content_type, image) in [
            ("image/png", REAL_PNG.to_vec()),
            ("image/jpeg", JPEG.to_vec()),
            ("image/png", big_png()), // several chunks, several envelopes
        ] {
            let plain = kitty(false)
                .draw(content_type, &image, placement())
                .unwrap();
            let wrapped = kitty(true).draw(content_type, &image, placement()).unwrap();
            assert_ne!(wrapped, plain, "tmux must change the bytes");
            assert_eq!(unwrap_passthrough(&wrapped), plain);
        }

        let jpeg = b"\xff\xd8\xff\xe0 pretend jpeg";
        let plain = Graphics::new(Protocol::ITerm2, false)
            .draw("image/jpeg", jpeg, placement())
            .unwrap();
        let wrapped = Graphics::new(Protocol::ITerm2, true)
            .draw("image/jpeg", jpeg, placement())
            .unwrap();
        assert_ne!(wrapped, plain);
        assert_eq!(unwrap_passthrough(&wrapped), plain);
    }

    #[test]
    fn the_cursor_move_stays_outside_the_envelope() {
        // Its coordinates are the pane's; only tmux can translate them. Passed
        // through, the artwork would land somewhere else entirely.
        let big = big_png();
        let out = kitty(true).draw("image/png", &big, placement()).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.starts_with("\x1b[3;5H\x1bPtmux;"),
            "cursor move first, then the envelope: {:?}",
            &text[..40.min(text.len())]
        );
        // One envelope per chunk, not one around the whole drawing.
        let envelopes = text.matches("\x1bPtmux;").count();
        assert!(envelopes > 1, "each chunk is wrapped: {envelopes}");
        // One graphics escape per chunk, one envelope around each.
        assert_eq!(envelopes, text.matches("\x1b_G").count());

        let out = Graphics::new(Protocol::ITerm2, true)
            .draw("image/jpeg", b"\xff\xd8\xff\xe0 pretend jpeg", placement())
            .unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("\x1b[3;5H\x1bPtmux;"));
        assert_eq!(
            text.matches("\x1bPtmux;").count(),
            1,
            "one OSC, one envelope"
        );
        assert!(text.ends_with("\x1b\\"), "the envelope terminates it");
    }

    #[test]
    fn the_delete_escape_is_wrapped_too() {
        // Spelled out: a stale image left on screen because the delete was
        // eaten is exactly the failure this whole change is about.
        assert_eq!(
            kitty(true).clear().map(String::from_utf8),
            Some(Ok(
                "\x1bPtmux;\x1b\x1b_Ga=d,d=I,i=7332\x1b\x1b\\\x1b\\".to_string()
            ))
        );
        // Nothing to draw means nothing to wrap.
        assert!(Graphics::new(Protocol::None, true)
            .draw("image/png", PNG, placement())
            .is_none());
        assert!(Graphics::new(Protocol::None, true).clear().is_none());
    }

    #[test]
    fn the_probe_asks_both_questions_through_one_envelope() {
        let plain = String::from_utf8(probe_query(false)).unwrap();
        assert!(plain.starts_with("\x1b_Gi=7331,"), "graphics query first");
        assert!(plain.ends_with("\x1b\\\x1b[c"), "then device attributes");

        // Under tmux both go to the outer terminal, in order: letting tmux
        // answer DA1 itself would end the wait before the graphics reply.
        let wrapped = probe_query(true);
        assert_eq!(
            String::from_utf8(unwrap_passthrough(&wrapped)).unwrap(),
            plain
        );
        assert_eq!(
            String::from_utf8_lossy(&wrapped)
                .matches("\x1bPtmux;")
                .count(),
            1
        );
    }

    #[test]
    fn a_terminal_that_draws_nothing_reserves_no_box() {
        assert!(kitty(false).draws());
        assert!(kitty(true).draws());
        assert!(Graphics::new(Protocol::ITerm2, false).draws());
        assert!(!Graphics::new(Protocol::None, true).draws());
    }

    #[test]
    fn the_log_line_says_what_is_happening() {
        // --log-file is how a user finds out why there is no artwork.
        assert_eq!(kitty(false).to_string(), "kitty");
        assert_eq!(kitty(true).to_string(), "kitty (wrapped for tmux)");
        assert_eq!(Graphics::new(Protocol::None, false).to_string(), "none");
    }

    #[test]
    fn parses_the_cli_choice() {
        assert_eq!(Protocol::parse("kitty"), Some(Protocol::Kitty));
        assert_eq!(Protocol::parse("iterm2"), Some(Protocol::ITerm2));
        assert_eq!(Protocol::parse("none"), Some(Protocol::None));
        assert_eq!(Protocol::parse("auto"), None); // handled by the caller
        assert_eq!(Protocol::parse("sixel"), None);
    }
}
