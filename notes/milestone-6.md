# Milestone 6 — Soft timing: pause/resume, seek, bounded latency (no PTP)

Goal: make single-stream playback **correct and robust** — honor transport
control (pause/resume, seek) and bound latency — **without** implementing PTP.

## Why no PTP

The use case is deliberately narrow: **one Mac, one stream, one receiver.** No
multi-speaker, no grouped rooms, no lip-sync to video. Full AirPlay 2 timing
(IEEE-1588 PTP on UDP 319/320, disciplined to the sender's grandmaster clock)
exists to align *multiple* outputs to a shared timeline. With a single output
there is nothing to align to, so PTP buys us nothing here.

We already don't run it: SETUP phase 1 replies `timingPort: 0` and we never
bind 319/320. The Mac streams buffered AAC anyway. This milestone keeps it that
way and makes the design explicit.

**Why drift isn't a problem for one device.** In buffered mode the Mac pushes
audio ahead into our buffer and we drain it at our sound card's rate. TCP
backpressure + the Mac's capped lookahead make the system self-regulating: it
can't overrun (we stop reading when our buffer is full, the Mac's TCP send
blocks) and it won't underrun under normal jitter (we keep a cushion). The only
residual effect of the card's crystal not exactly matching the Mac's is a
constant sub-0.1% pitch offset — inaudible, and with no second device there is
no reference against which it could even be noticed. So: **no resampler, no
clock discipline.** The Mac's buffer is the clock.

## Scope

In:

- **Pause / seek / skip via out-of-band flush** (`session.rs`, `player.rs`):
  the Mac drives all of these with `FLUSHBUFFERED` (pause = `rate=0` + flush;
  skip = flush + a new anchor; resume = fresh audio with a new `rtpTime`). The
  hard part is that a flush must **preempt the ~2 s audio buffer** — an in-band
  command sits behind it and only acts seconds later (the milestone-6 v1 bug).
  Fix: a `flush_gen` atomic the control path bumps instantly; every queued
  packet is stamped with its generation; on a flush the player drops all
  stale-stamped packets (microseconds, not played) and `drop`+`prepare`s the
  device. `SETRATEANCHORTIME rate=0` also flushes so audio stops even if no
  `FLUSHBUFFERED` follows; `rate=1` needs nothing (fresh audio just plays).
- **Bounded latency** (`player.rs` + the buffered reader): the player exposes
  how many frames are queued; the TCP reader backpressures (stops reading, so
  the Mac's send blocks) when the queue exceeds a target. This replaces the
  fixed 20-packet prebuffer with a frame-based cushion, caps latency/memory,
  and provides the underrun cushion. This *is* the soft-timing mechanism.
- **Docs**: README + this note stating the single-stream, no-PTP design.

Note: `snd_pcm_pause` is **not** used — it fails with EBADFD on real hardware
devices like `front`. `drop` + `prepare` (discard buffered frames) is the
portable way to stop output immediately.

Out: PTP / 319-320, multi-room, grouped playback, AV lip-sync, resampling /
sample-rate conversion, precise per-sample scheduling against network time.

## Control messages (captured from a real Mac, milestone-5 log)

- `SETRATEANCHORTIME` body (plist):
  `{rate: 1, networkTimeTimelineID: <clock id>, networkTimeSecs, networkTimeFrac,
    networkTimeFlags: 0, rtpTime: 3174381381}`. We use `rate` and `rtpTime`;
  the network-time fields would only matter with a PTP clock, so we ignore them.
- `FLUSHBUFFERED` body (plist): a flush range (`flushFromSeq`/`flushUntilSeq`
  or timestamps). Minimal handling: flush everything buffered.
- `SETPEERS` / `RECORD`: carry the PTP peer addresses — irrelevant without PTP,
  still `ack`ed.

## Module layout

```
src/player.rs  — Command::{Rate,Flush}; pending-frame counter; pause/flush;
                 frame-based prebuffer + backpressure signal
src/session.rs — set_rate_anchor()/flush(); hold a PlayerSender for control;
                 reader backpressures on the pending count
src/server.rs  — route SETRATEANCHORTIME -> set_rate_anchor, FLUSHBUFFERED -> flush
```

## Test strategy

- **Unit**: parse the real `SETRATEANCHORTIME` plist -> `(rate, rtpTime)`; parse
  a `FLUSHBUFFERED` body; the prebuffer/backpressure state machine as a pure
  decision (queued vs thresholds, started flag -> start / write / hold); pause
  state transitions.
- **Manual (you-run-it)**: play, then **pause/resume on the Mac** stops and
  resumes promptly; **skip a track** doesn't play stale audio; a long session
  neither underruns (clicks) nor grows unbounded latency.

## Acceptance criteria

- `cargo test` / `cargo clippy` clean, no hardware.
- Pause/resume and flush parsing covered by unit tests; latency bound tested.
- Hardware: pause/resume and track-skip behave; long playback stays clean.
