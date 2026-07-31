# Milestone 3 — FairPlay `fp-setup`

Goal: get a real Apple sender *past* the FairPlay gate. A live capture
(milestone 2 hardware test) showed the sequence a stock macOS sender uses:

```
GET /info → POST /pair-setup (transient, M1→M4) → [channel encrypted]
→ POST /fp-setup  ← we 501'd this, and the sender gave up before SETUP
```

So `fp-setup` is the blocker. Once we answer it, the sender proceeds to
`SETUP` (the audio negotiation), which milestone 4 will handle — this
milestone's secondary purpose is to let that real `SETUP` plist be captured.

## What `fp-setup` is

FairPlay's `fp-setup` here is a **canned handshake**, not live crypto — the
receiver replies with fixed tables (this is the widely-used interop behaviour
in shairport-sync and the openairplay Python receiver; the tables originate
from those GPL projects, same interop footing as v1's embedded AirPort key).
Requests are `application/octet-stream` beginning with `FPLY` (`46 50 4c 59`):

- byte 4 = version (3), byte 5 = type (1), byte 6 = **sequence**, byte 14 =
  **mode** (phase 1 only).
- **Phase 1** (seq == 1, 16-byte request): reply with one of four fixed
  **142-byte** tables selected by the mode byte (0–3).
- **Phase 2** (seq == 3, 164-byte request): reply is a fixed **12-byte**
  header `FPLY 03 01 04 00 00000000 14` followed by the **last 20 bytes** of
  the request → 32 bytes total.

The `fp-setup` request arrives on the **encrypted** channel (post-pairing);
`ControlConnection` already encrypts the response.

## Scope

In:

- **`fairplay.rs`**: parse the `FPLY` request and produce the phase-1 / phase-2
  reply. Pure and fully unit-testable.
- **Server wiring**: handle `POST /fp-setup`; also answer the keep-alive
  methods a sender interleaves (`POST /feedback`, `POST /command`) with `200`
  so the session stays alive and reaches `SETUP`.
- Everything still unhandled (`SETUP`, `RECORD`, `SETRATEANCHORTIME`,
  `FLUSHBUFFERED`, `TEARDOWN`, …) is logged (with the body hex dump added in
  the M2 bring-up) and `501`'d — so the real `SETUP` negotiation is captured
  for milestone 4.

Out: `SETUP` / channels / audio / timing — milestone 4+.

## Test strategy

- **Unit**: phase-1 request with each mode byte → the correct 142-byte table;
  phase-2 request → 12-byte header + echoed last-20-bytes (32 bytes);
  malformed / non-`FPLY` input rejected.
- **Integration**: extend the pairing test — after transient pairing, send an
  **encrypted** `POST /fp-setup` (phase 1 then phase 2) and assert the
  decrypted replies match the expected tables/echo.
- **Manual (hardware, you-run-it)**: a sender completes pairing → `fp-setup`
  (both phases) and then sends `SETUP`; the debug log captures the real
  `SETUP` plist(s) — the input for milestone 4.

## Acceptance criteria

- `cargo test` / `cargo clippy` clean.
- fp-setup unit + integration tests pass.
- Hardware: the sender gets past FairPlay and issues `SETUP` (visible in the
  log), rather than disconnecting at `fp-setup`.

## Result

Done. 36 tests pass (32 unit + 4 integration), clippy clean.

- **Unit** (`fairplay.rs`): each mode byte (0–3) → the correct 142-byte table;
  the **exact phase-1 request the real macOS sender sent** (from the M2
  capture) → REPLY1; phase 2 → 12-byte header + echoed last-20-bytes; garbage
  rejected.
- **Integration** (`tests/pairing.rs`): after transient pairing, an
  **encrypted** `POST /fp-setup` phase 1 then phase 2 return encrypted 142- and
  32-byte replies that decrypt correctly (phase 2 echoing the request suffix).

Also added: `POST /feedback`, `/command`, `/audioMode` are acknowledged with
`200` so a real session stays alive to reach `SETUP`; every still-unhandled
method (SETUP, RECORD, …) is logged with its body hex and `501`'d.

Not verified here (needs the you-run-it hardware test): that a real sender
gets past FairPlay and sends `SETUP`. That capture is milestone 4's input.

### Provenance note

The four 142-byte tables and the 12-byte header are the well-known FairPlay
interop constants shipped by shairport-sync and the openairplay Python
receiver, copied verbatim for interoperability (receiving audio streamed to
your own device) — the same footing as the embedded AirPort RSA key in the
AirPlay 1 receiver.
