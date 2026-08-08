# Debug and fix the track-switch "tick"

A short audible artifact — a "tick" — plays when pausing, resuming and switching
tracks. This plan localized it and fixed it. Because the root cause was not known
up front, the plan was **investigate → confirm → fix**, and the record below
keeps the two wrong turns, because they are the reason the eventual fix is what
it is.

> **Resolved (2026-08-08).** The pops were the ALSA stream being **dropped and
> restarted** on every pause/flush/resume (the DAC muting/un-muting its analog
> output), confirmed by ticks on *both* pause and resume that a digital fade-in
> did not remove. Fixed by keeping the PCM stream **running** for the whole
> session (via `alsa-sys`: `stop_threshold = boundary`, silence-fill, and
> `snd_pcm_rewind` for immediate silence) — no `drop`, no restart, no pops.
> Verified on hardware (A/B against a real Mac): pause, resume and switch are
> clean. A residual artifact on *seek* remains but reproduces when the same soft
> electronic track streams to the iPhone's own output, so it is source/track
> related, not the receiver.

## Symptom (as reported)

On skip/next, a clear, very short artifact — "like a buffer that should not be
playing", possibly a fragment of the previous track. Later refined by the
listener: a tick on **pause**, a tick on **resume**, and a skip that sounds like
the two in quick succession.

## Background: what a skip did (before the fix)

A skip is `FLUSHBUFFERED` (flush + a new anchor), and on iOS/macOS it arrives as
**pause → flush → resume** (`SETRATEANCHORTIME rate=0`, `FLUSHBUFFERED`,
`SETRATEANCHORTIME rate=1`). The paths that touch audio:

- **[session.rs](../openairplay2/src/session.rs) `flush`** sets `flushUntilSeq`
  and signals the library player; arriving TCP packets below the boundary are
  also dropped pre-decrypt in [buffered.rs](../openairplay2/src/buffered.rs).
- **[player.rs](../openairplay2/src/player.rs) (library queue)** retains only
  queued packets at or above the boundary and calls `sink.flush()`, then writes
  the rest with blocking writes that pace playback.
- **The sink** ([openairplay2-receiver/src/player.rs](../openairplay2-receiver/src/player.rs)):
  `AlsaSink::flush` called `AlsaOutput::reset` = **`snd_pcm_drop` + `prepare`**,
  and the next audio did `prepare`/`start` — i.e. it **stopped and restarted the
  ALSA stream** on every pause/flush/resume.

## Hypotheses considered (ranked at the time)

- **H1 — off-by-one flush boundary** retains 1–2 old packets that play as the
  new track begins (the reported duration ≈ 1–2 AAC frames made this the lead).
- **H2 — old-track tail + drop-click:** ~one packet of old audio drains while
  the player is blocked in `writei`, then `snd_pcm_drop` cuts mid-waveform.
- **H3 — stale ring content:** `drop`+`prepare` leaves the ring un-zeroed.
- **H4 — underrun/recovery glitch** on the new track.
- **H5 — pure discontinuity click** (a sample step, no coherent old audio).

## What the investigation actually found

1. **H1 ruled out (debug-log capture).** A temporary log of the flush boundary
   and the first packet to play after it, over two real skips (Cups ↔ Jumbo),
   showed `dropped N, retained 0` and every arriving old packet
   `skipping seq <below boundary>`. **Nothing below the boundary reaches the
   audio path** — the library queue and `buffered.rs` are correct. Two fixes
   aimed at coherent old audio and the digital start step were tried and made no
   difference:
   - **small-chunk writes** in the library player, so a pause/flush is noticed
     within a few ms rather than a whole packet (shrinks any pre-drop tail), and
   - a **fade-in** on the first audio after a start/flush (de-clicks the digital
     silence→audio step).

   These stay — they are correct at the audio↔silence boundaries — but neither
   changed the tick.

2. **Root cause: stream stop/start pops.** The listener's decisive report — a
   tick on **both** pause and resume, unremoved by the digital fade-in — meant
   the artifact is **not** a sample discontinuity. It is the **ALSA stream being
   stopped (`snd_pcm_drop`) and restarted (`prepare`/`start`)** on every
   transition: many DACs mute the analog output when the PCM stream goes idle and
   un-mute on start, so each stop and each start is a pop, independent of the
   samples. A skip is pause+resume → two pops.

## The fix (implemented)

**Keep the ALSA stream running for the whole session — never `drop`, never
re-`start`.** The safe `alsa` wrapper exposes neither `snd_pcm_rewind` nor the
silence software-params this needs, so `AlsaOutput` now drives the PCM directly
through `alsa-sys`:

- opened once with `stop_threshold = boundary` (never stop on underrun),
  `silence_size = boundary` (fill unwritten space with silence, so a gap plays
  silence, not stale buffer content), and started once when the buffer fills;
- kept running thereafter — `AudioSink::write` just appends;
- `AudioSink::flush` (pause/skip) calls `snd_pcm_rewind` to discard the unplayed
  audio for **immediate silence without stopping the stream**.

No `snd_pcm_drop`, no restart → no mute/un-mute pops. The old prebuffer/`started`
machinery is gone (the start-threshold handles startup); the fade-in becomes a
cross-chunk ramp applied to the first audio after a start or flush.

## Test strategy (what was done)

- **The raw FFI is unit-tested without a sender or audio hardware:** a test
  opens ALSA's always-present, pure-software `null` device and runs the whole
  path — open → configure (silence/stop-threshold sw-params) → write →
  `snd_pcm_rewind` → write — so a broken FFI call fails in CI, not only on real
  hardware. The same path was checked to succeed on real `default`/`pipewire`.
- **Library regression:** the flush/boundary and pause-hold behaviour is pinned
  by the existing recording-fake-`AudioSink` tests, plus a test that a packet
  larger than the write chunk reassembles exactly across the small-chunk writes.
- **Fade:** `fade_in_progress` is unit-tested (ramps across chunks, then passes
  audio through untouched).
- **Hardware A/B (skynet + a real Mac):** two binaries — pre-fix (`drop`/restart)
  and post-fix (running stream) — compared by ear; the post-fix build has clean
  pause, resume and switch.

## Outcome

Met. Pause, resume and track-switch produce no pop on hardware; the FFI path is
covered in CI; the library and pause/flush tests still pass. The one residual —
an artifact on *seek* — is out of scope: it reproduces streaming the same track
to the iPhone's own output, so it is the source/track, not the receiver.

## Out of scope

- Resampling, 48 kHz/S24, ALAC — unrelated.
- The startup buffer-fill latency at the very first track (a separate tuning
  question) — this plan only removed the *artifact*.
- The source-related seek artifact (above).
