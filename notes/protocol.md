# OpenAirPlay2 — AirPlay 2 pairing & "pincode" protocol notes

These notes record what this project understands about **AirPlay 2 pairing**
while building its optional `--password` ("pincode") protection. They are
research- and source-verified (shairport-sync's `pair_ap` pairing library and
the HomeKit Accessory Protocol *Pairing* sub-spec), and are being pinned
empirically against a real sender in Phase 2 of
`plans/20260805-01-pincode.md` — mirroring how openairplay1's AirPlay 1
password notes were written.

> **Status: Phase 2 verification in progress.** The mechanism is understood
> and written up below; the exact wire bytes of the persistent flows (the
> M5/M6 identity-envelope keys, the `pair-verify` request/response, whether
> iOS prompts a setup code) are **open items to capture**, listed under
> [Open items](#open-items-to-pin-on-hardware). Do not build protocol code
> against a guess — pin it here first.

## The core fact: AirPlay 2 has no AirPlay 1-style password

AirPlay 1 (openairplay1) gates streaming with RFC 2617 Digest auth over RTSP.
AirPlay 2 has **no such mechanism** — its access control is *pairing*. And
pairing exists in two flavours (from shairport-sync's `pair_ap/README.md`):

- **Transient pairing** — "no code from the user"; the client does a
  two-step `/pair-setup` (M1→M4) with a **fixed code `3939`**.
- **Persistent (normal) pairing** — the client needs a **one-time setup code**
  typed by the user; it does the full `/pair-setup` (M1→M6), stores the
  pairing, and **henceforth authenticates with `/pair-verify`**.

A live test settled which of these a user-facing "pincode" maps to: setting
`--password 4321` on the (then transient-only) receiver made an iPhone on
iOS 26 send a `transient=true` M1 and an M3 proof derived from the **fixed
`3939`**; SRP verification failed and iOS gave up with "Unable to connect".
Transient pairing has no place for a user-entered code, so **the pincode is
the setup code of persistent pairing** — implementing that is `plan
20260805-01`.

## Transient pairing (what openairplay2 does today)

Current behavior, and the "no password" default:

```
Client (controller)                          Receiver
  |--- POST /pair-setup M1 (transient=1) ---->|  start SRP; send salt + B
  |<------- M2 (salt, B) ---------------------|
  |--- M3 (A, proof M by PIN 3939) ---------->|  verify M -> M4 proof HAMK
  |<------- M4 (HAMK) ------------------------|
  |  shared secret = SRP K  -> installed as the ChaCha20 channel key
```

Both sides compute the SRP-6a session key `K` (3072-bit group, SHA-512) from
the fixed PIN `3939`; `K` becomes the encrypted-channel secret. No long-term
identity is exchanged, so nothing persists and anyone who "knows" 3939 can
connect. This is why 3939 is hardcoded and why changing it breaks pairing.

## Persistent pairing (the pincode) — target design

### pair-setup M1→M6

1. **M1→M4** — SRP-6a exactly as above, but for a *non-transient* client the
   session does **not** end at M4; `K` seeds HKDF to derive the keys for the
   encrypted identity envelopes (and ultimately the session key extracted
   differently than transient).
2. **M5** — the client sends `EncryptedData` (a HomeKit pairing envelope:
   ChaCha20-Poly1305 over a TLV blob) carrying its long-term Ed25519
   `PublicKey`, a `Signature`, and its `Identifier`. The signature binds the
   client's ephemeral and long-term keys so the receiver trusts it controls
   the long-term key.
3. **M6** — the receiver replies with its own encrypted `PublicKey` +
   `Signature` + `Identifier` (the Ed25519 identity it already persists in
   `identity`). After M6 both sides live-key the SRP `K` with the two
   long-term keys (HKDF) to the per-direction channel keys.
4. The receiver **persists** the controller's `(identifier, publicKey)` (and
   the SRP salt/verifier, to re-run setup later) — the `store` module.

### pair-verify M1→M4 (later connections)

1. **M1** — client sends its long-term Ed25519 `PublicKey` and an **X25519**
   ephemeral public key.
2. **M2** — receiver looks the public key up in its store; if unknown, refuse.
   Replies with its own long-term key, an X25519 ephemeral, a signature, and
   its identifier. Both sides derive the shared secret from the X25519
   ephemerals and the long-term keys (HKDF), then sign to prove key control.
3. **M3 / M4** — proof exchange; after M4 the derived secret is installed as
   the channel key, in place of transient's `K`.

This is the HomeKit Accessory Protocol "Pairing" sub-spec (HAP-R2).

## Crypto in use / needed

| Piece | Where |
| --- | --- |
| SRP-6a (3072-bit, SHA-512) M1→M4 | `srp.rs` (already present) |
| Ed25519 long-term identities + signatures | `ed25519-dalek` / `identity.rs` |
| X25519 ephemeral key agreement (pair-verify) | `x25519.rs` (Phase 1; `x25519-dalek`) |
| HKDF-SHA512 key derivation | `hkdf` + `sha2` |
| ChaCha20-Poly1305 (envelopes + channel) | `chacha20poly1305` |
| Paired-controller registry | `store.rs` (Phase 1) |

## Open items to pin on hardware (Phase 2 capture)

These are not assumed; they are captured from a real sender and recorded here
before phase 3/4 code is written:

- **M5/M6 identity envelope**: the exact HKDF salt/info strings and the
  derived-key layout (encryption key for the ChaCha20 envelope) that iOS uses.
- **pair-verify** M1→M4: the exact request/response TLVs (key fields,
  signature layout) and the HKDF info strings for the session key.
- **iOS prompt**: with a fresh receiver identity, does iOS show a
  "enter the setup code" dialog on first contact, and does it accept the
  configured `--password`? (This is what makes it a usable pincode.)
- **Advertising bits**: which `features`/`status` bits make a sender choose
  persistent pairing over transient (`sf`, etc.).
- **Storage**: what a controller's record must contain for a later
  `pair-verify` and a later re-setup.

These fill sections below; until then the "ground truth" is shairport-sync's
`pair_ap` and the HAP spec, cited above.
