# OpenAirPlay 2

An **AirPlay 2 audio receiver** for Linux, written in Rust — the AirPlay 2
counterpart of [openairplay](https://github.com/st3fan/openairplay) (a working
AirPlay 1 / RAOP receiver).

AirPlay 2 is a substantially different protocol from AirPlay 1: HomeKit-style
pairing (SRP + Curve25519 + Ed25519), a ChaCha20-Poly1305-encrypted control
channel carrying binary plists, per-packet ChaCha20-Poly1305 audio, AAC as
well as ALAC, and PTP timing.

See [`notes.md`](notes.md) for the protocol research and the milestone plan,
and `notes/milestone-*.md` for each milestone.

## Status

**Milestone 1 (discovery & `/info`) — complete.** Advertises `_airplay._tcp`
via Avahi with an AirPlay 2 `features` bitmask and an Ed25519 public key, runs
the HTTP/RTSP control server on port 7000, and answers `GET /info` with the
device plist.

**Milestone 2 (transient pairing & channel encryption) — complete.**
Implements HomeKit transient `pair-setup` (SRP-6a over the 3072-bit group,
SHA-512, fixed code `3939`) and, on completion, encrypts the control channel
with ChaCha20-Poly1305 (HKDF-derived keys, HAP block framing). Validated
against a real macOS sender.

**Milestone 3 (FairPlay `fp-setup`) — complete.** Answers the FairPlay
`fp-setup` handshake (the canned interop tables), which a real sender requires
after pairing before it will send `SETUP`. Validated against a real macOS
sender (it got past FairPlay and sent `SETUP`).

**Milestone 4 (SETUP & receiving the stream) — complete.** Handles the
two-phase `SETUP` — phase 1 binds the event channel and reports the
event/timing ports; phase 2 binds the audio data/control channels and
reports them, capturing the stream format and key. Acknowledges the session
control methods (RECORD, SETPEERS, …).

**Milestone 5 (decode & play buffered AAC) — complete.** For buffered
audio (`type 103`), the data channel is a **TCP** connection: it is framed
into packets, each decrypted with ChaCha20-Poly1305 (key `shk`), decoded from
raw AAC-LC via `symphonia`, and played to ALSA through a prebuffered output
thread. `--alsa-device` selects the device, `--no-audio` decodes without
playing. Validated against a real macOS sender (clean audio out).

**Milestone 6 (soft timing: pause/resume, seek) — implemented.** Honors
transport control for a single stream. The Mac drives pause and track-skip with
`FLUSHBUFFERED` (pause is `rate=0` + flush; skip is flush + a new anchor), so
the key is that a flush must **preempt** the ~2 s audio buffer rather than wait
behind it: a generation counter set out-of-band lets the player drop already-
buffered audio instantly and reset the device. The TCP reader backpressures on
the player's queue depth, so latency and memory stay bounded and the sound
card's drain rate sets the pace.

By design this receiver targets **one Mac → one stream → one output** and does
**not** implement PTP: it never binds UDP 319/320 and replies `timingPort: 0`.
PTP exists to align *multiple* outputs to a shared clock; with a single output
there is nothing to align to, so the Mac's own buffering plus our backpressure
are sufficient. Multi-room / grouped playback would require PTP and is out of
scope. See [`notes/milestone-6.md`](notes/milestone-6.md).

## Build & run

Building links against nothing exotic yet; a running `avahi-daemon` is needed
for discovery.

```sh
cargo build --release
./target/release/openairplay2 --name "Living Room"
```

Options: `--name`, `--port` (default 7000), `--mac`, `--identity-file`
(default `~/.config/openairplay2/identity`), `--no-avahi`,
`--alsa-device NAME` (default `default`), `--no-audio`. `RUST_LOG=debug`
shows every request — useful for watching what a real sender sends.
