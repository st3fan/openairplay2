# Plan: turn openairplay2 into an embeddable library

- **Date:** 2026-08-01
- **Status:** implemented (stacked PRs open; hardware validation pending)
- **Scope:** this repository only. The eventual embedding into
  [st3fan/radio](https://github.com/st3fan/radio)'s `radiod` motivates the
  design but is explicitly not part of this plan; that integration happens
  later, in the radio repository.

## Background

openairplay2 is a working single-stream AirPlay 2 receiver, currently shaped
as one crate where the library target exists mainly so the integration tests
can reach the modules — `lib.rs` re-exports everything `pub`, and `main.rs`
is a thin CLI over it. There is no designed embedding API: the protocol code,
the ALSA playback path, and the CLI are all one unit, and the crate only
builds on Linux because the `alsa` dependency is unconditional.

We want to embed the AirPlay 2 receiver into another daemon (radiod), which
has its own audio output path, its own volume model, and its own idea of what
to do when a session starts. That requires a real library boundary.

At the same time, the standalone receiver stays a first-class artifact of
this repo: `openairplay2-receiver`, a Linux-only, ALSA-only binary that is a
useful AirPlay 2 receiver on its own. It is deliberately *not* generic over
audio backends — the library deals in PCM, the receiver deals in ALSA.

## Goals

- A library crate whose public API is a designed embedding surface:
  configuration in, decoded PCM + session events out. No AirPlay wire
  concepts (plists, shk, sequence numbers) leak to the host.
- The library builds and its tests pass on macOS and Linux (no `alsa`
  dependency in the library).
- A separate `openairplay2-receiver` binary crate in the same workspace:
  Linux-only, ALSA-only, functionally identical to today's binary. This is a
  main artifact, not an example.
- Behavior validated against a real Mac is preserved at every phase: pairing,
  buffered AAC playback, pause/resume, seek/skip, volume, and the
  backpressure-based timing model.

## Non-goals

- No radiod integration work (later, in the radio repo).
- No new protocol features (no metadata/artwork, no pair-verify, no realtime
  stream decode — unchanged scope).
- No generic audio backend abstraction in the receiver binary; it is ALSA
  only.
- No crates.io publication (git dependency is sufficient for now; publishing
  can be its own step later).

## Design

### Where the seam is

The library owns **network to PCM**: discovery advertisement (optional),
pairing, the encrypted control channel, fp-setup, SETUP, the buffered-audio
channel, decrypt, and AAC decode. The host owns **PCM to speaker**: output
device, pacing against the hardware, and gain.

Concretely, today's `player.rs` splits in two:

- The **queue + dedicated playback thread + backpressure counter** stay in
  the library. The TCP reader's throttle on queued samples is the entire
  timing model (no PTP); it cannot move out. The pause gate and the
  flush-generation dropping also stay in the library — they are session
  semantics (the Mac keeps sending buffered-ahead audio during a pause; a
  seek must preempt ~2 s of queued audio), and every host would otherwise
  have to reimplement them identically.
- The **ALSA output and gain application** move to the host side, behind a
  sink trait. The receiver binary's sink is today's `AlsaOutput` (open,
  blocking `writei`, `drop`+`prepare` reset, prebuffer cushion) plus
  `apply_gain`.

### The sink trait

```rust
/// Called from a dedicated library-managed thread; `write` may block —
/// blocking is the pacing mechanism.
pub trait AudioSink: Send + 'static {
    fn write(&mut self, pcm: &[i16]);
    /// Seek/skip: immediately drop anything the sink has queued or buffered.
    fn flush(&mut self);
}
```

The library calls `write` only with audio that should actually play: it does
not deliver PCM while paused, and it drops pre-flush audio itself (both the
already-queued generation-stamped packets and the still-arriving TCP packets
below `flushUntilSeq`). `flush` exists because the sink may hold hardware or
prebuffer state of its own that must be discarded on seek (for ALSA:
`snd_pcm_drop` + `prepare`).

The library does **not** apply gain. Volume arrives at the host as an event
(dB, as sent by the sender), and the host applies it in its own gain path.
The receiver binary keeps the current dB→linear mapping and `apply_gain`
behavior; an embedding host (radiod) maps it onto its own volume model.

### Events

```rust
pub enum Event {
    /// SETUP phase 2 completed; a sink is about to be used.
    SessionStarted { rate: u32, channels: u8 },
    /// SET_PARAMETER volume, in AirPlay dB (0 = full, −144 = mute).
    Volume { db: f32 },
    /// SETRATEANCHORTIME rate gate.
    Paused(bool),
    /// FLUSHBUFFERED (seek/skip). Informational: the library already
    /// dropped its queue and called `AudioSink::flush`.
    Flushed,
    /// TEARDOWN or connection closed.
    SessionEnded,
}
```

Delivered over a channel (`tokio::sync::mpsc`) so the host consumes them at
its own pace. Metadata/artwork would slot in here later as new variants —
out of scope now, but the enum is `#[non_exhaustive]` from day one.

### Facade

```rust
let receiver = Receiver::builder()
    .name("Office")
    .port(7000)
    .identity_path("...")     // or .identity(Identity)
    .advertise(true)          // false: host owns mDNS; txt_records() exposed
    .build()?;

receiver.run(sink_factory, event_tx).await?;   // runs on the caller's runtime
```

- `sink_factory: impl Fn(u32, u8) -> Box<dyn AudioSink>` — invoked at SETUP
  phase 2 with the negotiated rate/channels, once per stream.
- The library never creates a tokio runtime; it runs inside the caller's.
  The receiver binary keeps `#[tokio::main]`.
- Advertisement is optional: an embedding host may own all of its Avahi
  registration. `txt_records(&config, &identity)` stays public so a
  self-advertising host gets the records right. The `avahi` module (zbus)
  stays in the library — zbus compiles on macOS, only `alsa` does not.

### Crate layout

Cargo workspace, two members:

```
Cargo.toml                    # workspace
openairplay2/                 # the library (no alsa dependency)
  src/{lib,server,session,pairing,srp,cipher,crypto_stream,http,tlv,
       fairplay,identity,info,avahi,mac,buffered,decode,...}.rs
  tests/                      # integration tests (synthetic sender), data/
openairplay2-receiver/        # the binary (Linux-only: alsa)
  src/{main,player}.rs        # CLI, AlsaSink (AlsaOutput + gain + prebuffer)
```

- Library `tokio` features trimmed to what it needs (`net`, `io-util`,
  `sync`, `time`, `macros` for tests); `rt-multi-thread` and `signal` move
  to the receiver.
- Protocol modules become `pub(crate)` except the designed surface
  (`Receiver`, builder, `AudioSink`, `Event`, `Identity`, `Config`,
  `txt_records`). The integration tests need sender-side pieces
  (`SrpClient`, `sender_control_channel`, `Tlv`); these stay `pub` but
  `#[doc(hidden)]` — usable by tests and by a future test-sender, absent
  from the documented API.
- The identity file format, CLI flags, defaults (features bitmask, port
  7000, `~/.config/openairplay2/identity`) are unchanged in the receiver.

### What deliberately stays inside the library

Session lifecycle, two-phase SETUP, `flushUntilSeq` handling, packet
decrypt, AAC decode, the pause gate, the queue/backpressure thread, the
GET_PARAMETER/SET_PARAMETER volume bookkeeping (including answering the
`volume` query — a sender aborts on an empty reply). Hosts see PCM and
`Event`s, nothing else.

## Phases

Multi-phase plan → stacked PRs via the `gh-stack` skill, per CLAUDE.md.
Each phase leaves the tree green (`cargo test`, `clippy`, `fmt`) and the
receiver verifiable against a real Mac.

### Phase 1 — cut the seam in place

Inside the existing single crate: introduce `AudioSink` and `Event`;
split `player.rs` into the library-side queue/pacing/gating half and an
`AlsaSink` (output + gain + prebuffer) used by `main.rs`; thread volume
through as an event consumed by the binary instead of a lib-applied gain.
No workspace change yet, no public-API change. Behavior identical —
verified against a real Mac (pause/resume, seek, volume, latency feel).

### Phase 2 — workspace split

Create the workspace; move `main.rs` + the ALSA sink into
`openairplay2-receiver`; drop `alsa` from the library; trim tokio features.
CI-able check: the library builds and tests pass on macOS *and* Linux; the
receiver builds on Linux and behaves identically on hardware.

### Phase 3 — the public facade

Add `Receiver`/builder/`run()`; make the binary consume only the public
API (it is embedder #1); tighten visibility (`pub(crate)` +
`#[doc(hidden)]` as above); make `advertise(false)` + `txt_records()` work
for host-owned mDNS; write rustdoc for the public surface and update
README + CLAUDE.md for the workspace layout. Final hardware validation.

## Test strategy

- Existing unit tests move with their code; the golden AAC fixture test
  stays in the library.
- The integration tests (`tests/info.rs`, `tests/pairing.rs`) keep running
  against the library — after phase 3, through the public facade where
  possible.
- New unit tests: the library-side queue thread delivers to a recording
  fake `AudioSink` (order, pause drops delivery, flush calls
  `AudioSink::flush` and drops queued PCM, backpressure counter rises and
  falls). This fake is also the seed for host-side testing later in radiod.
- macOS: `cargo test -p openairplay2` must pass (this is new — the crate
  currently cannot build here).
- Hardware (per phase): a real Mac pairs, streams, pause/resume, skip,
  volume slider live, no audible regression in start latency.

## Acceptance criteria

- `cargo test && cargo clippy --all-targets && cargo fmt --check` green on
  the workspace (Linux) and for the library alone (macOS).
- `openairplay2-receiver` is drop-in equivalent to today's binary: same
  flags, same identity file, same defaults, validated on hardware.
- The library's documented public API contains no ALSA and no AirPlay wire
  types; a host can embed it with: builder → `run(sink_factory, events)`.
- README documents the two artifacts; CLAUDE.md reflects the workspace.

## Open questions (to resolve during implementation)

- Whether `Receiver::run` should also accept a caller-provided shutdown
  signal (`CancellationToken`-style) rather than relying on drop — decide
  when the facade lands in phase 3.
- Exact tokio feature set for the library once the split is real.
- Whether `SessionStarted` should carry the peer address (useful for a
  host UI showing "AirPlay — Stefan's MacBook"; the connection knows it).
