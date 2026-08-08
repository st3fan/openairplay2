# Security review

Roadmap 0.4, the last item: put the network surface through a security review
before a release we are actively encouraging strangers to install on their own
networks. This plan defines the threat model, the surface, the method, and what
happens to what it finds — the review itself is the phase(s) stacked on top.

## Why now

0.4 turns openairplay2 from "a thing its author runs" into "a thing a stranger
installs from a `.deb` and leaves running." What that stranger installs is a
daemon that binds a well-known port and speaks pairing, an encrypted channel,
and a media decoder to **anything on the network that connects** — before any
trust is established. A receiver on a home LAN is reachable by every device on
that LAN, including ones the owner did not think about (a guest phone, a smart
TV, a compromised IoT bulb). This is the moment to look hard, while the surface
is still small enough to hold in one head.

## Threat model

**The attacker** is anyone who can open a TCP connection to port 7000, or send
UDP to the data/control ports once a session exists — i.e. any host on the same
L2/L3 network. Not a remote internet attacker (the receiver is not meant to be
port-forwarded; if it is, that is out of scope and the docs should say so), and
not a local-user threat beyond what is already addressed (the pincode/password
`ps` leak, fixed by the `/etc/default` work).

**What we are protecting:**

- **The host process and machine** — no memory-safety break, no panic that a
  remote peer can trigger at will (a receiver that a single malformed packet
  can crash is a trivial DoS on the one thing in the house playing music), no
  resource exhaustion that takes the box down.
- **The audio path's integrity** — an unpaired or wrongly-paired peer must not
  be able to inject or hijack audio, and the pincode gate must actually gate.
- **The two secrets** — the pincode and the endpoint password must not leak
  through logs, timing, or error text; the persistent identity key must not be
  disclosed or trivially overwritten.
- **The now-playing endpoint** — with a password set it must not be bypassable;
  without one it must expose only what it is meant to (metadata + art), not the
  filesystem or the process.

**Explicitly out of scope** (named so the review stays bounded, and so the
"declined" list in the roadmap gains any that deserve issues):

- The FairPlay handshake's cryptographic *correctness* — it is a canned,
  third-party-derived interop table ([notes/licensing.md](../notes/licensing.md)),
  not a security boundary we designed, and AirPlay 2's real access control is
  the pairing, which we do review.
- PTP / multi-room — not implemented, no surface.
- Supply-chain attestation of our *own* releases — already covered (signed
  provenance on every `.deb`, trusted publishing on crates.io).
- Physical/local-root attackers.

## Surface to review

Grouped by how far an attacker gets before touching it. The first two groups
are the crown jewels: reachable with **zero** authentication.

1. **Pre-pairing, unauthenticated** — everything
   [server.rs](../openairplay2/src/server.rs) `dispatch` and the pair-setup
   path reach before a cipher exists:
   - [http.rs](../openairplay2/src/http.rs) — the hand-written HTTP/RTSP
     message parser, reading `Content-Length` bodies a byte at a time. Header
     count/size limits, `Content-Length` trust, integer handling.
   - [crypto_stream.rs](../openairplay2/src/crypto_stream.rs) — the
     read-a-byte-until-boundary logic and the plaintext→cipher switch; the
     invariant that it never reads past a message boundary in the clear.
   - [srp.rs](../openairplay2/src/srp.rs) / [tlv.rs](../openairplay2/src/tlv.rs)
     / [pairing.rs](../openairplay2/src/pairing.rs) — SRP-6a verifier math,
     TLV parsing of attacker-supplied bytes, the M1→M4 state machine, and the
     **pincode comparison** (constant-time? does a wrong code fail closed?).
   - [info.rs](../openairplay2/src/info.rs) — the plist built for `GET /info`;
     what it discloses to an unpaired scanner.
2. **Post-pairing audio path** — reachable once a peer completes pairing (the
   pincode raises that bar, transient does not):
   - [buffered.rs](../openairplay2/src/buffered.rs) — per-packet framing and
     ChaCha20-Poly1305 decryption; the `[u16 len][packet]` bounds, the
     `packet[..]` slice indexing, nonce construction, backpressure limits.
   - [decode.rs](../openairplay2/src/decode.rs) — feeding attacker-controlled
     (but authenticated) bytes to `symphonia`'s AAC decoder; what a malformed
     frame does, and whether decode output size is bounded.
   - [session.rs](../openairplay2/src/session.rs) — the two-phase SETUP, the
     flush-boundary/seq handling, and the un-arbitrated second connection
     (the 0.6 "second sender" item — here we only confirm it cannot corrupt
     memory or the key, not that it behaves well).
   - [dmap.rs](../openairplay2/src/dmap.rs) — the DMAP/DAAP walker over
     `SET_PARAMETER` blobs; nested-length trust, allocation from attacker
     sizes.
3. **The now-playing endpoint** — [tui.rs](../openairplay2-receiver/src/tui.rs):
   the `Authorization` check and its constant-time compare, the 401 path, and
   whether an authorized (or password-less) client can reach anything beyond
   the intended messages.
4. **`unsafe` and process boundaries** — the handful of `unsafe` blocks
   (`gethostname`, `signal`, the tui's `poll`/`ioctl`/`winsize`), the identity
   file's on-disk permissions and load/overwrite behavior
   ([identity.rs](../openairplay2/src/identity.rs)), and the Avahi D-Bus
   registration ([avahi.rs](../openairplay2/src/avahi.rs)).
5. **Dependencies** — a `cargo audit` / `cargo deny` pass over the tree for
   known-vuln advisories and for anything surprising pulled in transitively.

## Method

A layered pass, cheapest and most mechanical first:

1. **Tooling** — `cargo audit` (RustSec advisories) and `cargo deny check`
   (advisories + licenses + bans) run and recorded; if clean, say so; if not,
   each advisory is triaged (reachable? fixable by bump?). Decide whether one
   or both belongs in CI as a standing check — likely yes, and cheap.
2. **Reachability-guided manual review** — read groups 1→4 in that order,
   against the threat model, looking specifically for: slice indexing on
   attacker-controlled lengths (the `panic!`/`unwrap`/`[a..b]` density in the
   library is ~160 call sites — most are fine, the review finds the ones on an
   untrusted path), allocations sized from the wire, unbounded reads/buffers,
   the two secrets in any log/error/timing path, and each `unsafe` block's
   soundness precondition.
3. **Adversarial probes for anything the read flags** — turn a suspected
   crash-on-malformed-input into a test that actually feeds the bytes (the repo
   already drives the real server over a real socket in
   `openairplay2/tests/pairing.rs`; a malformed-input harness extends that).
   A confirmed remote panic is a finding with a repro, not a hunch.

The `/security-review` skill is the natural driver for step 2 over the
branch/diff; this plan is the threat model and surface map it runs against, so
the review is grounded rather than generic.

## Deliverables and what happens to findings

The review's output is **not** code in this stack by default — it is a written
findings report (committed as `notes/security-review-0.4.md`) plus one GitHub
issue per finding, labeled `bug` (or `enhancement` for hardening that isn't a
defect), severity in the title, each with the observation, why it happens, and
the shape of a fix — the repo's standing convention.

Then, triaged with Stefan:

- **Release-blocking** (a remote can crash or take over the process, a secret
  leaks, the password/pincode gate is bypassable): fixed in this stack as
  stacked phases before 0.4 ships.
- **Not blocking** (defense-in-depth, limits that are generous but finite,
  hardening): the issue stands and is scheduled later; the review says so
  explicitly rather than silently dropping it.

A review that finds nothing release-blocking is a valid and good outcome — it
is recorded as such, with what was examined, so "we looked" is a fact with a
date and a scope, not a vibe.

## Test strategy

- Every confirmed input-handling finding gets a regression test that feeds the
  offending bytes to the real parser/server and asserts it fails safe (error,
  not panic) — these live beside the existing integration tests and are the
  proof a fix holds.
- `cargo audit` / `cargo deny` added to CI (if step 1 concludes they should
  be), so the dependency surface stays watched rather than reviewed once.
- No behavior change to the supported path: a well-formed sender must pair,
  stream, and control exactly as before — the existing suite plus a hardware
  pass against a real Mac/iPhone is the guard.

## Acceptance criteria

- `cargo audit` and `cargo deny` have been run; the result is recorded, and any
  standing CI check that should exist, does.
- Groups 1–5 have each been reviewed against the threat model, with the review
  written down in `notes/security-review-0.4.md` (scope, method, findings,
  what was explicitly not covered).
- Every finding is a labeled GitHub issue; every release-blocking finding is
  fixed in this stack with a regression test; every deferred finding says why
  it is safe to defer.
- The supported pairing/streaming/control path is unchanged, verified by the
  suite and a hardware pass.
- The roadmap's 0.4 security-review item can be checked off against a dated,
  scoped artifact.
