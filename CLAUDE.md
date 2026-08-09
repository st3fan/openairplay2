# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

An AirPlay 2 audio **receiver** (Rust) — the AirPlay 2 counterpart of
[openairplay1](https://github.com/st3fan/openairplay1). A real Mac/iPhone discovers it, pairs
with it, and streams AAC to it; audio comes out of an ALSA device with pause/seek/volume.

A cargo workspace with four members:

- **`openairplay2/`** — the embeddable library: network → PCM. Owns discovery advertisement,
  pairing, the encrypted channel, SETUP, decrypt, AAC decode, and the pause/seek/backpressure
  semantics. No ALSA dependency; builds and tests on macOS as well as Linux. Public surface:
  `Receiver` + `ReceiverBuilder`, `AudioSink`, `SinkFactory`, `Event`, `EventSender`, `Identity`,
  `Config`, `txt_records` — everything else is private or `#[doc(hidden)]` (test-sender pieces:
  `srp`, `tlv`, `cipher`, `server`).
- **`openairplay2-receiver/`** — the standalone Linux-only binary: CLI + `AlsaSink`
  (ALSA output, prebuffer cushion, dB→linear gain). It consumes only the library's public API
  (it is embedder #1). It is also what `packaging/` packages: a `.deb` (systemd unit,
  `/etc/default` options file, `openairplay2` system user) built by `packaging/build-deb.sh`
  from `[package.metadata.deb]` in its `Cargo.toml`. `--tui-listen ADDR` serves the
  now-playing WebSocket ([tui.rs](openairplay2-receiver/src/tui.rs)): a snapshot on connect,
  then one message per change, over a bounded broadcast channel so a slow display can never
  stall the audio path.
- **`openairplay2-tui/`** — the full-screen now-playing display (ratatui + crossterm, Kitty and
  iTerm2 terminal graphics for cover art, wrapped in tmux's DCS passthrough envelope when it runs
  inside tmux). It depends on neither the library nor ALSA — a WebSocket client and a renderer —
  so it builds and runs on macOS as well, and CI enforces
  that. **`openairplay2-tui-protocol/`** is the serde-only wire format both ends share; every
  message's exact JSON is asserted in its own tests, because a published format that drifts
  silently is worse than none.

Deliberate scope: **one sender → one stream → one output**. There is no PTP (the PTP ports
319/320 are never bound — the control and realtime channels do bind ephemeral UDP sockets —
and `SETUP` replies `timingPort: 0`) — PTP exists to align *multiple* outputs, and for
a single output the sender's buffering plus our backpressure suffice. Multi-room grouping is out
of scope. Also unimplemented: `pair-verify` / persistent (non-transient) pairing, ALAC, realtime
(type 96) audio decode, 48 kHz / S24.

## Build, test, run

The receiver's target platform is Linux: ALSA (`libasound2-dev`), a running `avahi-daemon` for
discovery, and `/sys/class/net` for MAC discovery. The **library** must additionally keep
building and passing its tests on macOS (`cargo test -p openairplay2`) — that portability is a
deliverable, not an accident.

```bash
cargo build --release && cargo test && cargo clippy --all-targets && cargo fmt --check
```

```bash
cargo test -p openairplay2            # library only (the macOS-portable subset)
cargo test volume_db_to_gain          # one unit test by name (receiver crate)
cargo test --test pairing             # one integration test file (openairplay2/tests/pairing.rs)
```

```bash
RUST_LOG=debug ./target/release/openairplay2-receiver --name "Living Room" --alsa-device default
```

`RUST_LOG=debug` logs every request head and hex-dumps bodies — the way to see what a real sender
actually sends. `--no-audio` decodes without opening ALSA; `--no-avahi` skips advertising. Full
option list is in the [README](README.md).

## Architecture

One TCP control connection carries the whole session, in three regimes on the same socket:

1. **Plaintext HTTP/RTSP** — `GET /info`, `POST /pair-setup`.
2. **Transient pairing** — SRP-6a (3072-bit group, SHA-512, fixed code `3939`, or the configured `--password`), M1→M2, M3→M4.
   The SRP session key `K` becomes the channel secret. A configured password
   (Apple's word — the iOS/macOS dialog is free-text, alphanumerics welcome;
   `--pincode` is the deprecated 0.4 spelling) sets status-flag bit 7
   ("password required") and answers `pair-pin-start`
   with an empty 200, so a sender prompts for and enters the password, which
   becomes the SRP password; iOS pairs at M4 + the encrypted channel (no
   M5/M6 in this flow). See `notes/protocol.md`.
3. **Encrypted** — everything after M4 is ChaCha20-Poly1305 in HomeKit block framing
   (`[u16 len LE][ciphertext][tag]`), keys HKDF-SHA512-derived per direction.

Request flow: [crypto_stream.rs](openairplay2/src/crypto_stream.rs) (`ControlConnection`) frames
and decrypts → [http.rs](openairplay2/src/http.rs) parses the hybrid HTTP/RTSP message →
[server.rs](openairplay2/src/server.rs) `handle_connection` (connection-level concerns only: the
handshake timeout, the `/pair-setup` cipher install, the `SETUP` takeover claim, the
`CSeq`/`Server` echo) → `handlers::dispatch`, the routing table in
[handlers/](openairplay2/src/handlers/) — one file per verb/endpoint. **Handlers, commands,
errors, types** (the tqs/tdb layering): a handler parses the wire body and builds a `Params`
struct; a command in [commands/](openairplay2/src/commands/) — `#[derive(Validate)]` params, a
function over `(&mut Session, params)` whose first line is `params.validate()?` — applies it to
session state. Parsing lives in handlers, normalization in
[types.rs](openairplay2/src/types.rs) constructors (`VolumeDb`), validation in commands — each
in exactly one place. [errors.rs](openairplay2/src/errors.rs) `CommandError` is the one spot an
error chooses a status code; handlers where garbage must answer 200 anyway (`SET_PARAMETER`,
`GET_PARAMETER`, transport control — metadata is decoration, and a refused volume must not
poison the session) state that **tolerance policy** at the top of the file and log instead.

[session.rs](openairplay2/src/session.rs) holds the session state and the audio pipeline.
`SETUP` arrives in two phases, distinguished by the presence of a `streams` array
([handlers/setup.rs](openairplay2/src/handlers/setup.rs) picks; each phase is a command):
phase 1 binds the event channel and reports `eventPort`; phase 2 captures `shk`/`audioFormat`,
binds the data + control channels, and
for buffered audio (`type` 103) starts the pipeline. The audio path is
TCP → [buffered.rs](openairplay2/src/buffered.rs) (block framing `[u16 len BE][packet]`,
per-packet ChaCha20-Poly1305) → [decode.rs](openairplay2/src/decode.rs) (raw AAC-LC, no ADTS,
via `symphonia`) → [player.rs](openairplay2/src/player.rs) (the library-side playback queue: a
dedicated thread feeding the host's `AudioSink`).

Track metadata and cover art arrive as DAAP/DMAP blobs on `SET_PARAMETER` and are walked by
[dmap.rs](openairplay2/src/dmap.rs) into `Event::Metadata` / `Event::Artwork`.

**The sink seam.** The library ends at PCM: at SETUP phase 2 it calls the host's sink factory
`(rate, channels) → Box<dyn AudioSink>` and thereafter delivers only audio that should actually
play — the pause gate and the flush boundary (each queued packet stamped with its sequence
number) live in the library (they are session semantics), while the device, its pacing, and gain
live in the sink.
[openairplay2-receiver/src/player.rs](openairplay2-receiver/src/player.rs) is the binary's sink:
`AlsaSink` (open, blocking `writei`, `drop`+`prepare` reset, ~0.5 s prebuffer cushion) plus
`SharedGain`/`volume_to_gain`. Session milestones (`SessionStarted` with the sender's address,
`Volume` in dB, `Metadata`, `Artwork`, `Progress`, `Paused`, `Flushed`, `SessionEnded`) reach the
host over an unbounded event channel; the library does **not** apply volume — the host does
([events.rs](openairplay2/src/events.rs)).

**`Progress` follows the audio, not the sender.** A sender's `progress:` line arrives at track
start and essentially never again, so the position is reported once a second by the playback
thread from the RTP timestamp it is feeding the sink, against the track extent that line named
([player.rs](openairplay2/src/player.rs) `Position`). Hosts display it as-is and must not
extrapolate: a pause simply stops the reports, which is what freezes the clock.

The embedding facade is [receiver.rs](openairplay2/src/receiver.rs): `Receiver::builder()`
(name/port/mac/identity/password/advertise) → `build()` → `run(sink_factory, events)` on the caller's
runtime. `advertise(false)` + `Receiver::txt_records()` supports hosts that own their mDNS.

Timing is "soft": the sink prebuffers ~0.5 s and blocking ALSA writes pace playback, while the
TCP reader backpressures on `pending_samples()` so latency and memory stay bounded (~2 s).

## Invariants worth knowing before editing

- **Transport control must bypass the audio queue.** An in-band command would sit behind the ~2 s
  buffer. Pause is a persistent atomic gate; seek is `flushUntilSeq`, applied out-of-band to the
  queue (each decoded packet is stamped with its sequence number) and pre-decrypt to arriving TCP
  packets by their *plaintext* sequence number. Both call `AudioSink::flush` so the sink discards
  its own device/prebuffer state.
- **Pause holds; only a flush discards — and exactly what it names.** A sender may pause with a
  bare `rate=0` and expects everything it sent to still be buffered at resume, so pause parks
  audio (queued-sample count stays up → backpressure freezes the sender's cursor) and resume
  plays it. `FLUSHBUFFERED` discards only `seq < flushUntilSeq`, retaining the rest; the boundary
  is consumed when the stream reaches it and reset at stream setup — a stale boundary silently
  discarded ~47 s of audio once (see plans/20260801-02-pause-resume-hold.md).
- **The library must stay free of audio-output dependencies** (no `alsa` anywhere in
  `cargo tree -p openairplay2`) and free of AirPlay wire types in its documented public API.
- **Never read past a message boundary while in the clear**
  ([crypto_stream.rs](openairplay2/src/crypto_stream.rs)): the head is read a byte at a time and
  the body to its exact `Content-Length`, so the cipher can be installed mid-connection right
  after the plaintext M4 response is written.
- **Every response needs `Content-Length` (even empty), the echoed `CSeq`, and the request's own
  protocol token** (`HTTP/1.1` vs `RTSP/1.0`). A `GET_PARAMETER volume` with an empty body makes a
  real sender abort before `SETUP` phase 2 — an observation from hardware testing that was never
  written down as a capture, unlike the pincode and metadata findings in `notes/`.
- **The `features` bitmask `0x0001_8340_405F_CA00`** ([receiver.rs](openairplay2/src/receiver.rs))
  is shairport-sync's known-good value with bits 15/16/17 — the metadata bits (covers, progress,
  DAAP text) — set on top; shairport's own value is `0x0001_8340_405C_4A00`. Getting the mask
  wrong makes senders offer AirPlay 1 or nothing (per shairport-sync and pyatv; not tested here),
  and senders silently skip sending track metadata/artwork unless the metadata bits are
  advertised (observed against an iPhone — see plans/20260802-01-metadata-artwork.md).
- **The identity (Ed25519 `pk` + `pi` UUID) must be stable across restarts** — senders remember a
  receiver by it. The builder therefore requires `identity` or `identity_path` (no ephemeral
  default); the receiver binary persists to `~/.config/openairplay2/identity`.
- **`aac_params()` hard-codes 44.1 kHz stereo AAC-LC** — `audioFormat` is captured but not yet
  honored. Other formats aren't negotiated.
- **The FairPlay tables in [fairplay.rs](openairplay2/src/fairplay.rs) are verbatim third-party
  Apple-derived constants**, and `/fp-setup` here is a canned handshake, not live crypto. Read
  [notes/licensing.md](notes/licensing.md) before touching or extending that path; do not import
  FairPlay *decryption* code (the known implementations are GPLv2 and this project is MIT).

## Runbooks

Operational procedures live in `runbooks/`. When asked to do a **release**, follow
[runbooks/releasing.md](runbooks/releasing.md) — publishing a GitHub Release is the only event
that ships anything: [release.yml](.github/workflows/release.yml) checks the tag against the
crate version and dispatches [cargo.yml](.github/workflows/cargo.yml) (crates.io) and
[debian.yml](.github/workflows/debian.yml) (amd64 + arm64 + armhf `.deb`s, attached to the
release) in
parallel. The runbook also holds the failure procedure and the autopilot arrangement. CI
([.github/workflows/ci.yml](.github/workflows/ci.yml)) runs the workspace on Linux, and the
library, the protocol crate and the display on macOS, for every PR — the macOS portability
deliverable is enforced there.

## Tests

Unit tests live inline (`#[cfg(test)] mod tests`) next to the code; integration tests in
`openairplay2/tests/` run the real server over a real TCP socket and drive it with a synthetic
sender — [pairing.rs](openairplay2/tests/pairing.rs) completes transient pair-setup using
`SrpClient` (the test-side SRP implementation kept in [srp.rs](openairplay2/src/srp.rs)) and then
exchanges encrypted requests; they reach the server through `#[doc(hidden)]` modules (`server`,
`srp`, `tlv`, `cipher`), which is what those exist for. Protocol parsers are tested against
bodies captured from a real Mac (see `parses_real_setrateanchortime`,
`parses_real_flushbuffered`). `openairplay2/tests/data/aac_frames.bin` is a committed golden
AAC-LC fixture used by the decoder test. The playback queue is tested against a recording fake
`AudioSink` ([player.rs](openairplay2/src/player.rs)) — delivery order, pause gating, flush
dropping, backpressure.

Anything touching the wire protocol or timing behavior is also expected to be verified against a
real Mac; that hardware check is part of each milestone's acceptance criteria.

## Working conventions

New features and changes start with a **plan** in `plans/YYYYMMDD-NN-slug.md` (`NN` is a
per-day sequence number, e.g. `plans/20260805-01-pincode.md`). A plan holds the high-level
implementation details for one change: background, scope with explicit out-of-scope, module
layout, test strategy, acceptance criteria — the shape the `notes/milestone-*.md` files already
use.

**A plan and its implementation live together in one stack** (managed with the `gh-stack`
skill). The plan document is the bottom PR of a fresh stack; the implementation follows in one
or more **phases**, each phase one PR stacked on top, one branch per phase, each based on the
one below it:

- Open the stack with the plan PR alone, and **wait for Stefan to approve the plan** (review
  feedback on the open PR — not a merge) before stacking implementation PRs onto it.
- The plan PR **stays open for the whole task** — that is the point of stacking it: if the work
  reveals mid-way that the plan needs adjusting, or decisions worth recording, commit them to
  the plan document on its still-open branch (then `gh stack rebase --upstack`), so the plan
  that eventually merges matches what was actually built.
- At the end Stefan reviews and merges the whole stack himself. (The earlier convention —
  merge the plan PR first, then implement — is retired; plan `20260801-01` and older predate
  the stack workflow.)

**All changes land through pull requests. Never commit directly to `main`** — always branch first.

**Anything not being done now goes in a GitHub issue.** Work deliberately skipped, an
out-of-scope item that still deserves to exist, a limitation found while testing, a proposed
enhancement, an idea worth keeping — file it (`gh issue create`) rather than leaving it in a PR
description or a conversation. Write it so it stands alone: what was observed, why it happens if
that is known, and the shape of a fix.

**Every issue gets exactly one of `bug` or `enhancement`.** `bug` = something does not work as
documented or intended; `enhancement` = something that never existed. Add `documentation` as a
second label when the fix is only prose. Set it at creation: `gh issue create --label bug …`.

**Never assume the status of a pull request.** Whether a PR is open, merged, closed, approved, or
green in CI is only knowable by asking: run `gh pr view <n>` / `gh pr status` / `gh pr checks <n>`
before acting on that status or reporting it.

Milestones 1–7 predate this convention and are recorded under `notes/` instead:
[notes/status.md](notes/status.md) is the milestone history, `notes/milestone-*.md` the
per-milestone plans, and [notes.md](notes.md) the protocol research and original plan. Keep
`notes/status.md` and the README's status section current as behavior changes.
