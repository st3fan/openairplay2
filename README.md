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

The repository is a cargo workspace with two artifacts:

- **`openairplay2`** — an embeddable library: network in, decoded PCM +
  session events out. The host application provides the audio output (an
  `AudioSink`) and its own volume model; no ALSA dependency, builds and
  tests on macOS as well as Linux.
- **`openairplay2-receiver`** — the standalone Linux/ALSA receiver binary,
  built on the library's public API.

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

The milestone-by-milestone development history is in
[`notes/status.md`](https://github.com/st3fan/openairplay2/blob/main/notes/status.md).

## Install (Debian / Ubuntu)

Every release carries a `.deb` for **amd64**, **arm64** and **armhf** (Debian's
ARMv7 port — ARMv6 boards like the Pi Zero W are not supported). Download the one
for your machine from the
[releases page](https://github.com/st3fan/openairplay2/releases) and install
it — it brings a systemd service that starts at boot:

```sh
sudo apt-get install -y ./openairplay2-receiver_X.Y.Z-1_arm64.deb
```

Set the name and the audio device in `/etc/default/openairplay2-receiver`
(the same flags as below), then:

```sh
sudo systemctl restart openairplay2-receiver
systemctl status openairplay2-receiver
```

The service runs as its own `openairplay2` user and keeps its pairing
identity in `/var/lib/openairplay2`, so senders keep recognizing the receiver
across upgrades. The packages carry a build-provenance attestation:

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

The same package list is what `packaging/setup-build.sh` installs on a
`.deb` build box (plus a cross toolchain with `./setup-build.sh cross`).

### Build

```sh
cargo build --release
./target/release/openairplay2-receiver --name "Living Room"
```

Or install the released binary straight from crates.io:

```sh
cargo install openairplay2-receiver
```

### Options

| Option | Description | Default |
| --- | --- | --- |
| `--name NAME` | Name advertised to senders (mDNS + `GET /info`) | `OpenAirPlay2` |
| `--port PORT` | TCP port for the HTTP/RTSP control server | `7000` |
| `--mac AA:BB:CC:DD:EE:FF` | Device ID (`deviceid`) reported to senders | discovered from a network interface, else a fixed fallback |
| `--identity-file PATH` | Where the Ed25519 identity keypair is stored | `~/.config/openairplay2/identity` |
| `--alsa-device NAME` | ALSA output device to play to | `default` |
| `--no-audio` | Decode but don't open ALSA (silent run) | audio on |
| `--no-avahi` | Don't advertise over Avahi / mDNS | advertising on |
| `--tui-listen ADDR` | Serve the now-playing WebSocket (track, artwork, position) on this address, e.g. `127.0.0.1:7392` | off |
| `--pincode CODE` | Require this pincode to pair — senders must enter it (free-text "password" dialog). Unset = transient `3939` (trusted LAN). The pincode is never logged. | transient `3939` |
| `-h`, `--help` | Print usage and exit | — |

Set `RUST_LOG=debug` to log every request — useful for watching what a real
sender sends.

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
`SessionStarted`, `Volume` (in AirPlay dB), `Paused`, `Flushed`,
`SessionEnded`. A host that owns its mDNS registration builds with
`.advertise(false)` and publishes `receiver.txt_records()` itself.
