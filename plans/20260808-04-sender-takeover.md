# Sender takeover: last stream wins

> **Done (2026-08-09).** Implemented in #120 (the library's active-session
> slot) and #122 (the receiver's process-lifetime ALSA device), and
> **verified on hardware** — a real sender takeover works, and the
> `EBUSY` → decode-only failure it was also aimed at is gone (#110 closed;
> what remains of it is the first open at process start, #123).

Playing from a second sender while a first is streaming should **interrupt
the first and take over**. Today it doesn't: play from an iPhone, then play
from a Mac, and the Mac does not take over.

## Background: what happens today

The server accepts up to 32 concurrent control connections and gives each
its own independent `Session`
([server.rs](../openairplay2/src/server.rs)); nothing arbitrates between
them. A second sender pairs, completes SETUP, and calls the sink factory —
so two streams run at once. What comes out depends on the audio device:

- On a shared device (`default` through PipeWire, `dmix`) both senders play,
  **mixed**.
- On an exclusive device (`plughw:…`) the second stream's sink gets
  `EBUSY` and falls into silent decode-only — one shape of the incident in
  [#110](https://github.com/st3fan/openairplay2/issues/110).

Neither is a behavior anyone chose.

## Research: what the right behavior is

**AirPlay 2 is last-stream-wins; there is no "busy" refusal.** Evidence:

- **shairport-sync** (`rtsp.c`) keeps a single `principal_conn` — the one
  connection allowed to play — guarded by a play lock. At the AirPlay 2
  **initial SETUP** (phase 1, before any stream exists) it acquires the lock
  with interruption **hardcoded on**, with the comment *"airplay 2 always
  allows interruption, so should never return play_lock_aquisition_failed"*.
  The previous principal connection is then **terminated** (its thread
  cancelled, its TCP connection closed); the interrupted sender sees the
  drop and shows playback stopped. Only *Classic* AirPlay (AirPlay 1)
  consults a config option (`allow_session_interruption`, default off →
  refuse with busy); AirPlay 2 deliberately has no such option.
- **Real Apple receivers behave this way — verified on hardware
  (2026-08-08).** Stefan tested against a HomePod: iPhone playing to it,
  then playing something different from the Mac — **the Mac takes over, the
  iPhone's audio stops, the iPhone's player sets itself to *paused*, and
  the iPhone disconnects from the AirPlay receiver entirely**: its output
  route reverts to the phone itself, and pressing play afterwards plays on
  the iPhone's own speaker, not the HomePod. The interrupted sender ends in
  a clean paused-and-disconnected state, not an error; losing the receiver
  is a normal event a sender is built for. That end state is the reference
  this plan implements: our takeover closes the old connection, and the
  sender's own reaction to that is what produces the pause and the route
  change — it does not try to reclaim the receiver.

Also observed in shairport-sync, and deliberately **not** part of this plan:
while a session is active it mirrors the sender's `groupUUID` as its own
`gid` TXT record and sets status-flag bit 11, re-announcing over mDNS — the
"this receiver is in use by X" polish senders can show. Filed separately as
a follow-up issue.

## Scope

Two halves that interlock: **who is playing** (library) and **what they play
through** (receiver binary).

**Library — the play lock and the takeover path:**

- A connection acquires the **active-session slot** at **SETUP phase 1**
  (`setup_timing`, [session.rs](../openairplay2/src/session.rs)) — the same
  point shairport-sync uses. Connections that only probe (`GET /info`,
  `pair-setup`) never touch it, so browsing senders don't disturb playback.
- If another connection holds the slot, it is told to shut down, and the
  new SETUP **waits (bounded) for its teardown to complete** — old sink
  dropped — before proceeding. The device is handed over in order, never
  contended for, which also removes the receiver-races-itself shape of
  #110 (a sender reconnecting quickly), though not its external-contention
  shape.
- The interrupted connection closes; that is what tells the old sender it
  lost the speaker (same mechanism as shairport-sync). Its session emits
  `SessionEnded` before the new one's `SessionStarted`, so hosts and the
  now-playing display follow the handover.
- **No configuration option.** AirPlay 2 semantics are fixed; an
  "allow interruption" knob is Classic-AirPlay-only in shairport-sync and
  is declined here (recorded so it isn't re-proposed).

**Receiver binary — one ALSA device for the life of the process:**

Ordered handover fixes the receiver racing *itself*, but not the failure
seen in the wild:

```
WARN openairplay2_receiver::player] player: cannot open ALSA
     (snd_pcm_open: Device or resource busy); decode-only
```

Any moment the receiver closes the device — session end, or the gap inside
a takeover — an external contender (PipeWire with queued desktop streams,
on a box where the card is also the desktop sink) can seize it, and the
next stream start is `EBUSY` → silent decode-only (#110). So the receiver
stops closing it: **`AlsaOutput` becomes global — opened once, kept for the
whole process, shared by every stream including taken-over ones.** The
track-tick fix already built exactly the right object: the PCM stream is
configured never to stop (`stop_threshold = boundary`, silence-fill,
`snd_pcm_rewind` for flush), so between streams the card simply plays
silence. The per-stream `AudioSink` the sink factory hands the library
becomes a cheap handle onto the one persistent output — flush rewinds it,
the gain and fade-in work as today — and dropping a sink no longer closes
the device. Purely host-side; the library's sink seam is untouched.

Consequences worth naming:

- The takeover gap disappears as a *device* event entirely: nothing closes,
  nothing reopens, nobody else can get in between. This closes #110's
  external-contention half for a running receiver — only the very first
  open can fail, which the startup probe already surfaces.
- A card that is held open playing silence never enters standby — the
  roadmap's "DAC standby prevention" item falls out as a side effect
  (to be confirmed on hardware and, if borne out, retired from the
  roadmap).
- Today every stream is 44.1 kHz stereo (`aac_params()` hard-codes it), so
  one persistent configuration fits all streams; if format negotiation
  ever lands, the output reopens on a rate change — noted here so that
  future work knows this assumption lives in the binary.
- `--no-audio` and a failed startup open behave as today (`NullSink`,
  decode-only); the persistent output is lazily opened at first use if the
  startup probe warned rather than failed.

### Out of scope

- **Busy/gid TXT advertising while active** (status bit 11 + sender
  `groupUUID` mirroring, mDNS re-announce) — follow-up issue.
- **Retry-on-EBUSY at the very first open** — the persistent output narrows
  #110 to exactly one moment: process start on a box where something else
  briefly holds the card. The startup probe already warns there; a retry
  loop for it stays out of scope (and #110 gets updated to say so).
- **Multi-room, relaying, or any second *simultaneous* stream.** The design
  stays one sender → one stream → one output; this plan is about *which*
  sender that is.

## Design

**Library side:**

- `Context` gains the slot: conceptually
  `active: Mutex<Option<ActiveSession>>`, where `ActiveSession` carries a
  shutdown signal for the owning connection and a completion handle that
  resolves when that connection's teardown — including the drop of its sink
  — has finished.
- **Cooperative cancellation, not task abort.** The per-connection request
  loop (`handle_connection`) selects on its shutdown signal; on takeover it
  exits through the normal disconnect path, so the existing teardown code
  runs: session drop stops the library player, the sink is dropped,
  `SessionEnded` is emitted, the connection is logged as disconnected. (An
  aborted task would skip whatever the exit path does explicitly — too easy
  to break events or logging.)
- `setup_timing` acquires the slot before doing anything else: free → take
  it; held by this connection → keep it (a sender may re-SETUP on the same
  connection); held by another → signal it, await its completion handle
  with a timeout (order of ~2 s — generous for a queue flush and a device
  close, short enough that a wedged old session cannot hold the new sender
  hostage), then take the slot. On timeout, take the slot anyway and log a
  warning: the new sender wins by specification, and the wedged session's
  eventual death releases nothing we still depend on.
- The slot is released on connection teardown (whoever holds it), so a
  sender that stops normally leaves the receiver free without any takeover.

**Receiver-binary side:**

- `AlsaOutput` moves out of `AlsaSink` into a process-lifetime holder
  (opened at startup when the probe succeeds, else lazily at first use).
  `AlsaSink` keeps its whole per-stream role — gain, fade-in, scratch
  buffer — but borrows the shared output for writes and rewinds it on
  flush; its `Drop` rewinds instead of closing, so a session ending leaves
  the card open, playing silence.
- The ordering guarantee still matters and still holds: the library's
  takeover path drops the old sink (ending its player thread's writes)
  before the new sink exists, so two streams never interleave writes into
  the shared output.

## Test strategy

- **Integration (`openairplay2/tests/`, real sockets, synthetic senders):**
  - *Takeover:* sender A pairs, SETUPs both phases, streams; sender B pairs
    and issues SETUP phase 1. A's connection must close (its next read is
    EOF), B completes phase 2 and streams. A recording sink factory asserts
    A's sink was dropped **before** B's was created; the event stream shows
    `SessionEnded` (A) before `SessionStarted` (B).
  - *Probes don't interrupt:* while A streams, B connects, fetches
    `GET /info`, completes pair-setup — and A's stream is undisturbed.
- **Unit:** the slot's three acquire cases (free / held-by-self /
  held-by-other) and the timeout path.
- **Receiver binary:** the shared-output handover against the ALSA `null`
  device (open once → stream A writes → A drops → stream B writes, same
  output, no reopen — extending the existing FFI round-trip test); a
  sink drop leaves the output open.
- **Hardware acceptance (the milestone convention):** the HomePod
  reference is already recorded above; against this receiver the same
  sequence must match it — iPhone streams, Mac starts, the Mac plays
  within a couple of seconds and the iPhone's player shows paused; then
  the reverse direction; the now-playing display follows the handover;
  and a same-device quick stop/restart still reconnects cleanly. All of it
  on the exclusive `plughw:` device on the desktop where the `EBUSY` →
  decode-only failure was observed, with the desktop session active: the
  warn line must not appear across takeovers and session ends, and desktop
  audio must not capture the card between streams.

## Acceptance criteria

- A second sender's SETUP always wins: the first stream stops, its
  connection closes, and the new stream plays — on hardware, both
  directions, matching the HomePod-observed behavior.
- Probing connections never interrupt playback.
- Sink lifetimes never overlap (asserted in the integration test), and the
  receiver binary opens its ALSA device once for the whole process — no
  close/reopen on session end or takeover, so nothing external can capture
  the card and the `EBUSY` → decode-only failure cannot recur while the
  receiver runs.
- No public API change beyond behavior; no new configuration; macOS library
  tests stay green.

## Phases

1. **This plan** (bottom of the stack), including the HomePod observation.
2. **Library: the takeover** — the slot, cooperative shutdown, ordered
   handover, integration tests.
3. **Receiver: the persistent output** — the process-lifetime `AlsaOutput`,
   sinks as handles onto it, tests; update #110 to its narrowed scope, and
   note the DAC-standby side effect on the roadmap item once confirmed on
   hardware.

## Outcome

Met. Both phases landed and behave as intended on hardware.

Two things the work turned up that the plan did not anticipate:

- **The `EBUSY` failure reproduced itself during development**, which is how
  we know the persistent device matters: pointing the new build at the
  desktop's own card found PipeWire already holding it *at startup*, before
  any sender existed. The startup hold is therefore best-effort — it warns
  and retries at the first stream — leaving exactly one moment unprotected.
  That, and whether an operator should be able to turn the hold off, is
  #123.
- **The library needed tokio's `macros` feature declared.** `tokio::select!`
  in the connection loop compiled locally through workspace feature
  unification but could not package standalone; CI's
  `cargo package --workspace` gate (from #74) caught it before release, which
  is precisely the job that gate was added for.

The DAC-standby side effect is recorded on the roadmap item rather than
retiring it: a card held open playing silence is the mechanism that item
asks for, but nothing here was tested against a DAC that actually sleeps.
