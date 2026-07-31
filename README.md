# OpenAirPlay 2

An **AirPlay 2 audio receiver** for Linux, written in Rust — the AirPlay 2
counterpart of [openairplay1](https://github.com/st3fan/openairplay1) (a working
AirPlay 1 / RAOP receiver).

AirPlay 2 is a substantially different protocol from AirPlay 1: HomeKit-style
pairing (SRP + Curve25519 + Ed25519), a ChaCha20-Poly1305-encrypted control
channel carrying binary plists, per-packet ChaCha20-Poly1305 audio, AAC as
well as ALAC, and PTP timing.

See [`notes.md`](notes.md) for the protocol research and the milestone plan,
and `notes/milestone-*.md` for each milestone.

## Status

openairplay2 is a **working single-stream AirPlay 2 receiver**. A real macOS or
iOS device discovers it on the network, pairs with it, and streams to it, and
audio comes out of an ALSA device with working transport controls — all verified
against a real Mac.

It handles the full path end to end: mDNS/Bonjour discovery, HomeKit transient
pairing, a ChaCha20-Poly1305-encrypted control channel, the FairPlay `fp-setup`
handshake, two-phase `SETUP`, and buffered **AAC** playback — plus **pause /
resume**, **seek / skip**, and **volume** control.

By design it targets **one Mac → one stream → one output**. It deliberately does
**not** implement PTP (it never binds UDP 319/320): PTP exists to align multiple
outputs to a shared clock, and for a single output the sender's own buffering
plus our backpressure are enough. Multi-room / grouped playback would require PTP
and is out of scope.

The milestone-by-milestone development history is in
[`notes/status.md`](notes/status.md).

## Build & run

Building links against nothing exotic yet; a running `avahi-daemon` is needed
for discovery.

```sh
cargo build --release
./target/release/openairplay2 --name "Living Room"
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
| `-h`, `--help` | Print usage and exit | — |

Set `RUST_LOG=debug` to log every request — useful for watching what a real
sender sends.
