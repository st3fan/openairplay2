# Gate session commands on the encrypted channel

Fixes [#141](https://github.com/st3fan/openairplay2/issues/141) — every session
method (`SETUP`, `SET_PARAMETER`, `GET_PARAMETER`, `SETRATEANCHORTIME`,
`FLUSHBUFFERED`, `TEARDOWN`, the `RECORD`/`SETPEERS` acks) and the active-slot
takeover are dispatched on a plaintext, never-paired connection.

## Background

[server.rs](../openairplay2/src/server.rs) consults `conn.is_encrypted()` only
for the handshake timeout (`next_request`, `server.rs:103`) and logging
(`server.rs:165`) — never as an authorization check. After the two plaintext
special cases (`/pair-setup`, `/pair-pin-start`), the loop runs the `SETUP`
slot-claim (`server.rs:211-215`) and then `dispatch_session`
(`server.rs:251-293`) unconditionally. So an unpaired LAN peer can:

- send `SETUP` → `context.active.claim(...)` evicts whoever holds the slot
  (`takeover.rs:91`, unconditional) — the legitimate sender's connection is
  closed. A bare plaintext `SETUP` with a garbage body still evicts, because the
  claim happens *before* `handle_setup` runs and can fail. Permanent playback
  denial of service, refreshed with one request per `HANDSHAKE_TIMEOUT` (10 s);
- send a phase-2 `SETUP` carrying an attacker-chosen 32-byte `shk` — the audio
  channel keys off `shk`, not the pairing secret, so the pipeline starts and
  plays attacker audio pushed to the data port;
- set volume, pause, flush, and teardown at will.

### What this does and does not fix

`is_encrypted()` becomes true only when `pair-setup` reaches `Outcome::Done`
(`server.rs:184`), i.e. SRP completed. With a `--password` configured, a wrong
password fails at M3→M4 (`pairing.rs:93`) and encryption is never enabled — so
`is_encrypted()` is an exact proxy for "this peer completed pairing (and, if a
password is set, knew it)." Gating session commands on it therefore makes the
`--password` feature actually enforce access, which today it does not: the
password protects only the SRP exchange, not any subsequent command, so the
password and the open default are equivalent in practice.

For the **open default** (no password), pairing uses the fixed transient code
`3939` ([#156](https://github.com/st3fan/openairplay2/issues/156)), so a peer
that simply completes pairing still gets in. This gate does not — and cannot —
change that; closing the open default needs device authentication
(`pair-verify` / persistent pairing plus an access-control policy), which is a
separate, larger effort. What the gate buys for the open default is narrower but
real: a peer can no longer skip pairing entirely and inject audio or DoS the
slot with a plaintext `SETUP`.

For reference, shairport-sync gates nothing here either — for AirPlay 2 it sets
`conn->authorized = 1` for any connection (`rtsp.c:4218`) and dispatches every
method without an encryption check — so this is not a regression against the
reference implementation; it is a hardening that makes our password option
meaningful.

## Scope

One gate in the control loop, where "may this peer issue commands" belongs.

**In scope**

1. **[server.rs](../openairplay2/src/server.rs) `handle_connection`** — after the
   `/pair-setup` and `/pair-pin-start` special cases (which must stay reachable
   in the clear) and *before* the `SETUP` slot-claim, refuse any request on a
   not-yet-encrypted connection except discovery:

   - allowed in the clear: `GET /info` (and `/pair-*`, handled above);
   - everything else on an unencrypted connection → `403 Forbidden`, empty body,
     with the usual `Content-Length`/`CSeq`/protocol token via `finalize`, then
     `continue` (the connection stays; the handshake timeout still governs an
     unpaired peer that goes quiet).

   Because the gate `continue`s before the slot-claim, `active.claim` and
   `dispatch_session` are reached only on an encrypted connection — no unpaired
   peer can evict the holder, start a stream, or touch session state.

**Out of scope**

- **Closing the open (no-password) default.** That requires `pair-verify` /
  persistent pairing and an access-control policy; tracked separately. This plan
  does not add device authentication.
- The other findings from the same review — the fixed PIN
  ([#156](https://github.com/st3fan/openairplay2/issues/156)), data/event peer
  binding ([#146](https://github.com/st3fan/openairplay2/issues/146)), repeated
  `SETUP` accumulation ([#145](https://github.com/st3fan/openairplay2/issues/145)),
  and the rest ([#142](https://github.com/st3fan/openairplay2/issues/142)–158)
  — each has its own issue.
- Which methods a *paired* connection may call, and in what order. The gate is
  binary: encrypted or not.

### Why `/fp-setup` and the keep-alives are gated, not allowed

A real macOS sender does `/fp-setup` and the `/feedback`·`/command`·`/audioMode`
keep-alives *after* pairing, over the encrypted channel — the pairing
integration test drives `/fp-setup` in exactly that position with a body
captured from real hardware
([pairing.rs](../openairplay2/tests/pairing.rs) `transient_pairing_then_encrypted_info`).
So gating them behind encryption matches observed behavior. The hardware check
below is the backstop: if a real Mac is seen sending any of these in the clear,
the fix is to widen the allowlist by that one method.

## Test strategy

Integration tests in [pairing.rs](../openairplay2/tests/pairing.rs), which
already has the plaintext and encrypted-request helpers:

- **Plaintext session verbs are refused.** On a fresh (unpaired) connection,
  each of `SETUP`, `SET_PARAMETER`, `GET_PARAMETER`, `SETRATEANCHORTIME`,
  `FLUSHBUFFERED`, `TEARDOWN` over plaintext returns `403 Forbidden`.
- **Discovery still works in the clear.** Plaintext `GET /info` returns `200`.
- **The takeover DoS is closed.** Pair sender A and drive its `SETUP` phase 1
  (A claims the slot) over the encrypted channel; from a second, *unpaired*
  connection send a plaintext `SETUP` → `403`; then assert sender A is still live
  by issuing another encrypted request on A and getting `200` (an evicted
  connection would have been closed). This proves an unpaired peer cannot claim
  or steal the slot.
- **Regression: the legitimate flow is untouched.** The existing
  `transient_pairing_then_encrypted_info` still completes `fp-setup` + both
  `SETUP` phases over the encrypted channel, confirming the gate does not refuse
  a paired sender.

These run in the macOS-portable library crate, so CI covers them on both
platforms.

**Hardware check (acceptance):** a real Mac/iPhone still discovers, pairs, and
plays end to end — the gate must never refuse a legitimately paired sender's
`SETUP`. This is the one behavior a unit test cannot fully stand in for, per the
milestone convention.

## Acceptance criteria

- An unpaired connection receives `403` for every method except `GET /info` and
  `/pair-*`; no slot is claimed, no session state changes, no event is emitted.
- With `--password` set, a peer that never completes SRP cannot `SETUP`, inject
  audio, set volume, pause, flush, or teardown.
- A legitimately paired sender still completes `SETUP` and plays (verified on
  hardware).
- `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check` clean;
  `cargo test -p openairplay2` passes on macOS.

## Phases

One phase — the change is a single gate plus tests:

1. **`pairing-gate`** — the `is_encrypted()` gate in `handle_connection` and the
   integration tests above.
