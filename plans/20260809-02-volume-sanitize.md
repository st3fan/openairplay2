# Sanitize the `volume:` parameter

Fixes [#153](https://github.com/st3fan/openairplay2/issues/153) — a `SET_PARAMETER`
line of `volume: nan` or `volume: inf` is accepted verbatim and reaches the gain
path, where it becomes **full scale**.

## Background

[session.rs](../openairplay2/src/session.rs) `set_text_parameters` parses the
sender's volume line with `parse::<f32>()` and passes the result straight into
two places: `self.volume` (echoed back on `GET_PARAMETER volume`) and
`Event::Volume { db }` (the host's gain path). Rust's float parser accepts
`nan`, `inf`, `-inf` and overflowing literals like `1e40` (→ `inf`), and none of
the code downstream expects them.

This is the only float parsed off the wire anywhere in the workspace —
`progress:` is three `u32`s and everything else is integers — so the blast
radius is one function.

### What each value does today (measured, not assumed)

| `volume:` | software gain (`volume_to_gain`) | hardware mixer (`Mixer::set`) | `GET_PARAMETER` echo |
|---|---|---|---|
| `nan` | **1.0 — full scale** | control minimum | `volume: NaN` |
| `inf` | **1.0 — full scale** | **control maximum (0 dB)** | `volume: inf` |
| `-inf` | 0.0 (mute) | minimum | `volume: -inf` |
| `1e30` | 1.0 | maximum | `volume: 1000000015047466219876688855040.000000` |

`NaN` survives the software path because `db.min(0.0)` returns the *other*
operand when one side is NaN (so `10^0 = 1.0`), and takes the opposite route
through the mixer because `f32::clamp` propagates NaN and `NaN as i64`
saturates to `0`.

### Why it matters

- **An unexpected jump to full volume.** A sender sitting at −30 dB that sends
  `nan` or `inf` lands at unity gain. That is the whole hazard — there is no
  memory unsafety and no panic (`SharedGain::set` clamps, and `f32::clamp` only
  panics on NaN *bounds*, not a NaN value).
- **The two output paths disagree** (full scale vs. mute for `nan`), which is a
  correctness bug independent of the security framing.
- **The echo is malformed.** We answer `GET_PARAMETER volume` with
  `volume: NaN`. A bad answer to that query is exactly what makes a real sender
  abort before `SETUP` phase 2 (see CLAUDE.md) — so one bad line can poison the
  rest of the session.

Not affected, and deliberately not "fixed": the now-playing display. serde_json
writes non-finite floats as `null`, [client.rs](../openairplay2-tui/src/client.rs)
logs `ignoring unrecognized message` and skips it, and `Snapshot.volume_db:
Some(NaN)` serializes as `null` — indistinguishable from `None` and decoded as
such. No poisoned snapshot, no dropped connection.

Reachability is the usual caveat: this rides the post-pairing channel, but with
the fixed `3939` code ([#156](https://github.com/st3fan/openairplay2/issues/156))
and no auth gate on session commands
([#141](https://github.com/st3fan/openairplay2/issues/141)), anyone on the LAN
gets there.

## Scope

Sanitize at the library boundary — where session semantics live — so every
embedder gets the guarantee, and keep a cheap guard in the host so the volume
path fails *quiet* rather than *loud*.

**In scope**

1. **[session.rs](../openairplay2/src/session.rs) `set_text_parameters`** —
   after parsing:
   - a non-finite value is **dropped** with a `debug!` and the previous volume
     is kept (garbage must not move the knob at all);
   - a finite value is **clamped to `[-144.0, 0.0]`** — `-144` is already the
     mute sentinel and `0` is full scale, so this only rewrites values that were
     nonsense to begin with.

   Both `self.volume` and the emitted event get the sanitized number, which
   fixes the malformed `GET_PARAMETER` echo for free.
2. **[events.rs](../openairplay2/src/events.rs)** — state the invariant on
   `Event::Volume`: `db` is always finite and always within `[-144.0, 0.0]`.
   The doc comment already implies it; the code has not been holding it.
3. **Defense in depth in the host** (two lines, unreachable once (1) lands, but
   `volume_to_gain` is `pub` and mute is the right failure direction):
   - [openairplay2-receiver/src/player.rs](../openairplay2-receiver/src/player.rs)
     `volume_to_gain` returns `0.0` for a non-finite `db`;
   - [openairplay2-receiver/src/mixer.rs](../openairplay2-receiver/src/mixer.rs)
     `Mixer::set` treats a non-finite `db` as mute.

**Out of scope**

- The other findings from the same review — the auth gate (#141), the fixed PIN
  (#156), channel bounds (#155, #157) — each has its own issue.
- Honoring anything else in `text/parameters`; only `volume:` and `progress:`
  are parsed and `progress:` is integers.
- Changing the dB→gain curve or the mixer's slider mapping. This plan only
  rejects values that were never valid.

## Test strategy

- **[session.rs](../openairplay2/src/session.rs) unit tests** (the important
  ones): `nan`, `inf`, `-inf`, `1e40`, `-500`, `+6` through `set_parameter`,
  asserting both the event that is (or is not) emitted and that
  `get_parameter(b"volume\r\n")` still answers a well-formed float. These run in
  the macOS-portable library, so CI covers them on both platforms.
- **`volume_to_gain(f32::NAN) == 0.0`** alongside the existing
  `volume_db_to_gain` test.
- **`slider_to_range` with a non-finite `db`** in the mixer's pure-function
  tests.

No hardware check: nothing here changes the wire protocol or timing, and a real
sender never sends these values. (Stated explicitly because a hardware check is
normally part of a milestone's acceptance criteria.)

## Acceptance criteria

- `volume: nan` / `inf` / `-inf` / `1e40` leave the current volume untouched and
  emit no `Event::Volume`; a debug log says why.
- A finite out-of-range volume is clamped into `[-144.0, 0.0]` and the clamped
  value is what both the event and the `GET_PARAMETER` echo carry.
- No input from the wire can move either output path to full scale except a
  volume that legitimately asks for it.
- `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check` clean;
  `cargo test -p openairplay2` passes on macOS.

## Phases

One phase — the change is a handful of lines plus tests:

1. **`volume-sanitize`** — library sanitizer + documented invariant + host
   guards + tests.
