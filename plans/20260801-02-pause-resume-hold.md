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

The randomness: Mac captures from milestone 6 showed pause arriving as
`rate=0` **plus** `FLUSHBUFFERED`; when a flush accompanies the pause,
discarding is exactly what was asked and resume works. The iPhone evidently
does not always send the flush — then discarding is exactly wrong.

**This is not a PTP issue.** PTP aligns multiple outputs to a shared clock;
every symptom above is a single-output flow-control problem.

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
- **Seek/skip (`FLUSHBUFFERED`) is unchanged:** the flush generation still
  discards queued *and held* stale packets, and `flushUntilSeq` still drops
  still-arriving packets by plaintext sequence number. A sender that pauses
  with an explicit flush therefore still gets discard semantics — the flush
  says so; mere pause no longer implies it.

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

### Phase 1 — hold-don't-drop

Rework the playback thread as designed above; update the pause unit tests
(pause currently *asserts* dropping — it will assert holding instead) and add
new ones. Library-only.

### Phase 2 — anchor trimming (contingent)

Only if the phase-1 hardware check still shows resume-position error: carry
RTP timestamps into the queue and trim held audio older than the resume
anchor's `rtpTime`.

## Test strategy

- Fake-sink unit tests (the deterministic gated recorder from PR #15):
  - pause → play N → resume delivers all N in order (today they are dropped);
  - `pending_samples()` stays high across a pause (the backpressure signal)
    and drains after resume;
  - flush during pause discards held audio and subtracts it from pending;
  - pause still calls `AudioSink::flush` immediately (silence);
  - resume after flush-during-pause plays only new-generation audio.
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

- Whether the iPhone's pause ever includes `FLUSHBUFFERED` (capture will
  tell); the design is correct either way, since flush keeps its discard
  semantics.
- Whether the ~2 s hold is enough for very long pauses — the sender may
  eventually tear the session down on its own timeline; observe during
  hardware testing.
