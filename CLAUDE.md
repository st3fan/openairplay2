# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

An AirPlay 2 audio **receiver** (Rust, Linux/ALSA) — the AirPlay 2 counterpart of
[openairplay1](https://github.com/st3fan/openairplay1). A real Mac/iPhone discovers it, pairs
with it, and streams AAC to it; audio comes out of an ALSA device with pause/seek/volume.

Deliberate scope: **one sender → one stream → one output**. There is no PTP (UDP 319/320 is
never bound, `SETUP` replies `timingPort: 0`) — PTP exists to align *multiple* outputs, and for
a single output the sender's buffering plus our backpressure suffice. Multi-room grouping is out
of scope. Also unimplemented: `pair-verify` / persistent (non-transient) pairing, ALAC, realtime
(type 96) audio decode, 48 kHz / S24, metadata.

## Build, test, run

The target platform is Linux: ALSA (`libasound2-dev`), a running `avahi-daemon` for discovery,
and `/sys/class/net` for MAC discovery. It does **not** build or run on macOS — build and test on
a Linux machine even though development may happen elsewhere.

```bash
cargo build --release && cargo test && cargo clippy --all-targets && cargo fmt --check
```

```bash
cargo test volume_db_to_gain          # one unit test by name
cargo test --test pairing             # one integration test file (tests/pairing.rs)
```

```bash
RUST_LOG=debug ./target/release/openairplay2 --name "Living Room" --alsa-device default
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

Request flow: [crypto_stream.rs](src/crypto_stream.rs) (`ControlConnection`) frames and decrypts
→ [http.rs](src/http.rs) parses the hybrid HTTP/RTSP message → [server.rs](src/server.rs)
`handle_connection` special-cases `/pair-setup`, then tries `dispatch_session` (per-connection
`Session` state) and falls back to `dispatch` (stateless: `/info`, `/fp-setup`, keep-alives).

[session.rs](src/session.rs) owns the streaming session. `SETUP` arrives in two phases,
distinguished by the presence of a `streams` array: phase 1 binds the event channel and reports
`eventPort`; phase 2 captures `shk`/`audioFormat`, binds the data + control channels, and for
buffered audio (`type` 103) starts the pipeline. The audio path is
TCP → [buffered.rs](src/buffered.rs) (block framing `[u16 len BE][packet]`, per-packet
ChaCha20-Poly1305) → [decode.rs](src/decode.rs) (raw AAC-LC, no ADTS, via `symphonia`) →
[player.rs](src/player.rs) (dedicated ALSA thread).

Timing is "soft": the player prebuffers ~0.5 s and blocking ALSA writes pace playback, while the
TCP reader backpressures on `pending_samples()` so latency and memory stay bounded (~2 s).

## Invariants worth knowing before editing

- **Transport control must bypass the audio queue.** An in-band command would sit behind the ~2 s
  buffer. Pause is a persistent atomic gate (the Mac keeps pushing buffered-ahead audio during a
  pause, so a one-shot flush would not stop it); seek is a generation counter — queued PCM is
  stamped and stale stamps are dropped — plus `flushUntilSeq`, which discards still-arriving TCP
  packets by their *plaintext* sequence number, before any decrypt.
- **Never read past a message boundary while in the clear** ([crypto_stream.rs](src/crypto_stream.rs)):
  the head is read a byte at a time and the body to its exact `Content-Length`, so the cipher can be
  installed mid-connection right after the plaintext M4 response is written.
- **Every response needs `Content-Length` (even empty), the echoed `CSeq`, and the request's own
  protocol token** (`HTTP/1.1` vs `RTSP/1.0`). A `GET_PARAMETER volume` with an empty body makes a
  real sender abort before `SETUP` phase 2.
- **The `features` bitmask `0x0001_8340_405C_4A00`** ([main.rs](src/main.rs)) is a known-good
  shairport-sync value. Getting it wrong makes senders offer AirPlay 1 or nothing.
- **The identity (Ed25519 `pk` + `pi` UUID) must be stable across restarts** — senders remember a
  receiver by it; it is persisted to `~/.config/openairplay2/identity`.
- **`aac_params()` hard-codes 44.1 kHz stereo AAC-LC** — `audioFormat` is captured but not yet
  honored. Other formats aren't negotiated.
- **The FairPlay tables in [fairplay.rs](src/fairplay.rs) are verbatim third-party Apple-derived
  constants**, and `/fp-setup` here is a canned handshake, not live crypto. Read
  [notes/licensing.md](notes/licensing.md) before touching or extending that path; do not import
  FairPlay *decryption* code (the known implementations are GPLv2 and this project is MIT).

## Tests

Unit tests live inline (`#[cfg(test)] mod tests`) next to the code; integration tests in `tests/`
run the real server over a real TCP socket and drive it with a synthetic sender —
[tests/pairing.rs](tests/pairing.rs) completes transient pair-setup using `SrpClient` (the
test-side SRP implementation kept in [src/srp.rs](src/srp.rs)) and then exchanges encrypted
requests. Protocol parsers are tested against bodies captured from a real Mac (see
`parses_real_setrateanchortime`, `parses_real_flushbuffered`). `tests/data/aac_frames.bin` is a
committed golden AAC-LC fixture used by the decoder test.

Anything touching the wire protocol or timing behavior is also expected to be verified against a
real Mac; that hardware check is part of each milestone's acceptance criteria.

## Working conventions

New features and changes start with a **plan** in `plans/YYYYMMDD-NN-slug.md` (`NN` is a
per-day sequence number, e.g. `plans/20260731-01-alac-realtime.md`). A plan holds the high-level
implementation details for one change: background, scope with explicit out-of-scope, module
layout, test strategy, acceptance criteria — the shape the `notes/milestone-*.md` files already
use.

A plan is implemented in one or more **phases**, and each phase is one pull request:

- **One phase** → one branch, one PR.
- **Multiple phases** → stacked PRs, one branch per phase, each based on the one below it. Use the
  `gh-stack` skill to create, push, rebase, and sync the stack.

**All changes land through pull requests. Never commit directly to `main`** — always branch first.

**Never assume the status of a pull request.** Whether a PR is open, merged, closed, approved, or
green in CI is only knowable by asking: run `gh pr view <n>` / `gh pr status` / `gh pr checks <n>`
before acting on that status or reporting it.

Milestones 1–7 predate this convention and are recorded under `notes/` instead:
[notes/status.md](notes/status.md) is the milestone history, `notes/milestone-*.md` the
per-milestone plans, and [notes.md](notes.md) the protocol research and original plan. Keep
`notes/status.md` and the README's status section current as behavior changes.
