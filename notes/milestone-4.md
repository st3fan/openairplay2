# Milestone 4 — SETUP & receiving the stream

Goal: complete the AirPlay 2 `SETUP` negotiation with a real sender so it
proceeds to `RECORD` and starts sending the audio stream, and receive that
stream (log/drain — no decode/playback yet). This captures the real audio
format the sender picks, which is milestone 5's input.

The milestone-3 hardware capture confirmed the flow up to here:
`GET /info → pair-setup → fp-setup ×2 → SETUP (phase 1)` — the sender's
phase‑1 plist wanted **`timingProtocol = PTP`** and carried its
`timingPeerInfo`.

## Scope

In:

- **SETUP phase 1** (no `streams` array): parse the plist; bind a TCP **event**
  socket; reply with a plist `{ eventPort, timingPort: 0, timingPeerInfo:
  { Addresses: [<our ip>], ID: <our ip> } }`. Accept + drain the event channel
  (receiver→sender notifications; we don't need to emit any for basic
  playback).
- **SETUP phase 2** (`streams` array): for stream 0, read `type`
  (96 realtime / 103 buffered), `audioFormat`/`spf`, and the stream key
  (`shk`). Bind **data** (RTP audio) and **control** (RTCP) UDP sockets; reply
  `{ streams: [{ type, dataPort, controlPort, [audioBufferSize] }] }`. Stash
  the format + key for milestone 5. Spawn a receiver that logs the arriving
  audio/control packets.
- **Session methods**: acknowledge `RECORD`, `SETRATEANCHORTIME`, `SETPEERS`,
  `SETPEERSX`, `FLUSHBUFFERED`, `SET_PARAMETER`, `GET_PARAMETER`, `TEARDOWN`
  (200, with the small plist bodies where the reference returns one) so the
  session proceeds and stays alive.
- **Per-connection `Session`** holding the bound sockets/ports, stream format,
  and key — mirrors the v1 receiver's session object.

Out: audio decrypt/decode/playback (milestone 5) and PTP timing/sync
(milestone 6). We bind and drain the channels; we don't yet turn packets into
sound.

## Wire details (from shairport-sync `rtsp.c`)

- Phase 1 reply keys: `eventPort` (our TCP event port), `timingPort` `0` for
  PTP, `timingPeerInfo` echoing our address. `Content-Type:
  application/x-apple-binary-plist`.
- Phase 2 per-stream reply: `type` (echo 96/103), `dataPort`, `controlPort`,
  and for buffered (`103`) also `audioBufferSize`.
- Stream `type`: 96 = realtime audio, 103 = buffered audio.
- Audio is ChaCha20-Poly1305 per packet using a key derived from `shk` (kept
  for milestone 5).

## Module layout

```
src/plist_ext.rs — small helpers to read/build the SETUP plists (new)
src/session.rs   — per-connection AirPlay session: SETUP phases, bound
                   sockets, receiver tasks, control-method handling (new)
src/server.rs    — dispatch SETUP/RECORD/... into the Session
```

## Test strategy

- **Unit**: SETUP phase-1 plist (built from the real captured request) →
  response contains `eventPort`, `timingPort=0`, `timingPeerInfo`; phase
  detection (streams present/absent); a synthetic phase-2 streams plist →
  response echoes `type` and carries `dataPort`/`controlPort`.
- **Integration**: over the encrypted channel after pairing+fp-setup, send a
  SETUP phase‑1 plist and assert the decrypted response is a plist with an
  `eventPort`; then a phase‑2 streams plist and assert a `streams` response
  with ports.
- **Manual (you-run-it)**: a real sender completes SETUP (both phases) and
  proceeds to RECORD; the debug log shows the phase‑2 `streams` plist (the
  real format) and audio packets arriving on the data port.

## Acceptance criteria

- `cargo test` / `cargo clippy` clean.
- SETUP unit + integration tests pass.
- Hardware: the sender gets past SETUP (both phases) and reaches RECORD /
  starts streaming, rather than disconnecting at SETUP — and we capture the
  real `streams` format for milestone 5.

## Result

Done. 39 tests pass, clippy clean.

- **Unit** (`session.rs`): a phase‑1 plist (`timingProtocol=PTP`, no streams) →
  response with a bound `eventPort`, `timingPort=0`, and `timingPeerInfo`; a
  phase‑2 streams plist → response echoing `type` with bound
  `dataPort`/`controlPort` (and `audioBufferSize` for buffered), and the `shk`
  key stashed.
- **Integration** (`tests/pairing.rs`): the full real flow over the encrypted
  channel — pairing → fp‑setup → **SETUP phase 1** (decrypted response has an
  `eventPort`) → **SETUP phase 2** (decrypted response has `streams` with
  data/control ports).

Design notes:
- A per‑connection `Session` binds a TCP event socket + two UDP audio/control
  sockets on the address the control connection arrived on, and reports those
  ports back; its receiver tasks currently log/drain packets (decode is
  milestone 5). Sockets are torn down when the `Session` drops.
- `RECORD`, `SETRATEANCHORTIME`, `SETPEERS(X)`, `FLUSHBUFFERED`,
  `SET_PARAMETER`, `GET_PARAMETER`, `TEARDOWN` are acknowledged `200` so the
  sender proceeds.
- The stream key (`shk`), `audioFormat`, and `type` are captured for
  milestone 5's audio decrypt/decode.

Not verified here (needs the you‑run‑it hardware test): that a real sender
accepts our SETUP responses and proceeds to RECORD + streams. The captured
phase‑2 `streams` plist (the real codec/format) is milestone 5's input.
