# Plan: pause holds the buffer instead of dropping it

- **Date:** 2026-08-01
- **Status:** awaiting approval
- **Scope:** the pause/resume path in the library's playback queue
  ([openairplay2/src/player.rs](../openairplay2/src/player.rs)). No protocol
  additions, no PTP, no receiver-binary changes expected.

## Background

Testing from an iPhone (post embeddable-library stack, PRs #15–#17) shows:

1. Start plays immediately. ✓
2. Pause silences immediately. ✓
3. **Resume takes many seconds** before audio returns.
4. **Sometimes audio resumes at the wrong position**, as if timing is
   confused.
5. **Sometimes it stutters** until it "catches up".
6. The iPhone's Now Playing widget can show a track that ended a while ago.
7. Deep into a Music.app playlist (several tracks past the starting one),
   a single pause/resume can restart playback at the **original track from
   ~15 minutes earlier**.

It is random and hard to reproduce — which fits the diagnosis below, because
the failure size depends on how much audio happened to be in flight and on
how much divergence has accumulated over the session.

### Root cause

The milestone-6 pause gate **drops** audio: on pause-engage it discards the
~2 s decoded queue, and while paused it discards every packet still arriving
over TCP. Because we discard instantly, the queue stays empty, the
backpressure never engages, and the reader keeps draining the socket — so the
sender keeps streaming into the 8 MB buffer we advertised at `SETUP`.

The sender's model is now wrong: it believes everything up to its send cursor
is safely buffered at the receiver (that is the point of *buffered* audio — it
does not expect to resend it). On resume:

- the audio between the pause point and the send cursor is simply gone →
  seconds of silence (symptom 3), then playback resumes at the send cursor's
  position, ahead of the pause point (symptom 4);
- the sender streams at roughly real-time pace (it thinks we are deeply
  buffered), so we play from a near-empty queue and underrun on any jitter
  (symptom 5);
- the phone's sender-side timeline arithmetic ("what the receiver has
  buffered and when it will finish") diverges from reality by however much we
  discarded, which is what a stale Now Playing widget looks like (symptom 6 —
  there is no channel by which a receiver corrects the sender's UI; the only
  fix is keeping flow control honest).

**The divergence accumulates.** Each drop makes the *audible* position jump
ahead of the sender's model by the dropped span — the phone believes that
span is still queued in our advertised 8 MB buffer, to be played at
real-time rate, and nothing ever corrects it. A Music.app playlist streams
gaplessly over one buffered session with no teardown between tracks, so the
error compounds across track boundaries: after enough pause/resume cycles
the listener is audibly tracks ahead of the phone's timeline (symptom 6
again). A later pause/resume then resumes **from the phone's believed
position** — Music.app re-anchors and re-sends from where *it* thinks
playback is — which lands tracks back in the past (symptom 7). The backward
jump is the accumulated divergence being repaid at once.

Confirming observation: immediately after such a backward jump, the Now
Playing widget matches audible playback **exactly** (title and seek
position). The widget was never malfunctioning — it faithfully reports the
phone's model; the jump re-synchronizes reality to that model, so the two
agree again until the next drop re-diverges them. This is the behavior of a
sender whose model is authoritative and a receiver that has been silently
discarding — not of a clock/timing (PTP) problem.

### What an INFO-level capture of the repro shows

A log of the failing session (iPhone, Music.app playlist, 2026-08-01)
establishes two facts:

- **The iPhone's pause always includes the flush**: every pause is a
  `SETRATEANCHORTIME` + `FLUSHBUFFERED` pair; every resume is a lone
  `SETRATEANCHORTIME`. (An earlier draft guessed the flush was sometimes
  missing — wrong.)
- **The `flushUntilSeq` boundary filter is discarding wanted audio.** The
  ~17-minute playlist session ended with `buffered audio disconnected
  (0 decrypt failures, 2006 skipped)` — 2006 packets × 1024 samples ≈
  **47 seconds of audio** silently dropped by the reader's
  `seq < flushUntilSeq` check. The sender re-sends buffered audio after a
  resume with sequence numbers below our stored boundary, and two defects
  turn that into data loss:
  1. the boundary is **sticky** — set on every flush and never reset, so a
     re-send below the latest boundary is discarded until the seq catches
     up to it (the delayed/mispositioned resume, measured);
  2. independent of the boundary, our flush handling **drops the entire
     decoded queue** via the generation bump, rather than discarding
     exactly what the flush asked for (`seq < flushUntilSeq`) — correct
     for forward skips (everything queued is below the boundary), wrong
     for a pause-flush where buffered-ahead audio at/after the boundary
     should be retained.

### What a DEBUG-level capture of one isolated cycle shows

A `RUST_LOG=debug` capture of start → 9 s play → pause → 3 s → resume
(iPhone, 2026-08-01) measures the failure directly:

- **Pause can be flush-less.** This pause was a bare `rate=0` — no
  `FLUSHBUFFERED` anywhere in the cycle (the playlist log showed
  pause+flush pairs; both flavors exist). A flush-less pause gives the
  receiver no licence to discard anything: **hold is mandatory**.
- **The resume anchor is the exact pause position.** Resume was `rate=1`
  with an anchor 353,514 samples ≈ 8.0 s after the start anchor, matching
  the pause point; no packets were re-sent (no skipped-seq lines,
  continuous stream). The sender simply expects the buffered-ahead audio
  it already sent to still be there.
- **Post-resume starvation, measured:** after resume the player received
  only ≈ 9.5 s of audio in 14 s of wall time — the dropped span is waited
  out (silence for a few seconds), then playback resumes at the sender's
  send cursor (ahead of the pause point) from a thin, real-time-rate queue
  (the stutter).

**This is not a PTP issue.** PTP aligns multiple outputs to a shared clock;
every symptom above is a single-output flow-control problem. The two
capture levels together cover both pause flavors, and the design below
handles each: flush-less pause → hold everything; pause/skip with flush →
discard exactly what `flushUntilSeq` names, keep the rest.

## Design: hold, don't drop

Pause becomes a *freeze* of the pipeline instead of a *discard*:

- **Pause engage (`SETRATEANCHORTIME rate=0`):** silence the sink immediately
  (`AudioSink::flush`, as today — the sink discards its own device/prebuffer
  state), but **retain** every queued packet, and keep counting held samples
  in the backpressure counter. The playback thread parks arriving PCM in a
  hold buffer instead of dropping it.
- **Backpressure does the protocol work:** with held samples still counted,
  `pending_samples()` stays above the high-water mark, the TCP reader stops
  reading, TCP flow control pushes back, and the sender's send cursor freezes
  close to the pause point. The sender's "what is buffered at the receiver"
  model stays true.
- **Resume (`rate=1`):** deliver the held audio to the sink first, then
  continue normally. Playback restarts instantly (the sink's ~0.5 s prebuffer
  refills from the ~2 s we hold), at the right position, with a full buffer —
  no gap, no jump, no stutter.
- **Seek/skip (`FLUSHBUFFERED`) becomes boundary-accurate:** discard exactly
  what the flush asks for — packets with `seq < flushUntilSeq` — from the
  decoded queue, the hold buffer, and the still-arriving TCP stream, and
  retain everything at/after the boundary. This requires stamping decoded
  PCM with its packet sequence number (replacing or augmenting the
  generation stamp). The boundary must also stop being sticky: a re-send at
  or above the last boundary plays; the measured 47 s of discarded re-sent
  audio must go to zero. Forward skips behave as today (everything queued is
  below the boundary); pause-flushes stop destroying buffered-ahead audio.

### Mechanics (playback thread)

The queue and control share one mpsc channel, so the paused thread keeps
receiving: `Pcm` commands are pushed to a local hold buffer **without**
decrementing the pending counter (that is what keeps backpressure engaged);
`Wake` re-checks the pause/flush flags. On resume, the hold buffer is played
(decrementing pending as it drains, stale generations discarded) before
returning to the normal loop. On flush, held packets with stale generation
stamps are dropped and their samples subtracted from pending.

### Accepted tradeoffs

- The `AudioSink::flush` on pause still discards up to ~0.5 s already handed
  to the hardware/prebuffer, so resume can skip forward by that much at most.
  Acceptable; revisit only if audible.
- The resume anchor's `rtpTime` is still not honored (we resume from the held
  queue head, not from the anchor frame). If hardware testing after this
  change still shows a position offset, a follow-up phase threads per-packet
  RTP timestamps through decode → queue and trims held audio older than the
  anchor. Deliberately contingent — not built until proven necessary.

## Out of scope

- PTP / multi-room (unchanged project scope).
- Sender-side UI correctness beyond what honest flow control restores.
- Any receiver-binary (`AlsaSink`) changes.

## Phases

Stack layout per the plan+implementation-in-one-stack convention: this plan
is the bottom PR; implementation PRs stack on top after approval.

### Phase 1 — hold-don't-drop + boundary-accurate flush

Rework the playback thread as designed above (hold on pause; stamp decoded
PCM with its packet seq; flush discards exactly `seq < flushUntilSeq` and the
boundary is not sticky); update the pause unit tests (pause currently
*asserts* dropping — it will assert holding instead) and add new ones.
Library-only. Before implementation details are finalized, a `RUST_LOG=debug`
capture of one pause/resume cycle pins down the exact boundary semantics
(see open questions).

### Phase 2 — anchor trimming (contingent)

Only if the phase-1 hardware check still shows resume-position error: carry
RTP timestamps into the queue and trim held audio older than the resume
anchor's `rtpTime`.

## Test strategy

- Fake-sink unit tests (the deterministic gated recorder from PR #15):
  - pause → play N → resume delivers all N in order (today they are dropped);
  - `pending_samples()` stays high across a pause (the backpressure signal)
    and drains after resume;
  - flush during pause discards held audio below the boundary and subtracts
    it from pending, and retains held audio at/after the boundary;
  - pause still calls `AudioSink::flush` immediately (silence);
  - a re-send at/above the last flush boundary is played, not skipped (the
    sticky-boundary regression test);
  - resume after flush-during-pause plays only post-boundary audio.
- Hardware (iPhone, the original repro): start → immediate; pause →
  immediate silence; resume → immediate, correct position, no stutter;
  Now Playing widget stays accurate through several pause/resume cycles;
  seek/skip and volume unchanged. A `RUST_LOG=debug` capture of one cycle
  before/after to confirm the sender-behavior analysis (does the iPhone's
  pause include `FLUSHBUFFERED`; does the packet counter keep climbing while
  paused today and freeze after the fix).

## Acceptance criteria

- `cargo test && cargo clippy --all-targets && cargo fmt --check` green on
  the workspace; `cargo test -p openairplay2` green alone.
- The five-step iPhone repro no longer shows delayed/mispositioned/stuttery
  resume; the widget stays in sync.
- Seek/skip behavior is unchanged on hardware.

## Open questions

- The exact `flushUntilSeq` value the iPhone uses on a pause-*with*-flush
  (play position vs send cursor) is still unobserved — but no longer
  blocking: boundary-accurate handling obeys whichever the sender names,
  and the flush-less pause (now confirmed) is handled by holding. A debug
  capture of a pause+flush cycle during implementation would still be nice
  for the test fixtures.
- Whether the ~2 s hold is enough for very long pauses — the sender may
  eventually tear the session down on its own timeline; observe during
  hardware testing.
