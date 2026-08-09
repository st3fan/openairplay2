# Sender takeover: last stream wins

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

Library-only. One new piece of shared state — the play lock — and the
takeover path:

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

### Out of scope

- **Busy/gid TXT advertising while active** (status bit 11 + sender
  `groupUUID` mirroring, mDNS re-announce) — follow-up issue.
- **Retry-on-EBUSY for external device contention** — that is #110's
  remaining half (PipeWire and friends), unrelated to sender arbitration.
- **Multi-room, relaying, or any second *simultaneous* stream.** The design
  stays one sender → one stream → one output; this plan is about *which*
  sender that is.

## Design

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
- **Hardware acceptance (the milestone convention):** the HomePod
  reference is already recorded above; against this receiver the same
  sequence must match it — iPhone streams, Mac starts, the Mac plays
  within a couple of seconds and the iPhone's player shows paused; then
  the reverse direction; the now-playing display follows the handover;
  and a same-device quick stop/restart still reconnects cleanly.

## Acceptance criteria

- A second sender's SETUP always wins: the first stream stops, its
  connection closes, and the new stream plays — on hardware, both
  directions, matching the HomePod-observed behavior.
- Probing connections never interrupt playback.
- Sink lifetimes never overlap (asserted in the integration test), so
  exclusive ALSA devices hand over cleanly.
- No public API change beyond behavior; no new configuration; macOS library
  tests stay green.

## Phases

1. **This plan** (bottom of the stack), including the HomePod observation.
2. **Implementation** — the slot, cooperative shutdown, ordered handover,
   tests. One PR; library-only.
