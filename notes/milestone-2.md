# Milestone 2 — Transient pairing & channel encryption

Goal (from [`../notes.md`](../notes.md)): a stock sender completes **transient
pairing** with the receiver, after which the control channel is encrypted and
the sender's next (encrypted) request decrypts cleanly. This is the
crypto-heavy milestone — HomeKit SRP + HKDF + ChaCha20-Poly1305. Persistent
(non-transient) pairing, `pair-verify`, and FairPlay are deferred.

## Scope

In:

- **TLV8** (`tlv.rs`): HomeKit type-length-value encoding/decoding, including
  the >255-byte fragmentation rule (same type repeated, concatenated on
  decode). Types: Method=0, Identifier=1, Salt=2, PublicKey=3, Proof=4,
  EncryptedData=5, State=6, Error=7, Signature=10, Flags=19.
- **SRP-6a server** (`srp.rs`): the exact HomeKit variant —
  RFC 5054 **3072-bit** group, `g=5`, **SHA-512**, username `"Pair-Setup"`,
  password `"3939"`. Non-standard details that must match Apple (verified
  against shairport-sync's csrp fork):
  - `k = SHA512(N ‖ PAD(g))`, `u = SHA512(PAD(A) ‖ PAD(B))` (both padded to
    the 384-byte modulus length),
  - session key `K = SHA512(S)` (the hash of S, not S itself),
  - `M1 = SHA512((H(N)⊕H(g)) ‖ H(I) ‖ s ‖ A ‖ B ‖ K)` (s/A/B as minimal
    big-endian bytes),
  - `HAMK = SHA512(A ‖ M1 ‖ K)`,
  - salt is 16 random bytes.
- **pair-setup transient** (`pairing.rs`): the M1→M4 state machine.
  - M1 (State 1, Method 0, Flags 0x10) → M2 (State 2): Salt + PublicKey(B).
  - M3 (State 3): PublicKey(A) + Proof(M1) → M4 (State 4): Proof(HAMK),
    or an Error TLV on auth failure.
  - The shared secret for the session is `K` (64 bytes). No M5/M6, no
    identity exchange (that's persistent pairing).
- **Channel cipher** (`cipher.rs`):
  - Derive two ChaCha20-Poly1305 keys with HKDF-SHA512 over `K`: salt
    `Control-Salt`, info `Control-Write-Encryption-Key` (sender→us, we
    *decrypt*) and `Control-Read-Encryption-Key` (us→sender, we *encrypt*).
  - **HAP block framing**: each block is `[u16 len (LE)][ciphertext][16-byte
    tag]`, `len ≤ 1024`; AAD = the 2-byte length; nonce = 4 zero bytes ‖
    8-byte little-endian block counter; separate counters per direction.
- **Encrypted transport** (`crypto_stream.rs`): AsyncRead/AsyncWrite adapters
  that decrypt inbound blocks and encrypt outbound writes, so `http::` parsing
  works unchanged once the cipher is installed.
- **Wiring** (`server.rs`): a `POST /pair-setup` handler advancing the state
  machine; on transient completion, swap the connection onto the encrypted
  transport for subsequent requests. `GET /info` still works pre-pairing.

Out: `pair-verify` / persistent pairing, `fp-setup` / `auth-setup`, SETUP and
audio (milestone 3+).

## Module layout

```
src/tlv.rs           — TLV8 codec (new)
src/srp.rs           — SRP-6a server + the 3072-bit group (new)
src/pairing.rs       — pair-setup transient state machine (new)
src/cipher.rs        — HKDF channel keys + HAP block framing (new)
src/crypto_stream.rs — AsyncRead/AsyncWrite encryption adapters (new)
src/server.rs        — /pair-setup dispatch; install the cipher post-pairing
```

## Crate additions

| Concern | Crate |
|---|---|
| Big integers (SRP) | `num-bigint` (`num-bigint-dig` if constant-time needed later) |
| SHA-512 / HKDF | `sha2`, `hkdf` |
| ChaCha20-Poly1305 (IETF) | `chacha20poly1305` |
| Ed25519 / X25519 (later) | already have `ed25519-dalek` |

## Test strategy

- **TLV8** round-trips, including a >255-byte value that fragments and
  reassembles; integer and byte items.
- **SRP**: a matching SRP *client* (in tests) runs the full exchange against
  the server and both derive the **same** `K`; a wrong password fails
  verification. This client also drives the integration test.
- **Cipher**: HKDF keys are deterministic for a known `K`; encrypt→decrypt
  round-trips across a multi-block (>1024-byte) payload; the counter advances.
- **Encrypted transport**: write a request through the encrypting writer,
  read it back through the decrypting reader, assert the plaintext matches.
- **Integration** (`pairing.rs` test): a synthetic sender does the full
  transient `POST /pair-setup` (M1→M4) over TCP, installs the same cipher, and
  sends an encrypted `GET /info` — assert the receiver returns an encrypted
  `200` that decrypts to the device plist.

## Acceptance criteria

- `cargo test` / `cargo clippy` clean.
- The unit + integration tests above pass (SRP interop, cipher round-trip,
  encrypted `GET /info`).
- Manual (hardware): a Mac/iPhone selecting the receiver completes
  `POST /pair-setup` (State 1→4) and its following request decrypts — visible
  in the debug log. If the sender demands `fp-setup`/`auth-setup` first, that
  surfaces here as the next thing to handle.

## Result

Done. 31 tests pass (28 unit + 3 integration), clippy clean.

The crypto was verified layer by layer:
- **SRP interop** — a matching SRP client and the server derive the *same*
  64-byte `K`; a wrong PIN fails proof verification. This confirms the exact
  HomeKit variant (padded `k`/`u`, `K = H(S)`, the `M1`/`HAMK` formulas).
- **Channel cipher** — HKDF keys are deterministic; encrypt↔decrypt round-trips
  across multi-block (>1024-byte) payloads with the counter/nonce framing;
  partial trailing blocks are buffered.
- **Encrypted transport** — a plaintext request, then the cipher install, then
  an encrypted request+response all pass through one `ControlConnection`
  without read-ahead corruption at the switch.
- **End-to-end** (`tests/pairing.rs`) — a synthetic sender runs the full
  transient `POST /pair-setup` (M1→M4) over TCP, installs the same cipher, and
  sends an **encrypted `GET /info`**; the receiver's encrypted `200` decrypts
  to the device plist (asserting `deviceID`).

Design notes:
- The mode switch happens after the plaintext M4 response is written; the
  connection then reads/writes HAP-framed blocks. To switch cleanly the reader
  never reads past a message boundary in the clear (byte-at-a-time head,
  exact-length body).
- SRP is hand-rolled over `num-bigint` because the RustCrypto `srp` crate does
  not match Apple's non-standard hashing; a client side is included (public,
  used by the sender and the tests).

Not verified here (needs a real sender): that a Mac/iPhone actually uses
transient pairing and completes M1→M4. If it demands `fp-setup`/`auth-setup`
first, or uses persistent pairing + `pair-verify`, that shows up in the debug
log as the next milestone's input.
