# Development status — milestone history

The milestone-by-milestone development history of openairplay2. For the current
high-level status and feature summary, see the [README](../README.md); for each
milestone's plan and results, see `notes/milestone-*.md`. For what each release
contains and what the next ones are for, see [`roadmap.md`](roadmap.md).

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
two-phase `SETUP` — phase 1 binds the event channel and reports the event
port (and `timingPort: 0` — see "no PTP" below); phase 2 binds the audio
data/control channels and
reports them, capturing the stream format and key. Acknowledges the session
control methods (RECORD, SETPEERS, …).

**Milestone 5 (decode & play buffered AAC) — complete.** For buffered
audio (`type 103`), the data channel is a **TCP** connection: it is framed
into packets, each decrypted with ChaCha20-Poly1305 (key `shk`), decoded from
raw AAC-LC via `symphonia`, and played to ALSA through a prebuffered output
thread. `--alsa-device` selects the device, `--no-audio` decodes without
playing. Validated against a real macOS sender (clean audio out). *(ALSA moved
to `openairplay2-receiver` in the library split below; the library ends at
PCM.)*

**Milestone 6 (soft timing: pause/resume, seek) — complete.** Honors
transport control for a single stream. The Mac drives pause and track-skip with
`FLUSHBUFFERED` (pause is `rate=0` + flush; skip is flush + a new anchor), so
the key is that a flush must **preempt** the ~2 s audio buffer rather than wait
behind it: an out-of-band signal lets the player act on a flush instantly, and
`flushUntilSeq` discards the buffered-ahead audio still arriving over TCP. The
TCP reader backpressures on the player's queue depth, so latency and memory stay
bounded and the sound card's drain rate sets the pace. Validated on real
hardware (pause/resume/skip). **Superseded in part:** pause dropping audio was
wrong and was replaced by a hold, and the generation counter by a sequence
boundary — see the pause/resume fix below and
[`plans/20260801-02`](../plans/20260801-02-pause-resume-hold.md).

**Milestone 7 (volume control) — complete.** The Mac's volume slider now
changes playback volume. The volume it sends (`SET_PARAMETER volume: <dB>`) is
converted to a linear gain (`10^(dB/20)`, `-144 dB` = mute) and applied to the
PCM before the ALSA write, updated live via a shared atomic so slider moves take
effect mid-stream. *(Since the library split the library reports dB and the host
applies the gain — `volume_to_gain` lives in `openairplay2-receiver`.)*

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

**Published on crates.io + release automation
([plan](../plans/20260802-02-crates-io.md)) — complete (PRs #26–#29).** The
library and the receiver are published to crates.io, and a GitHub Release is
the only event that ships anything: `release.yml` checks the tag against the
crate version and dispatches `cargo.yml` (crates.io, via trusted publishing —
no stored token) and `debian.yml` (the `.deb` matrix) in parallel. 0.1.0 and
0.2.0 were published by hand; 0.3.0 was the first automated release. The
procedure and its failure modes are in
[`runbooks/releasing.md`](../runbooks/releasing.md).

**Pincode pairing ([plan](../plans/20260805-01-pincode.md)) — complete,
validated on hardware (iOS 26).** `--pincode CODE` makes a sender ask for a
code instead of pairing silently: it sets status-flag bit 7 ("password
required") in both the mDNS TXT record and `GET /info`, answers
`POST /pair-pin-start` with an empty 200, and uses the configured code as the
SRP password in place of the transient `3939`. iOS completes at M4 plus the
encrypted channel — no M5/M6 in this flow. The wire-level details, including
what a real iPhone does, are in [`notes/protocol.md`](protocol.md).

**Logging pass ([plan](../plans/20260805-02-logging.md)) — complete (PR #41).**
`info` is startup and problems only; per-request protocol chatter moved to
`debug`, so a service journal stays readable and `RUST_LOG=debug` is what shows
the wire.

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

**Now-playing display ([plan](../plans/20260805-05-tui.md)) — complete,
validated on hardware (skynet, Ghostty).** `openairplay2-tui` shows what is playing —
title/artist/album centered, cover art as a real image in terminals that can
draw one, position, volume, format and the sender's address — driven by a
WebSocket the receiver serves with `--tui-listen`. Ported from openairplay1's
dashboard, with the difference AirPlay 2 allows: pause is on the wire
(`SETRATEANCHORTIME rate=0`), so the display says "paused" rather than leaving
a frozen clock unexplained. The library gained the two things it was missing:
the sender's address on `SessionStarted`, and `Event::Progress` reported by the
playback thread from the RTP timestamp it is feeding the sink.

**Cover art through tmux ([plan](../plans/20260805-06-tmux-artwork.md)) —
complete (PRs #60, #61, #63), validated on hardware (skynet: Ghostty inside
tmux 3.5a).** Inside tmux the display drew its text and no artwork: tmux
forwards a terminal-graphics escape only inside its DCS passthrough envelope,
and detection is blind through a pane (`TERM`/`TERM_PROGRAM` describe tmux, and
the Kitty capability probe was swallowed on the way out). The display now wraps
what it emits, probes through the envelope, and — because tmux tracks no images
and would otherwise leave the cover art floating over the next window — takes
its image down when the pane stops being the visible one, transmitting nothing
at all while it is not. That takedown needs `allow-passthrough all` (tmux 3.4+)
and `focus-events on`; the display asks tmux what the setting is and says so in
its log rather than failing silently.

## Design: no PTP, by intent

This receiver targets **one Mac → one stream → one output** and does **not**
implement PTP: it never binds UDP 319/320 and replies `timingPort: 0`. PTP
exists to align *multiple* outputs to a shared clock; with a single output there
is nothing to align to, so the Mac's own buffering plus our backpressure are
sufficient. Multi-room / grouped playback would require PTP and is out of scope.
See [`notes/milestone-6.md`](milestone-6.md).
