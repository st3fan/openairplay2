# Development status — milestone history

The milestone-by-milestone development history of openairplay2. For the current
high-level status and feature summary, see the [README](../README.md); for each
milestone's plan and results, see `notes/milestone-*.md`.

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

**Milestone 6 (soft timing: pause/resume, seek) — complete.** Honors
transport control for a single stream. The Mac drives pause and track-skip with
`FLUSHBUFFERED` (pause is `rate=0` + flush; skip is flush + a new anchor), so
the key is that a flush must **preempt** the ~2 s audio buffer rather than wait
behind it: a generation counter set out-of-band lets the player drop already-
buffered audio instantly, and `flushUntilSeq` discards the buffered-ahead audio
still arriving over TCP. The TCP reader backpressures on the player's queue
depth, so latency and memory stay bounded and the sound card's drain rate sets
the pace. Validated on real hardware (pause/resume/skip).

**Milestone 7 (volume control) — complete.** The Mac's volume slider now
changes playback volume. The volume it sends (`SET_PARAMETER volume: <dB>`) is
converted to a linear gain (`10^(dB/20)`, `-144 dB` = mute) and applied to the
PCM before the ALSA write, updated live via a shared atomic so slider moves take
effect mid-stream.

**Embeddable library ([plan](../plans/20260801-01-embeddable-library.md)) —
complete (PRs #15–#17), validated on hardware.** The repo is now a workspace: the
`openairplay2` library owns network → PCM behind a designed embedding API
(`Receiver` builder → `run(sink_factory, events)`; `AudioSink` for PCM out,
`Event` for session milestones, volume delivered in dB for the host's own gain
path) and builds without ALSA on macOS and Linux; `openairplay2-receiver` is
the standalone Linux/ALSA binary, functionally identical to the pre-split
receiver, consuming only the public API.

**Pause/resume fix ([plan](../plans/20260801-02-pause-resume-hold.md)) —
implemented; initial hardware validation passed (extended listening in
progress).** iPhone testing showed delayed,
mispositioned, stuttery resumes and a diverging sender-side timeline (stale
Now Playing widget, resumes jumping tracks back). Root cause: pause *dropped*
buffered audio the sender believed was safely delivered, and a sticky
`flushUntilSeq` boundary discarded re-sent audio (~47 s measured in one
session). Pause now holds audio — backpressure freezes the sender at the
pause point and resume plays instantly from the held buffer — and
`FLUSHBUFFERED` discards exactly the sequence range it names: the boundary
self-clears when the stream reaches it and resets at stream setup.

**Metadata and artwork events
([plan](../plans/20260802-01-metadata-artwork.md)) — complete, validated
on hardware (iPhone).** Requested by the embedding consumer
([st3fan/radio](https://github.com/st3fan/radio)): `SET_PARAMETER` payloads
are now dispatched on `Content-Type` — DMAP track metadata
(title/artist/album via a minimal `mlit` walker) becomes `Event::Metadata`,
cover art becomes `Event::Artwork` (forwarded as-is, `image/none`/empty =
cleared). Both are delivered only inside a session: pushed-early payloads
are latched and replayed right after `SessionStarted`.

**Debian packages ([plan](../plans/20260805-03-debian-packages.md),
[armhf](../plans/20260805-04-armhf-package.md)) — implemented; hardware
validation pending.** The receiver ships as a `.deb` for **amd64**, **arm64**
and **armhf**, each built by the release (amd64/arm64 natively, armhf
cross-compiled — no 32-bit ARM runners exist), attested, and attached to the
GitHub Release. Installing one yields a systemd service running as its own
`openairplay2` user, configured through `/etc/default/openairplay2-receiver`,
with the pairing identity in `/var/lib/openairplay2` so upgrades never force a
re-pair. Publishing a GitHub Release is now the only event that ships anything:
`release.yml` checks the tag against the crate version and dispatches
`cargo.yml` (crates.io) and `debian.yml` (the architecture matrix) in parallel.
The binary also handles SIGTERM, which is how systemd stops it.

**Now-playing display ([plan](../plans/20260805-05-tui.md)) — implemented;
hardware validation pending.** `openairplay2-tui` shows what is playing —
title/artist/album centered, cover art as a real image in terminals that can
draw one, position, volume, format and the sender's address — driven by a
WebSocket the receiver serves with `--tui-listen`. Ported from openairplay1's
dashboard, with the difference AirPlay 2 allows: pause is on the wire
(`SETRATEANCHORTIME rate=0`), so the display says "paused" rather than leaving
a frozen clock unexplained. The library gained the two things it was missing:
the sender's address on `SessionStarted`, and `Event::Progress` reported by the
playback thread from the RTP timestamp it is feeding the sink.

## Design: no PTP, by intent

This receiver targets **one Mac → one stream → one output** and does **not**
implement PTP: it never binds UDP 319/320 and replies `timingPort: 0`. PTP
exists to align *multiple* outputs to a shared clock; with a single output there
is nothing to align to, so the Mac's own buffering plus our backpressure are
sufficient. Multi-room / grouped playback would require PTP and is out of scope.
See [`notes/milestone-6.md`](milestone-6.md).
