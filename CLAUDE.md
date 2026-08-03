# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

An AirPlay 2 audio **receiver** (Rust) — the AirPlay 2 counterpart of
[openairplay1](https://github.com/st3fan/openairplay1). A real Mac/iPhone discovers it, pairs
with it, and streams AAC to it; audio comes out of an ALSA device with pause/seek/volume.

A cargo workspace with two members:

- **`openairplay2/`** — the embeddable library: network → PCM. Owns discovery advertisement,
  pairing, the encrypted channel, SETUP, decrypt, AAC decode, and the pause/seek/backpressure
  semantics. No ALSA dependency; builds and tests on macOS as well as Linux. Public surface:
  `Receiver` + builder, `AudioSink`, `Event`, `Identity`, `Config`, `txt_records` — everything
  else is private or `#[doc(hidden)]` (test-sender pieces: `srp`, `tlv`, `cipher`, `server`).
- **`openairplay2-receiver/`** — the standalone Linux-only binary: CLI + `AlsaSink`
  (ALSA output, prebuffer cushion, dB→linear gain). It consumes only the library's public API
  (it is embedder #1).

Deliberate scope: **one sender → one stream → one output**. There is no PTP (UDP 319/320 is
never bound, `SETUP` replies `timingPort: 0`) — PTP exists to align *multiple* outputs, and for
a single output the sender's buffering plus our backpressure suffice. Multi-room grouping is out
of scope. Also unimplemented: `pair-verify` / persistent (non-transient) pairing, ALAC, realtime
(type 96) audio decode, 48 kHz / S24, metadata.

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
2. **Transient pairing** — SRP-6a (3072-bit group, SHA-512, fixed code `3939`), M1→M2, M3→M4.
   The SRP session key `K` becomes the channel secret.
3. **Encrypted** — everything after M4 is ChaCha20-Poly1305 in HomeKit block framing
   (`[u16 len LE][ciphertext][tag]`), keys HKDF-SHA512-derived per direction.

Request flow: [crypto_stream.rs](openairplay2/src/crypto_stream.rs) (`ControlConnection`) frames
and decrypts → [http.rs](openairplay2/src/http.rs) parses the hybrid HTTP/RTSP message →
[server.rs](openairplay2/src/server.rs) `handle_connection` special-cases `/pair-setup`, then
tries `dispatch_session` (per-connection `Session` state) and falls back to `dispatch`
(stateless: `/info`, `/fp-setup`, keep-alives).

[session.rs](openairplay2/src/session.rs) owns the streaming session. `SETUP` arrives in two
phases, distinguished by the presence of a `streams` array: phase 1 binds the event channel and
reports `eventPort`; phase 2 captures `shk`/`audioFormat`, binds the data + control channels, and
for buffered audio (`type` 103) starts the pipeline. The audio path is
TCP → [buffered.rs](openairplay2/src/buffered.rs) (block framing `[u16 len BE][packet]`,
per-packet ChaCha20-Poly1305) → [decode.rs](openairplay2/src/decode.rs) (raw AAC-LC, no ADTS,
via `symphonia`) → [player.rs](openairplay2/src/player.rs) (the library-side playback queue: a
dedicated thread feeding the host's `AudioSink`).

**The sink seam.** The library ends at PCM: at SETUP phase 2 it calls the host's sink factory
`(rate, channels) → Box<dyn AudioSink>` and thereafter delivers only audio that should actually
play — the pause gate and flush-generation dropping live in the library (they are session
semantics), while the device, its pacing, and gain live in the sink.
[openairplay2-receiver/src/player.rs](openairplay2-receiver/src/player.rs) is the binary's sink:
`AlsaSink` (open, blocking `writei`, `drop`+`prepare` reset, ~0.5 s prebuffer cushion) plus
`SharedGain`/`volume_to_gain`. Session milestones (`SessionStarted`, `Volume` in dB, `Paused`,
`Flushed`, `SessionEnded`) reach the host over an unbounded event channel; the library does
**not** apply volume — the host does ([events.rs](openairplay2/src/events.rs)).

The embedding facade is [receiver.rs](openairplay2/src/receiver.rs): `Receiver::builder()`
(name/port/mac/identity/advertise) → `build()` → `run(sink_factory, events)` on the caller's
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
  real sender abort before `SETUP` phase 2.
- **The `features` bitmask `0x0001_8340_405F_CA00`** ([receiver.rs](openairplay2/src/receiver.rs))
  is a known-good shairport-sync value. Getting it wrong makes senders offer AirPlay 1 or nothing.
  Bits 15/16/17 are the metadata bits (covers, progress, DAAP text) — senders silently skip
  sending track metadata/artwork unless these are advertised.
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
[runbooks/releasing.md](runbooks/releasing.md) — tag-driven crates.io publishing via the Release
workflow, with the failure procedure and the autopilot arrangement. CI
([.github/workflows/ci.yml](.github/workflows/ci.yml)) runs the workspace on Linux and the
library on macOS for every PR — the macOS portability deliverable is enforced there.

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
per-day sequence number, e.g. `plans/20260731-01-alac-realtime.md`). A plan holds the high-level
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

**Never assume the status of a pull request.** Whether a PR is open, merged, closed, approved, or
green in CI is only knowable by asking: run `gh pr view <n>` / `gh pr status` / `gh pr checks <n>`
before acting on that status or reporting it.

Milestones 1–7 predate this convention and are recorded under `notes/` instead:
[notes/status.md](notes/status.md) is the milestone history, `notes/milestone-*.md` the
per-milestone plans, and [notes.md](notes.md) the protocol research and original plan. Keep
`notes/status.md` and the README's status section current as behavior changes.
