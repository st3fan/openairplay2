# Hardware mixer volume

Drive the sound card's own mixer control instead of multiplying PCM samples by
a gain. This is the "Hardware mixer volume" item from
[notes/roadmap.md](../notes/roadmap.md) ("0.6 and later → wanted, not yet
scheduled"), promoted to a plan.

## Background

Today the volume path is entirely software: the AirPlay volume arrives as an
`Event::Volume(db)` (the sender's slider mapped to `[-30, 0]` dB, `-144` for
mute), [main.rs](../openairplay2-receiver/src/main.rs) converts it with
`volume_to_gain` and stores it in `SharedGain`, and the sink scales every
sample by that gain just before `writei`
([openairplay2-receiver/src/player.rs](../openairplay2-receiver/src/player.rs)
`apply_gain`).

Two things are wrong with that on real hardware:

- **Software gain throws away bits.** Scaling S16 samples by 0.1 leaves ~13
  bits of signal; the DAC then amplifies its own noise floor along with the
  quieter signal. A card that attenuates in its analog stage (or in a
  higher-resolution digital stage) keeps the full 16 bits all the way down.
- **A real amp or DAC expects to be told its own volume.** A USB DAC or an amp
  HAT with a volume control should have *that* control follow the iPhone's
  slider — not receive full-scale samples that were pre-attenuated upstream.

The seam for this already exists and is one of the library's invariants: the
library reports dB and never applies volume; the host decides what to do with
it. So this change is **entirely in `openairplay2-receiver`** — no library
changes, no new library API.

## Scope

When a mixer control is configured, `Event::Volume` drives that ALSA mixer
control; the software gain stays at full scale (untouched samples). When no
mixer is configured, behavior is exactly today's: software gain. Concretely:

- New options: `--mixer CONTROL` (with env `OPENAIRPLAY2_MIXER`) and
  `--mixer-device DEV` (env `OPENAIRPLAY2_MIXER_DEVICE`).
- `--list-mixers` prints the playback mixer controls of the mixer device (or
  of each card, when no device is given), with their dB ranges — the
  `--list-devices` counterpart for picking a `CONTROL` value.
- Volume mapping from the AirPlay slider range onto the control's dB range,
  mute via the control's playback switch when it has one.
- Documentation: README options table, the `/etc/default` options-file
  template in `packaging/`, and the man-page-ish `--help` text (the HELP
  unit test enforces the latter).

### Out of scope

- **The library.** No changes; the invariant (library reports dB, host
  applies) is the reason this plan is receiver-only.
- **Reading the mixer back / external volume changes.** If someone turns the
  knob with `alsamixer` mid-session, we don't notice and don't report it back
  to the sender (AirPlay has no unsolicited volume-report path in our
  transient-pairing session anyway). The control is write-only for us.
- **A "both" mode** (hardware for the coarse range, software for the
  remainder below the control's floor — what shairport-sync does when the
  mixer range is smaller than the requested range). If a control's range
  turns out too small in practice, that's a follow-up issue.
- **DAC standby prevention** — the neighbouring roadmap item; separate plan.

## Design

### CLI and resolution

- `--mixer CONTROL` — the ALSA simple-mixer control name, e.g. `PCM`,
  `Master`, `Digital`, `Speaker`. An optional `,INDEX` suffix
  (`--mixer "Speaker,1"`) selects a non-zero selem index; index 0 otherwise.
  Absent → software volume, exactly as today.
- `--mixer-device DEV` — the device to open the mixer on. Default: derived
  from `--alsa-device` when that names a card (`plughw:CARD=S2` →
  `hw:CARD=S2`, reusing `card_id_of`), else `default`. Given explicitly, used
  verbatim. `--mixer-device` without `--mixer` is a config error.
- Both merge from the environment like every other option
  ([main.rs](../openairplay2-receiver/src/main.rs) `resolve`), so the Debian
  package's options file can set `OPENAIRPLAY2_MIXER=...`.
- **Fail fast at startup**, like `probe_device`: if the mixer device won't
  open or the control doesn't exist on it, exit `EX_CONFIG` with a message
  that points at `--list-mixers` (the systemd unit then stays stopped instead
  of restart-looping). A mixer that opened at startup but errors at set-time
  logs a warning and the volume event is dropped — audio keeps playing.

### Volume mapping

`Event::Volume` carries the sender's slider as dB in `[-30, 0]`, with `-144`
as the mute sentinel (see `volume_to_gain`). The mixer path maps the slider
**linearly in dB** onto the control's own dB range `[min_db, max_db]` (from
`snd_mixer_selem_get_playback_dB_range`):

```
db == -144           → mute
db in [-30, 0]       → min_db + (db + 30) / 30 * (max_db - min_db)
```

That is: slider-top = the control's maximum, slider-bottom = the control's
minimum, and the taper in between is the control's own dB scale — the same
mapping shairport-sync uses. It deliberately does **not** pass the dB value
through unscaled: 30 dB of slider on a control with a 100 dB range would
strand the bottom 70 dB, and the slider's own range is an arbitrary AirPlay
constant, not something the listener chose.

Mute (`-144`):

- If the control has a playback switch (`has_playback_switch`), switch off;
  any non-mute volume switches it back on before setting the level.
- If it has no switch, set the control to its minimum **and** set the
  software gain to 0 as a belt-and-braces (some controls' minimum is −60 dB,
  which is quiet, not silent). This is the only case where the mixer path
  touches `SharedGain`; on unmute it restores gain 1.0.

Controls that report no dB information (rare, but ALSA allows it): map the
slider linearly onto the raw volume range instead
(`set_playback_volume_all`), with a startup log line saying so. Crude, but it
beats refusing to work with the control the user asked for.

### Module layout

- **`openairplay2-receiver/src/mixer.rs`** (new): `HwVolume` — open
  (device + control name + index, via the safe `alsa` crate's
  `alsa::mixer::{Mixer, SelemId}`; no new dependency, and unlike the PCM
  stream nothing here needs `alsa-sys`), `set_db(airplay_db)` implementing
  the mapping/mute above, and the `--list-mixers` listing. The mapping
  itself is a pure function `slider_to_milli_bel(db, min, max) -> MilliBel`
  so it is unit-testable without hardware.
- **[main.rs](../openairplay2-receiver/src/main.rs)**: parse/resolve/validate
  the new options; probe at startup; in the event loop,
  `Event::Volume(db)` becomes `hw.set_db(db)` when a mixer is configured,
  `gain.set(volume_to_gain(db))` otherwise. The `alsa::Mixer` handle is not
  `Send`, so if it can't live in the event task directly, it gets a tiny
  dedicated thread fed volume values over a channel — same ownership pattern
  as the sink. The sink itself is untouched: with the gain left at 1.0,
  `apply_gain` is already a no-op.

### Packaging and docs

- README: the two options + `--list-mixers` in the options table, and a
  short "hardware volume" paragraph (what it does, how to pick a control).
- `packaging/`: commented `OPENAIRPLAY2_MIXER=` / `OPENAIRPLAY2_MIXER_DEVICE=`
  lines in the options-file template.

## Test strategy

- **Pure mapping, unit-tested:** `slider_to_milli_bel` — endpoints map to the
  control's min/max, midpoint lands proportionally, out-of-range input
  clamps, `-144` is recognized as mute (and is *not* mapped), degenerate
  range (min == max) doesn't divide by zero. Raw-range fallback mapping
  likewise.
- **CLI/resolution tests** alongside the existing ones in main.rs: flags
  parse, env vars merge, flag beats variable, `--mixer-device` without
  `--mixer` is rejected, HELP documents the new flags (existing tripwire
  test).
- **No CI hardware dependency:** there is no ALSA `null` mixer, so
  open/set against a real control is *not* CI-tested — the FFI here is the
  safe `alsa` wrapper, not hand-rolled `alsa-sys` calls, which is what made
  CI coverage essential for the PCM path.
- **Hardware acceptance (skynet + a real sender), per the milestone
  convention:**
  - `--list-mixers` shows the card's controls with sane dB ranges.
  - With `--mixer` on the USB card's control: the iPhone slider sweep is
    audible and `amixer`/`alsamixer` show the control tracking it; samples
    reach the card at full scale.
  - Mute from the sender is silent; unmute restores the previous level.
  - Volume set *before* playback starts (slider moved while paired but idle)
    is in effect when audio starts.
  - A bogus `--mixer nonsense` refuses to start with the
    `--list-mixers` hint; without `--mixer`, behavior is unchanged
    (regression check on the software path).

## Acceptance criteria

- `Event::Volume` drives the configured ALSA mixer control over its full dB
  range; PCM samples are not attenuated in software while a mixer is
  configured.
- Mute works on controls with and without a playback switch.
- No `--mixer` → byte-for-byte today's behavior.
- No changes under `openairplay2/`; `cargo test -p openairplay2` on macOS
  unaffected.
- Options documented in README, `--help`, and the packaging options file;
  hardware checklist above passes on skynet.

## Phases

1. **This plan** (bottom of the stack).
2. **Implementation** — `mixer.rs`, CLI/env wiring, event-loop switch, docs
   and packaging, tests. One PR; it's one concern.
