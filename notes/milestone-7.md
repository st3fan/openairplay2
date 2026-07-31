# Milestone 7 — Volume control

Goal: make the **Mac's volume slider actually change the volume.** We already
receive the volume and report it back, but never apply it, so playback is always
at full volume. This applies the negotiated volume as gain to the PCM.

## Background

The sender sets volume with `SET_PARAMETER` (`text/parameters`), a line like
`volume: -12.500000`. The value is in **dB**, AirPlay's convention:

- `0.0` = full volume
- down to about `-30.0` = quiet
- `-144.0` (a sentinel below the usable range) = **muted**

We parse this into `Session::volume` and echo it back for `GET_PARAMETER`, but
nothing applies it to the audio. The slider is cosmetic.

## Scope

In:

- **dB → linear gain** (`session.rs`): `gain = 10^(dB/20)` (standard amplitude
  mapping): `0 dB → 1.0`, `-6 dB → 0.5`, `-20 dB → 0.1`; `<= -144 dB → 0.0`
  (mute). Never amplify (clamp dB to `<= 0`).
- **Apply gain to PCM** (`player.rs`): scale each interleaved `i16` sample by
  the current gain just before the ALSA write. Gain lives in a shared atomic
  (f32 bits) so slider moves take effect live, mid-stream. Full volume is a
  no-op (skip the multiply).
- **Wire it up** (`session.rs`): on each `SET_PARAMETER volume`, push the new
  gain to the player; initialise the player's gain from the last-seen volume
  when the stream starts (a volume `SET_PARAMETER` often arrives before SETUP
  phase 2 creates the player).

Out: per-channel balance, dithering, volume curves/tapers beyond the plain dB
mapping, hardware/mixer volume (we scale in software).

## Module layout

```
src/player.rs  — shared volume atomic; apply_gain() before ALSA write
src/session.rs — volume_to_gain(dB); push gain on SET_PARAMETER + at stream start
```

## Test strategy

- **Unit**: `volume_to_gain` at 0 / −6 / −20 / −144 dB and the no-amplify clamp;
  `apply_gain` scales samples (half gain halves amplitude; gain 0 → silence;
  full gain is unchanged).
- **Manual (you-run-it)**: move the Mac's volume slider — playback volume
  tracks it, minimum is quiet, mute is silent, and it responds live.

## Acceptance criteria

- `cargo test` / `cargo clippy` clean.
- dB→gain and gain-application covered by unit tests.
- Hardware: the Mac's volume slider changes playback volume in real time.
