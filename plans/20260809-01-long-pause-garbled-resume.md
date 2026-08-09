# Explore: garbled audio after resuming a multi-minute pause

- **Date:** 2026-08-09
- **Status:** plan — investigate → confirm → fix (root cause not yet confirmed
  on hardware; this plan names a prime suspect and how to prove or disprove it)
- **Scope:** the pause/resume path for pauses lasting *minutes*, receiver
  binary's ALSA sink first
  ([openairplay2-receiver/src/player.rs](../openairplay2-receiver/src/player.rs)),
  library pipeline second if the capture points there. Short pauses, seek,
  track switch and takeover must stay exactly as they are.

## Symptom (as reported)

Playing from an iPhone, pause for **a few minutes**, then resume:

1. Audio comes back **completely garbled** — chaotic, high-pitched, shredded.
2. Then **silence**.
3. Then, **once in a while, the same chaotic playback** again — the session
   never recovers on its own.

Short pause/resume cycles are fine (verified on hardware for
plans/20260801-02 and again for plans/20260808-01). The failure needs the
pause to be *long*.

## Why this is probably a regression from the never-stop stream

Two prior changes shaped this path:

- **plans/20260801-02** made pause *hold* audio: the library queue parks
  everything, `pending_samples()` stays high, the TCP reader backpressures,
  and the sender's cursor freezes. On resume the held ~2 s plays first. That
  design was verified for short pauses; its own open question — "whether the
  ~2 s hold is enough for very long pauses" — was left to observe.
- **plans/20260808-01** (PR #122, 2026-08-08 — the day before this report)
  reworked the sink to keep the ALSA stream **running forever**:
  `stop_threshold = boundary` (never stop on underrun), `silence_size =
  boundary` (unwritten space plays silence), no `snd_pcm_drop`, and
  pause/flush silences by **rewinding** the unplayed audio
  (`snd_pcm_rewind`). That killed the pause/resume pops.

The combination creates a state the old `drop`+`prepare` sink could never
enter, and it matches every reported symptom.

### H1 (prime suspect): ALSA application/hardware pointer divergence

With the stream configured never to stop, a pause looks like this at the
device:

1. Pause → `AlsaSink::flush` → `snd_pcm_rewind` pulls the **application
   pointer** back to the play position. No further writes arrive (the library
   holds everything).
2. The stream keeps running. The **hardware pointer** advances in real time
   for the whole pause, playing the silence fill. The app pointer stands
   still.
3. After a few minutes the hardware pointer is *minutes* past the app
   pointer. `avail` exceeds the buffer size (ALSA's "avail overrange" /
   negative-delay underrun-while-running state — exactly the state
   `stop_threshold = boundary` asks for, and nothing in
   [`AlsaOutput::write`](../openairplay2-receiver/src/player.rs) resynchronizes).
4. Resume → the held audio is written at the stale app pointer, i.e. **into
   the past**. Frames land in ring positions the hardware already considers
   played.

That predicts the reported behavior precisely:

- **Garble, high-pitched and chaotic:** writes into the past are consumed
  instantly; only the fragments that happen to land where the hardware
  pointer is sweeping become audible — shredded discontinuous audio, which
  reads to the ear as harsh/high-pitched chaos.
- **Then silence:** because writes never block, the entire ~2 s held queue is
  consumed at effectively infinite speed. The queue drains, the pacing that
  blocking `writei` is supposed to provide is gone, and mostly-silence plays.
- **Recurring chaos, never recovering:** the sender streams on at real-time
  pace, so the app pointer advances at real time — the *same* rate the
  hardware pointer advances. The gap between them never closes; every packet
  is written into the past forever, surfacing as occasional audible bursts.
  Backpressure also collapses (writes don't block → `pending_samples()`
  hovers near zero → the reader never sleeps), so the library's ~2 s cushion
  is gone too.

A short pause survives because the divergence is small: the hardware pointer
advances only ~0.5 s (one buffer) into the silence fill before... no — it
advances the full pause length, but for a pause of a few *seconds* the first
blocking-write retry pattern and the buffer-sized gap produce at most a brief
skip that the fade-in and the held cushion mask. Minutes make it unmissable.
(The capture below measures rather than trusts this hand-waving.)

### H2: the sender gives up or re-keys during the long pause

The iPhone may not idle politely for minutes: it may tear the stream down
(`TEARDOWN`), close the data TCP connection (which ends `buffered_audio` —
it accepts exactly once), or re-`SETUP` with a fresh `shk` on resume. Any of
those would leave the current session playing held audio into a dead pipeline
or decrypting new packets with an old key (decrypt failures → silence). This
does not obviously produce *garble*, but the capture must establish what the
sender actually does across a multi-minute pause before any fix is trusted.

### H3: backpressure counter or hold-buffer pathology over minutes

While paused, `pending` holds ~2 s of samples and the reader sleeps in 5 ms
ticks — that part is bounded and fine. But if the sink's non-blocking-write
collapse (H1) or a re-send epoch (H2) ever double-subtracts `pending`
(`usize`), it wraps to a huge value and the reader backpressures **forever**
→ permanent silence. A cheap thing to rule in/out from the same capture (log
`pending_samples()` at resume and periodically after).

## Investigation steps

1. **Reproduce with `RUST_LOG=debug` on hardware** (iPhone → skynet):
   play ~30 s → pause → wait 3–5 minutes → resume → let it run 1 minute.
   Capture the full log. This is the decisive artifact; everything below
   reads from it.
2. **What the sender did** (H2): during the pause and at resume, list every
   RTSP method received (`SETRATEANCHORTIME`, `FLUSHBUFFERED`, `TEARDOWN`,
   `SETUP`, keep-alives), whether the data connection stayed open (the
   `buffered audio disconnected` line), and whether a new `shk`/stream was
   set up.
3. **What the device did** (H1): add temporary (or permanent, behind
   `debug!`) instrumentation to `AlsaOutput`: log `snd_pcm_avail`,
   `snd_pcm_delay` and the PCM state at `discard()` (pause), and at the first
   `write()` after a discard. H1 predicts `avail` far above the ~0.5 s buffer
   size and/or `writei` returning without blocking at resume.
4. **Library-side health**: at resume, log `pending_samples()` and the rate
   packets are handed to the sink. H1 predicts the held queue draining near
   instantly; H3 predicts a wrapped counter; a healthy library with a sick
   sink pins the fix to the receiver binary.

## The likely fix (if H1 confirms)

Resynchronize the application pointer with the hardware pointer whenever they
have diverged past the ring, before writing:

- In `AlsaOutput::write` (or `discard`), read `snd_pcm_avail`; when it
  exceeds the buffer size (overrange — the only way that happens is the
  running-underrun state), `snd_pcm_forward` the app pointer by
  `avail − buffer_size` (plus nothing else: landing exactly at "buffer
  empty, hw pointer here" restores normal blocking-write pacing and the
  ~0.5 s cushion refills from the held audio).
- The resync arithmetic (avail, buffer size → frames to forward) goes in a
  pure function with unit tests; the FFI call sits next to the existing
  `rewind` and is exercised by the existing `null`-device round-trip test
  (write → rewind → write already runs there; add forward to the sequence —
  the `null` device cannot *time-shift*, so the divergence itself is
  hardware-verified, not CI-verified).
- The fade-in already re-arms on flush, so the resumed audio still ramps —
  no new click at the resync boundary.

Explicitly **not** the fix: `snd_pcm_drop`/`prepare` on resume (reintroduces
the pop that plans/20260808-01 removed), or stopping the stream during pause
(same pop, and it re-opens the device-grab gap of issue #110).

If the capture instead confirms H2 (sender re-keys / reconnects), the fix
moves to the library (accept a reconnection on the data listener, or handle
the re-`SETUP` path) and gets its own phase — shape decided by what the
capture shows, recorded here before implementing.

## Phases

Stack layout per convention: this plan is the bottom PR; implementation PRs
stack on top after approval.

### Phase 1 — capture and confirm

Hardware capture per the investigation steps, plus the `AlsaOutput`
instrumentation (kept, behind `debug!` — this class of bug will recur and
the numbers are cheap). Findings recorded in this document on its still-open
branch: which hypothesis held, with the log excerpts that prove it.

### Phase 2 — the fix

For H1: the pointer-resync in `AlsaOutput` as designed above, receiver
binary only. For H2/H3: scoped by phase 1's findings, recorded here first.

**Both phases were built before the capture**, at Stefan's request, so one
hardware session can test the measurement and the fix together: the phase-1
build alone prints the divergence (and still garbles), the phase-2 build on
top prints it and corrects it. That ordering does not change what the
capture has to establish — if the numbers show the application pointer
staying level with the hardware pointer across a long pause, H1 is refuted
and phase 2 is the wrong fix, however green its tests are.

### Implementation notes (recorded during the build)

- **The resync must be gated on the stream actually `RUNNING`.** Only a
  running stream advances its hardware pointer on its own; before it starts
  (`PREPARED` — where a session's *first* audio is written) there is no play
  position to be behind, and `avail` need not even be meaningful there.
  ALSA's `null` device reports `avail` = twice the buffer in `PREPARED`,
  which is how this surfaced: an ungated resync would have skipped the
  opening audio of every session. The `null`-device round-trip test now
  pins it.
- The check runs before **every** write, not only after a pause: any stall
  that empties the ring ends in the same state, and one `snd_pcm_avail` per
  ~3 ms chunk is cheap next to the `writei` beside it.
- A resync re-arms the sink's fade-in, so resumed audio ramps rather than
  clicking — the same treatment every other silence→audio boundary gets
  (plans/20260808-01).

## Test strategy

- **Unit:** the resync arithmetic pure function (overrange → forward frames;
  in-range → zero; exact-boundary cases). The `null`-device FFI round-trip
  extended with `snd_pcm_forward`.
- **Regression (must stay green):** the library player's hold/flush/
  backpressure tests; the receiver's handover-reuses-device and fade tests.
- **Hardware (acceptance):** the repro itself — pause 3–5 minutes, resume →
  clean immediate audio at the right position, correct pacing afterwards
  (no drift, backpressure numbers healthy). Also: pause ~10 minutes (does
  the sender even keep the session? record what it does); short pause/resume,
  seek, track switch, takeover — all still clean (no reintroduced pops).

## Acceptance criteria

- `cargo test && cargo clippy --all-targets && cargo fmt --check` green on
  the workspace; `cargo test -p openairplay2` green alone.
- The multi-minute pause/resume repro plays cleanly on hardware, and the
  short-cycle behaviors from plans/20260801-02 and plans/20260808-01 are
  unchanged (verified by ear on the same hardware).
- The capture's findings — whatever they are — are recorded in this plan
  before the fix PR merges.

## Out of scope

- PTP / multi-room (unchanged project scope).
- Sender sessions abandoned entirely during very long pauses (if the capture
  shows the iPhone tears down after some minutes, resuming *that* is a new
  feature — file an issue, don't fix it here).
- Realtime (type 96) streams, format negotiation, ALAC — unrelated.
