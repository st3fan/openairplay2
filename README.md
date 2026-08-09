# OpenAirPlay 2

[![crates.io](https://img.shields.io/crates/v/openairplay2.svg)](https://crates.io/crates/openairplay2)
[![docs.rs](https://img.shields.io/docsrs/openairplay2)](https://docs.rs/openairplay2)
[![CI](https://github.com/st3fan/openairplay2/actions/workflows/ci.yml/badge.svg)](https://github.com/st3fan/openairplay2/actions/workflows/ci.yml)

An **AirPlay 2 audio receiver** for Linux, written in Rust — the AirPlay 2
counterpart of [openairplay1](https://github.com/st3fan/openairplay1) (a working
AirPlay 1 / RAOP receiver).

AirPlay 2 is a substantially different protocol from AirPlay 1: HomeKit-style
pairing (SRP + Curve25519 + Ed25519), a ChaCha20-Poly1305-encrypted control
channel carrying binary plists, per-packet ChaCha20-Poly1305 audio, AAC as
well as ALAC, and PTP timing.

See [`notes.md`](https://github.com/st3fan/openairplay2/blob/main/notes.md)
for the protocol research and the milestone plan, and `notes/milestone-*.md`
for each milestone.

## Status

openairplay2 is a **working single-stream AirPlay 2 receiver**. A real macOS or
iOS device discovers it on the network, pairs with it, and streams to it, and
audio comes out of an ALSA device with working transport controls — all verified
against a real Mac.

The repository is a cargo workspace of four crates — a library, two binaries,
and the wire format the binaries share:

- **`openairplay2`** — an embeddable library: network in, decoded PCM +
  session events out. The host application provides the audio output (an
  `AudioSink`) and its own volume model; no ALSA dependency, builds and
  tests on macOS as well as Linux.
- **`openairplay2-receiver`** — the standalone Linux/ALSA receiver binary,
  built on the library's public API.
- **`openairplay2-tui`** — a full-screen now-playing display (with cover art
  in the terminal) that watches a receiver over a WebSocket, plus
  `openairplay2-tui-protocol`, the wire format they share.

It handles the full path end to end: mDNS/Bonjour discovery, HomeKit transient
pairing, a ChaCha20-Poly1305-encrypted control channel, the FairPlay `fp-setup`
handshake, two-phase `SETUP`, and buffered **AAC** playback — plus **pause /
resume**, **seek / skip**, and **volume** control, and **now-playing events**
(track title/artist/album and cover art) for embedding hosts to display.

By design it targets **one Mac → one stream → one output**. It deliberately does
**not** implement PTP (it never binds UDP 319/320): PTP exists to align multiple
outputs to a shared clock, and for a single output the sender's own buffering
plus our backpressure are enough. Multi-room / grouped playback would require PTP
and is out of scope.

Also not implemented: **ALAC**, **realtime (type 96) audio**, **48 kHz / S24**
(the decoder is fixed at 44.1 kHz stereo AAC-LC), and **`pair-verify` /
persistent pairing** — pairing is transient, so a sender pairs afresh each
session.

What each release contains, and what the next ones are for, is in
[`notes/roadmap.md`](https://github.com/st3fan/openairplay2/blob/main/notes/roadmap.md);
the milestone-by-milestone development history is in
[`notes/status.md`](https://github.com/st3fan/openairplay2/blob/main/notes/status.md).

## Install (Debian / Ubuntu)

Every release carries `.deb`s for **amd64**, **arm64** and **armhf** (Debian's
ARMv7 port — ARMv6 boards like the Pi Zero W are not supported): the receiver,
and the now-playing display as its own package. They are built on Debian 13
(trixie) and depend on a glibc of at least 2.39 (the receiver also on
`libasound2t64`), so they install on **Debian 13+ / Ubuntu 24.04+**. Download
the ones for your machine from the
[releases page](https://github.com/st3fan/openairplay2/releases) and install
them — the receiver brings a systemd service that starts at boot:

```sh
sudo apt-get install -y ./openairplay2-receiver_X.Y.Z-1_arm64.deb
sudo apt-get install -y ./openairplay2-tui_X.Y.Z-1_arm64.deb      # optional
```

Set the name, the audio device, a pairing password — any option — in
`/etc/default/openairplay2-receiver`: each is a named `OPENAIRPLAY2_*`
variable, documented in the file itself (the same options as the table
below). Then:

```sh
sudo systemctl restart openairplay2-receiver
systemctl status openairplay2-receiver
```

The service runs as its own `openairplay2` user and keeps its pairing
identity in `/var/lib/openairplay2`, so senders keep recognizing the receiver
across upgrades. The display package is just the `openairplay2-tui` binary —
no service, no configuration; it can go on a different machine than the
receiver. All the packages carry a build-provenance attestation:

```sh
gh attestation verify openairplay2-receiver_X.Y.Z-1_arm64.deb --repo st3fan/openairplay2
```

## Build & run

### System packages

Building links against nothing exotic, but the receiver binary does need the
ALSA headers — without them `cargo build` fails in `alsa-sys` with a
`pkg-config` error along the lines of `The system library 'alsa' required by
crate 'alsa-sys' was not found`.

On Debian / Ubuntu / Raspberry Pi OS:

```sh
sudo apt-get install build-essential pkg-config libasound2-dev avahi-daemon
```

| Package | Why |
| --- | --- |
| `build-essential` | a C compiler and linker for the crates with build scripts |
| `pkg-config` | how `alsa-sys` locates libasound at build time |
| `libasound2-dev` | ALSA headers — the receiver's audio output |
| `avahi-daemon` | mDNS/Bonjour advertisement, so senders can discover the receiver (runtime only — the receiver runs without it, it is just undiscoverable; `--no-avahi` skips advertising entirely) |

On Fedora / RHEL the equivalents are `gcc`, `pkgconf-pkg-config`,
`alsa-lib-devel` and `avahi`; on Arch, `base-devel`, `alsa-lib` and `avahi`.

Rust itself comes from [rustup](https://rustup.rs); the workspace needs Rust
1.88 or newer.

The **library** alone (`cargo build -p openairplay2`) needs none of this — it
has no audio-output dependency and builds on macOS too. Only
`openairplay2-receiver` pulls in ALSA.

`packaging/setup-build.sh` installs the build half of this list on a `.deb`
build box — everything except `avahi-daemon`, which is only needed at runtime —
plus `git`, `curl` and `ca-certificates`, and a cross toolchain with
`./setup-build.sh cross`.

### Build

```sh
cargo build --release
./target/release/openairplay2-receiver --name "Living Room"
```

Or build and install the released version from crates.io (this compiles from
source, so it needs the same system packages as above):

```sh
cargo install openairplay2-receiver
```

### Options

| Option | Environment variable | Description | Default |
| --- | --- | --- | --- |
| `--name NAME` | `OPENAIRPLAY2_NAME` | Name advertised to senders (mDNS + `GET /info`); `%h` becomes the hostname | `OpenAirPlay2 (%h)` |
| `--port PORT` | `OPENAIRPLAY2_PORT` | TCP port for the HTTP/RTSP control server | `7000` |
| `--mac AA:BB:CC:DD:EE:FF` | `OPENAIRPLAY2_MAC` | Device ID (`deviceid`) reported to senders | discovered from a network interface, else a fixed fallback |
| `--identity-file PATH` | `OPENAIRPLAY2_IDENTITY_FILE` | Where the Ed25519 identity keypair is stored | `~/.config/openairplay2/identity` |
| `--alsa-device NAME` | `OPENAIRPLAY2_ALSA_DEVICE` | ALSA output device to play to | `default` |
| `--list-devices` | — | List the audio outputs (one friendly entry per sound card) and exit | — |
| `--list-all-devices` | — | List every ALSA playback device — sub-devices and plugins included — and exit | — |
| `--no-audio` | `OPENAIRPLAY2_AUDIO=off` | Decode but don't open ALSA (silent run) | audio on |
| `--mixer CONTROL` | `OPENAIRPLAY2_MIXER` | Drive this ALSA mixer control from the sender's volume instead of scaling samples in software (`NAME` or `NAME,INDEX`) — see [hardware volume](#hardware-volume) | software gain |
| `--mixer-device DEV` | `OPENAIRPLAY2_MIXER_DEVICE` | Mixer device holding that control | the card of `--alsa-device`, else `default` |
| `--list-mixers` | — | List each device's mixer volume controls, with their dB ranges, and exit | — |
| `--no-avahi` | `OPENAIRPLAY2_AVAHI=off` | Don't advertise over Avahi / mDNS | advertising on |
| `--tui-socket PATH` (or `off`) | `OPENAIRPLAY2_TUI_SOCKET` | Serve the [now-playing WebSocket](#now-playing-display) on this local Unix socket — what `openairplay2-tui` connects to by default on the same machine; any local user may connect | `$XDG_RUNTIME_DIR/openairplay2/tui.sock`, else `/run/openairplay2/tui.sock` |
| `--tui-listen ADDR` | `OPENAIRPLAY2_TUI_LISTEN` | Also serve the [now-playing WebSocket](#now-playing-display) over TCP for a display on another machine, e.g. `127.0.0.1:7392` | off |
| `--tui-password PASS` | `OPENAIRPLAY2_TUI_PASSWORD` | Require this password on the now-playing WebSocket (`openairplay2-tui --password`). Prefer the variable — a flag is visible in `ps`. | open |
| `--password PASS` | `OPENAIRPLAY2_PASSWORD` | Require this password to pair — senders must enter it, and iOS/macOS present it as a password dialog, so it may be alphanumeric. Unset = transient `3939` (trusted LAN). It is never logged. Prefer the environment variable (the service's options file): a `--password` argument is visible to any local user in `ps`, an environment variable is not. `--pincode` / `OPENAIRPLAY2_PINCODE` are the deprecated 0.4 spellings and still honored. | transient `3939` |
| `--log-level LEVEL` (or `--debug`) | `OPENAIRPLAY2_LOG_LEVEL` | Log verbosity: `error`, `warn`, `info`, `debug`, `trace`. `debug` logs every request and hex-dumps bodies. `RUST_LOG` overrides it for per-module control. | `info` |

Every option falls back to its environment variable when the flag is absent —
that is how `/etc/default/openairplay2-receiver` configures the service — and
a flag on the command line wins. An empty variable means unset. The booleans
take exactly `on` or `off`.
| `-h`, `--help` | Print usage and exit | — |

Pass `--debug` (or `--log-level debug`) to log every request — useful for
watching what a real sender sends. `RUST_LOG` still works and overrides it for
per-module control (e.g. `RUST_LOG=openairplay2::session=trace`).

### Hardware volume

By default the receiver applies volume in software, scaling every sample by
the sender's dB value — which works everywhere but throws away sample
resolution at low volume. With `--mixer`, the sender's slider drives the
sound card's **own** mixer control instead: the card attenuates (often in its
analog stage), the samples reach it at full scale, and a DAC or amp with a
real volume control follows the slider.

```sh
openairplay2-receiver --list-mixers        # what can be driven
openairplay2-receiver --alsa-device plughw:CARD=S2 --mixer Speaker
```

The slider maps linearly onto the control's whole dB range (slider-top = the
control's maximum), and muting on the sender uses the control's mute switch
when it has one. The mixer device is normally derived from `--alsa-device`;
`--mixer-device` overrides it for split setups.

## Now-playing display

`openairplay2-tui` is a full-screen display of what is playing: title, artist
and album centered on screen, the cover art as a **real image** on terminals
that can draw one (Ghostty, Kitty, WezTerm, iTerm2, Konsole), and a status line
with the receiver's name, the sender's address, the stream format and the
volume.

On Debian/Ubuntu, install it from the `openairplay2-tui` package on the
[releases page](https://github.com/st3fan/openairplay2/releases) (see the
install section above). Elsewhere — macOS included, where it builds and runs —
build it from this repository:

```sh
cargo build --release -p openairplay2-tui
```

On the machine the receiver runs on, that is the whole setup — the receiver
serves a local socket by default and the display finds it on its own, whether
the receiver is the packaged service or was started by hand, by any local
user:

```sh
openairplay2-tui
```

For a display on **another machine**, start the receiver with the TCP
endpoint on and point the display at it:

```sh
openairplay2-receiver --tui-listen 127.0.0.1:7392
openairplay2-tui ws://127.0.0.1:7392
```

```
                      ┌──────────────┐
                      │   artwork    │
                      └──────────────┘
                        Sonata No. 1
                         Some Artist
                         Some Album
                   ━━━━━━━━━━━━━━━━━━━━━━━
                      1:23 / 4:07  ⏸ paused
      Living Room · 192.168.1.42 · 44100 Hz 2ch · -12.5 dB
```

The two programs are independent: either can be started, stopped and restarted
without the other, the display reconnects by itself, and several displays can
watch one receiver. A display that joins mid-track gets the full picture
immediately, artwork included. It is **read-only** — it never sends the
receiver a command (only WebSocket pong frames, which the library answers for
it).

Because it talks to the receiver over a WebSocket rather than linking it, the
display has no ALSA dependency and runs anywhere, macOS included: leave the
receiver on a Pi and watch it from a laptop. On-device, the Unix socket's file
permissions are the access control; the TCP endpoint carries track metadata
and cover art, so keep it on loopback (or behind an SSH tunnel) unless you
mean otherwise, and give it a password (`--tui-password`) if others can reach
it.

The endpoint to watch is the one optional positional argument — a
`ws://HOST:PORT` URL or a socket path, e.g. `openairplay2-tui
ws://10.0.0.5:7392`. Without it, the receiver's default sockets are tried,
then `ws://127.0.0.1:7392`.

| Option | Description | Default |
| --- | --- | --- |
| `--images auto\|kitty\|iterm2\|none` | Terminal graphics protocol; `auto` probes, `none` is text-only | `auto` |
| `--log-file PATH` | Where logs go — the display owns the screen, so they are dropped otherwise | dropped |
| `--password PASS` | Password for a receiver whose endpoint requires one; falls back to `OPENAIRPLAY2_TUI_PASSWORD`, which unlike a flag is not visible in `ps` | none |
| `-h`, `--help` | Print usage and exit | — |

`q`, `Esc` or `Ctrl-C` quits.

### Inside tmux

tmux forwards a graphics escape only when it is wrapped in its passthrough
envelope — which the display does automatically — **and** when passthrough is
switched on, which it is not by default. This needs **tmux 3.4 or newer**: the
`all` setting below arrived there (plain `allow-passthrough` arrived in 3.3).

```sh
tmux set -g allow-passthrough all         # now, in the running server
tmux set -g focus-events on
```

```sh
# and next time
cat >> ~/.tmux.conf <<'EOF'
set -g allow-passthrough all
set -g focus-events on
EOF
```

`allow-passthrough on` is enough to *draw* the artwork, but tmux does not know
the image exists, so nothing takes it down when you switch windows and it hangs
over whatever you switched to. The display removes it itself the moment its
pane stops being the visible one — which is what `focus-events on` tells it and
what `all` lets it say, since by then the pane is no longer visible. While it
is not being looked at the display transmits nothing at all, so `all` cannot
turn it into the thing drawing over your other windows.

Without any of this the display falls back to text only — it reserves no
artwork box rather than leaving a hole, and nothing is ever sprayed across the
screen. `--log-file PATH` says which protocol was detected, whether it is
wrapping (`kitty (wrapped for tmux)`), and what tmux will do with the escapes.

Detection has less to go on inside a pane: `TERM` and `TERM_PROGRAM` describe
tmux rather than the terminal you are looking at, so the display asks the
terminal directly and falls back to whatever the outer environment left behind.
If it guesses wrong, `--images kitty` or `--images iterm2` settles it.

**GNU screen is not supported.** Its passthrough is not tmux's, and its string
buffer is far smaller than the chunks an image is sent in, so the tail would
arrive as literal text — base64 across your screen. Inside screen the display
detects it and stays text-only.

One honest caveat remains: window switching is handled, but tmux still does not
track images, so a scroll, a copy-mode entry or an unrelated full redraw can
leave a stale one behind until the next change. The display is a single
in-place frame that deletes its image when it exits, which is the friendly
case, but the limitation is real.

## Embedding

The library's public API is small: build a `Receiver`, hand it a sink factory
and an event channel, run it on your tokio runtime.

```toml
[dependencies]
openairplay2 = "0.3"
```

```rust,no_run
use openairplay2::{AudioSink, Event, Receiver};

struct MySink; // your PCM → speaker path

impl AudioSink for MySink {
    fn write(&mut self, pcm: &[i16]) { /* blocking write paces playback */ }
    fn flush(&mut self) { /* seek: drop your device/prebuffer state */ }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let receiver = Receiver::builder()
        .name("Office")
        .identity_path("/var/lib/myapp/airplay-identity")
        .build()?;
    let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let Event::Volume { db } = event { /* your gain path */ }
        }
    });
    receiver.run(|_rate, _channels| Box::new(MySink), events).await
}
```

The library keeps the session semantics (pairing, decrypt, AAC decode, the
pause gate, seek flushing, backpressure); the host sees only PCM and events —
`SessionStarted`, `Volume` (in AirPlay dB), `Metadata`, `Artwork`, `Progress`,
`Paused`, `Flushed`, `SessionEnded` (the enum is `#[non_exhaustive]`). A host
that owns its mDNS registration builds with
`.advertise(false)` and publishes `receiver.txt_records()` itself.
