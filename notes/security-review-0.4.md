# Security review — 0.4

Date: 2026-08-07. Scope and method: [plans/20260807-06-security-review.md](../plans/20260807-06-security-review.md).
Reviewer: Claude, at Stefan's request. Commit reviewed: `main` at the head of
the 0.4 work (roadmap, `.deb` packaging, CLI, `/etc/default` configuration and
endpoint password all merged or in flight).

## What was examined

The network-reachable surface, grouped by how far an attacker gets before
touching it, per the plan:

1. **Pre-pairing, unauthenticated** — `http.rs` (the HTTP/RTSP parser),
   `crypto_stream.rs` (the byte-at-a-time head read and the plaintext→cipher
   switch), `srp.rs` / `tlv.rs` / `pairing.rs` (SRP-6a, TLV parsing, the M1→M4
   state machine, the pincode comparison), `info.rs` (the `/info` disclosure),
   and the `server.rs` accept loop.
2. **Post-pairing audio path** — `buffered.rs` (per-packet framing + AEAD),
   `decode.rs` (the AAC decoder), `session.rs` (two-phase SETUP, the flush/seq
   handling, the buffered-audio pipeline), `dmap.rs` (the metadata walker).
3. **The now-playing endpoint** — `openairplay2-receiver/src/tui.rs` (the
   `Authorization` check and its constant-time compare).
4. **`unsafe` and process boundaries** — the `unsafe` blocks (`gethostname`,
   `signal`, the tui's `poll`/`ioctl`/`winsize`), `identity.rs` (the on-disk
   key), `avahi.rs` (D-Bus registration).
5. **Dependencies** — `cargo audit` over the 247-crate tree.

## Method

`cargo audit`, then a reachability-guided read of groups 1→4 against the threat
model (looking for slice indexing and allocation sized from the wire, unbounded
reads, the two secrets in any log/timing path, and each `unsafe` block's
precondition), then adversarial tests for what the read flagged.

## Result

**No memory-safety defect and no remote code-execution path was found.** The
wire parsers are uniformly bounds-checked and fail closed, the two crown-jewel
crypto paths are correct, and `cargo audit` is clean (0 vulnerabilities, 0
warnings). Three findings, all availability/hardening rather than integrity:
two are fixed in this stack with regression tests, one is filed for later.

### Findings

**F1 — Unauthenticated resource-exhaustion DoS (fixed in this stack).**
The control accept loop imposed no ceiling on concurrent connections and no
timeout on an unpaired peer, and `crypto_stream::read_exact_n` reserves the
declared `Content-Length` (up to `MAX_BODY` = 8 MB) up front. A LAN peer could
therefore open unbounded connections and hold each open indefinitely by
dribbling header bytes (slowloris), and/or reserve 8 MB per connection, denying
service to the real sender — the one thing in the house playing music.
*Fix:* a `MAX_CONNECTIONS` (32) semaphore in the accept loop (over the ceiling,
accept-and-drop rather than queue), and a `HANDSHAKE_TIMEOUT` (10 s) applied to
`read_request` **only while the channel is unencrypted** — a pairing sender is
actively talking, so it never disturbs an established, legitimately idle
session, but a slowloris peer never reaches encryption so every read it makes is
time-bounded. Regression test: `excess_connections_are_refused`
([tests/pairing.rs](../openairplay2/tests/pairing.rs)). *Issue:* [#87](https://github.com/st3fan/openairplay2/issues/87).

**F2 — Persistent identity key written world-readable (fixed in this stack).**
`identity.rs` persisted the Ed25519 **signing seed** (the private key) with
`fs::write`, which creates the file 0644 minus umask. On a hand-run receiver at
`~/.config/openairplay2/identity` that lets any other local user read the
private key and impersonate the receiver. (The packaged service is mitigated:
systemd's `StateDirectory` gives `/var/lib/openairplay2` mode 0700 owned by the
service user — but the library API itself was the leak.) *Fix:* `write_private`
creates the file 0600 on Unix and reasserts the mode on rewrite. Regression
test: `persisted_identity_is_owner_only`
([identity.rs](../openairplay2/src/identity.rs)). *Issue:* [#88](https://github.com/st3fan/openairplay2/issues/88).

**F3 — No rate-limit or lockout on pincode attempts (deferred, issue filed).**
SRP-6a makes each `--pincode` guess require a full online `pair-setup` round —
it cannot be brute-forced offline — but nothing limits how many rounds a peer
may attempt, and a new connection resets the state. A 4-digit pincode is 10 000
combinations; a LAN attacker could exhaust that online. **Not release-blocking:**
the pincode is an opt-in, deliberately-modest gate (the default is transient
pairing with *no* code), and F1's connection cap plus a per-connection SRP cost
already slow a brute-forcer. Deferred:
[#89](https://github.com/st3fan/openairplay2/issues/89) describes a per-source
backoff / attempt cap as the shape of a fix.

### Examined and found sound (no action)

- **`http.rs` / `crypto_stream.rs`** — `MAX_HEAD` (16 KB) and `MAX_BODY` (8 MB)
  are enforced on the production path; the head is read to an exact boundary and
  never past it in the clear; `Content-Length` is parsed with a checked
  `usize`. (Minor: the `#[cfg(test)]` `MAX_*` constants in `http.rs` duplicate
  the real ones in `crypto_stream.rs` — cosmetic, noted, not filed.)
- **`srp.rs`** — the SRP-6a `A mod N == 0` safety check is present, `M1` and
  `HAMK` are compared in constant time, a wrong password fails verification.
- **`tlv.rs` / `dmap.rs`** — every length read is bounds-checked with
  `get(...)?` / an explicit `len > rest.len()` guard; TLV lengths are `u8`, DMAP
  recursion is one level deep and not attacker-controlled; no allocation is
  sized from an unchecked wire value.
- **`buffered.rs`** — packet length is validated before slicing; decryption is
  ChaCha20-Poly1305 AEAD, so post-pairing audio is authenticated and cannot be
  forged; block framing is bounds-checked.
- **`decode.rs` / `session.rs`** — the AAC decoder sees only authenticated
  (post-pairing) bytes; a decode error is logged and skipped, and a decoder
  panic would kill only the spawned audio task, not the process.
- **`tui.rs` endpoint auth** — the `Authorization: Bearer` check compares in
  constant time over the max length and rejects with 401 before the upgrade;
  the wire format is untouched.
- **`unsafe` blocks** — `gethostname` bounds its buffer, checks the return, and
  converts with a NUL-terminated slice; `signal(SIGPIPE, SIG_DFL)` is scoped to
  print-and-exit paths; the tui's terminal `ioctl`/`poll`/`winsize` are local
  and not attacker-reachable.
- **Dependencies** — `cargo audit`: 0 vulnerabilities, 0 warnings.

## Recommendations carried forward

- **Watching the dependency surface** — a `cargo audit` CI job was added here,
  then removed again pending a better approach to advisory scanning. The
  dependency tree is still clean as of this review; keeping it watched is open
  work.
- **Endpoint bind guidance** — the README/options-file advice to keep
  `--tui-listen` on loopback unless a password is set stands; F1's fixes do not
  change that the endpoint, once bound to a routable address without a password,
  is readable by the LAN.

## Bottom line

Nothing found blocks the 0.4 release on integrity or code-execution grounds.
The two availability/disclosure defects worth fixing before strangers run this
(F1, F2) are fixed here with tests; the one hardening gap (F3) is filed and safe
to schedule later. "We looked" is now a dated, scoped fact.
