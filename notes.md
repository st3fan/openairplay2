# OpenAirPlay 2 — AirPlay 2 receiver, design & plan

> **Historical.** This is the pre-implementation protocol research and the
> original milestone plan, kept as a record of what was known before anything
> was built. It is *not* a description of the receiver as it stands — several
> things here were superseded by what the hardware actually did (the buffered
> data channel turned out to be TCP, not UDP; pair-verify, ALAC, realtime audio
> and PTP were never implemented). For what exists today see
> [`notes/status.md`](notes/status.md), the [README](README.md) and
> [`CLAUDE.md`](CLAUDE.md).

## Goal

A minimal **AirPlay 2 audio receiver** for Linux that stock Apple clients
(iPhone, iPad, Mac from macOS 10.15+, Apple TV, HomePod as a sender) can
discover and stream audio to, played to an ALSA device — the AirPlay 2
counterpart of the working AirPlay 1 receiver in `../openairplay1`.

Scope, in rough order of ambition, is to reach shairport-sync's "Basic" tier:
**ALAC / S16 / 44100 / stereo realtime audio**, then **AAC buffered audio**.
Explicitly *not* in scope (at least initially): 96/192 kHz hi-res, Dolby
Atmos, surround, screen mirroring, remote control, being added to the Home
app as a persistent HomeKit accessory, and multi-room grouping.

> **Reality check.** AirPlay 2 is a much larger undertaking than AirPlay 1.
> AirPlay 1 was ~one well-known RSA key, AES-CBC, and an RTSP text protocol.
> AirPlay 2 replaces essentially all of that: HomeKit-style pairing
> (SRP + Curve25519 + Ed25519), a ChaCha20-Poly1305-encrypted control
> channel carrying **binary plists**, per-packet ChaCha20-Poly1305 audio
> encryption, AAC as well as ALAC, and PTP (IEEE-1588) timing. Plan for this
> to be several times the work of v1.

## How AirPlay 2 differs from AirPlay 1

| Concern | AirPlay 1 (RAOP) | AirPlay 2 |
|---|---|---|
| Discovery | `_raop._tcp`, simple TXT | `_airplay._tcp` (+ `_raop._tcp`), 64-bit `features` bitmask, `pk` (Ed25519 pubkey) |
| Auth | RSA `Apple-Challenge` (well-known key) | HomeKit pairing: SRP-6a `pair-setup` → X25519/Ed25519 `pair-verify`; optional FairPlay `fp-setup` |
| Control channel | plaintext RTSP text | RTSP-like over TCP, **encrypted** (ChaCha20-Poly1305) after pairing, **binary plist** bodies |
| Session key | AES-128 via RSA-OAEP | shared secret from pairing → HKDF-SHA512 → per-channel keys |
| Audio crypto | AES-128-CBC (IV per packet) | ChaCha20-Poly1305 AEAD per packet (8-byte nonce, AAD from header) |
| Codecs | ALAC/S16/44100/2 only | ALAC + AAC(-ELD) + PCM + OPUS; S16/S24/F24; 44100/48000; stereo/5.1/7.1 |
| Stream model | one realtime stream | **realtime** (~2 s latency) *and* **buffered** (short latency, sender pushes ahead) |
| Timing | NTP-ish over the RAOP timing UDP channel | **PTP** (IEEE-1588, UDP 319/320) by default; NTP mode negotiable |
| Channels | RTSP + 3 UDP (audio/control/timing) | RTSP + **event** (TCP), **data** (RTP), **control** (RTCP), **timing** (PTP/NTP) |

## Protocol overview

### 1. Discovery (`_airplay._tcp` via Avahi)

Advertise `_airplay._tcp` (we can reuse the Avahi D-Bus registration from
v1). The important TXT keys:

- `features=0x<lo>,0x<hi>` — a 64-bit capability bitmask, split into two
  32-bit hex halves. Individual bits gate behaviour; the ones that matter for
  a minimal receiver:
  - **bit 48** — supports **transient pairing** (lets us avoid a user PIN and
    FairPlay for the common case)
  - **bit 27 / bit 19 region** — "supports AirPlay audio" / RAOP bridging
  - **bit 59** — supports "stream connection" / bufferless
  - **bit 61** — RFC 2198 redundancy
  - Getting this bitmask right is what makes a modern sender offer AirPlay 2
    audio (vs. falling back to AirPlay 1). Copy a known-good value from
    shairport-sync / owntone and pare down from there.
- `pk=<hex>` — our long-term **Ed25519 public key** (identity).
- `deviceid`, `srcvers` (e.g. `366.0`), `model`, `flags`, `acl`, `gid`,
  `pi` (public identifier UUID).
- We likely still advertise `_raop._tcp` too, for the fallback path.

### 2. Pairing / authentication (the crypto milestone)

All pairing messages are **TLV8** (HomeKit's type-length-value byte format,
with fragmentation for values > 255 bytes), POSTed to RTSP/HTTP endpoints.

- **`POST /pair-setup`** — establishes a shared secret via **SRP-6a**
  (3072-bit group, **SHA-512**, username `"Pair-Setup"`):
  - *Transient* (no on-screen code): fixed PIN `3939`, a **two-step**
    exchange (M1→M2, M3→M4) yielding the 64-byte SRP session key `K` directly.
    This is the path to target first — no UI, no FairPlay.
  - *PIN / non-transient*: `POST /pair-pin-start` shows a code, then a
    **six-step** M1–M6 exchange that additionally swaps **Ed25519** identity
    keys (encrypted with ChaCha20-Poly1305, HKDF salt/info
    `"Pair-Setup-Encrypt-Salt"` / `"Pair-Setup-Encrypt-Info"`, nonces
    `"PS-Msg05"`/`"PS-Msg06"`) so the pairing persists across sessions.
- **`POST /pair-verify`** — every subsequent session: a two-step
  **Curve25519 (X25519)** ECDH plus **Ed25519** signatures over the exchanged
  public keys (HKDF salt/info `"Pair-Verify-Encrypt-Salt"` /
  `"Pair-Verify-Encrypt-Info"`, nonces `"PV-Msg02"`/`"PV-Msg03"`). Produces
  the session shared secret.
- **`POST /fp-setup`** — FairPlay. Can be **avoided** for the transient path;
  only some senders/DRM'd content require it (FairPlay v3 has been
  reverse-engineered but is a large sub-project — defer).

**Session key derivation** — from the pairing shared secret, HKDF-SHA512
derives independent ChaCha20-Poly1305 keys per channel (salt / info strings,
verbatim from shairport `pair_ap`):

| Channel | HKDF salt | HKDF info (write / read) |
|---|---|---|
| Control (RTSP) | `Control-Salt` | `Control-Write-Encryption-Key` / `Control-Read-Encryption-Key` |
| Events | `Events-Salt` | `Events-Write-Encryption-Key` / `Events-Read-Encryption-Key` |
| Data stream | `DataStream-Salt`+`<seed>` | `DataStream-Output-Encryption-Key` / `DataStream-Input-Encryption-Key` |

After `pair-verify`, the RTSP/TCP control channel is **framed and encrypted**:
each block is `[u16 length][ciphertext][16-byte Poly1305 tag]`, ChaCha20-
Poly1305 with a per-direction key and a counter nonce, the length as AAD.

### 3. Control channel (encrypted RTSP + binary plists)

RTSP-like requests over the (now encrypted) TCP connection; bodies are
**binary plists** (`plist` crate). Methods to handle:

- `GET /info` — capabilities/status plist (device info, supported formats,
  `features`). Sent before and sometimes during a session.
- `POST /pair-setup`, `/pair-verify`, `/fp-setup`, `/auth-setup`.
- `SETUP` — two phases (see below).
- `RECORD` — start streaming.
- `SETRATEANCHORTIME` — anchors an RTP timestamp to a network/PTP time (the
  AirPlay 2 equivalent of the sync anchor) and sets the playback rate; the
  key input to latency-correct playback.
- `FLUSHBUFFERED` — flush the buffered-audio queue up to a seq/timestamp.
- `SET_PARAMETER` / `GET_PARAMETER` — volume, progress, artwork, metadata.
- `POST /feedback` — periodic heartbeat (keep the session alive).
- `POST /command`, `POST /audioMode` — remote-control / routing (mostly
  acknowledge).
- `TEARDOWN` — tear down a stream or the whole session.

#### SETUP phase 1 — timing & event channel

Sender plist announces `deviceID`, `model`, `osVersion`, encryption params
(`ekey`, `eiv`, `et`), and the **timing protocol** (`timingProtocol` =
`"PTP"` or `"NTP"`) plus `timingPeerInfo`/`timingPeerList`. Receiver replies
with an `eventPort` (TCP, encrypted, receiver→sender notifications), and a
`timingPort` (UDP, only for NTP; zero for PTP).

#### SETUP phase 2 — audio stream(s)

Sender plist has a `streams` array; per stream:

- `type` — `96` realtime audio, `103` buffered audio, `110` screen,
  `130` remote control.
- `ct` (compression) — `1`=PCM, `2`=ALAC, `4`=AAC, `8`=AAC-ELD, `32`=OPUS.
- `audioFormat` — a bitmask encoding rate/depth/channels.
- `spf` — samples per frame (e.g. 352 for ALAC, 1024 for AAC).
- `shk` — the per-stream shared key for audio payload AEAD.
- `controlPort` — sender's RTCP port.

Receiver replies with its own `dataPort` (RTP audio in) and `controlPort`
(RTCP), and for buffered audio a `type`/buffer size. Bind those UDP sockets.

### 4. Audio data & decryption

RTP-framed packets arrive on the data port. Payload is **ChaCha20-Poly1305**
AEAD-encrypted (per shairport `rtp.c`): the last 8 bytes are the nonce
(front-padded with 4 zero bytes → 12-byte IETF nonce), the ciphertext is the
middle, a Poly1305 tag follows it, and 8 bytes of the RTP header are the AAD.
Decrypt with the stream `shk`-derived key → the ALAC/AAC frame. Then decode:

- **ALAC** — reuse the `alac` crate (already validated in v1).
- **AAC / AAC-ELD** — needs an AAC decoder. Options: `symphonia`
  (pure-Rust, has an AAC-LC decoder; ELD support is the open question) or
  FFmpeg bindings (`ffmpeg-next`) which shairport relies on for the
  floating-planar AAC path. AAC is the bigger unknown.

### 5. Timing (the hard part)

AirPlay 2 buffered/realtime audio is normally clocked with **PTP (IEEE-1588)**
on UDP ports **319/320**, which need to be free and want near-real-time
scheduling. shairport-sync offloads this to a separate helper, **NQPTP**
("Not Quite PTP"), that owns those ports. Two realistic paths:

1. **NTP mode first.** SETUP phase 1 can negotiate `timingProtocol: "NTP"`;
   if senders will accept it, reuse the v1 NTP clock model to get audio out
   with far less effort. (Not all modern senders offer NTP — needs testing.)
2. **PTP.** Implement (or port NQPTP's logic for) a minimal PTP slave: parse
   Sync/Follow_Up/Delay_Req/Delay_Resp on 319/320, maintain the offset, and
   feed `SETRATEANCHORTIME` anchors into the same latency-correct-start +
   drift machinery proven in v1. This is the genuinely hard 20%.

Encouraging precedent: the openairplay Python receiver played **buffered
audio before it had any real PTP/NTP sync**, using basic latency
compensation — so first sound does not require perfect timing.

## Crate choices (candidate)

| Concern | Crate |
|---|---|
| Async runtime / sockets | `tokio` |
| SRP-6a (3072-bit, SHA-512) | `srp` if its params fit; likely a hand-roll over `num-bigint` + `sha2` (AP2's SRP is non-standard) |
| X25519 / Ed25519 | `x25519-dalek`, `ed25519-dalek` |
| ChaCha20-Poly1305 (IETF) | `chacha20poly1305` |
| HKDF-SHA512 | `hkdf` + `sha2` |
| TLV8 | hand-rolled (small) |
| Binary plist | `plist` |
| ALAC decode | `alac` (reused from v1) |
| AAC decode | `symphonia` (try first) or `ffmpeg-next` (fallback) |
| ALSA output | `alsa` (reused from v1) |
| mDNS | Avahi over D-Bus with `zbus` (reused from v1) |
| PTP | hand-rolled minimal slave (milestone 5) |

Reusable from `../openairplay1` almost verbatim: the ALSA player (prebuffer,
timed start, drift), the ALAC decoder wrapper, the jitter buffer, the NTP
clock model, and the Avahi D-Bus registration.

## Proposed milestones

Each milestone is a shippable, verifiable increment, mirroring the v1
approach (branch → plan → implement → test → PR, and verify on hardware
where possible with a Mac / iPhone as the sender).

- **M1 — Discovery & `/info`.** Advertise `_airplay._tcp` with an AirPlay-2
  `features` bitmask + `pk`; run the TCP server; answer `GET /info` with a
  plist. Goal: a Mac/iPhone lists the receiver as an AirPlay **2** device and
  begins the pairing handshake (even if we then reject it). Reuse v1's Avahi
  and server scaffolding.
- **M2 — Transient pairing & channel encryption.** TLV8 codec; `pair-setup`
  (transient, code 3939, SRP-6a) and `pair-verify` (X25519+Ed25519); derive
  the session keys; wrap the RTSP TCP channel in ChaCha20-Poly1305 framing.
  Goal: the sender completes pairing and its next (encrypted) request
  decrypts cleanly. This is the crypto-heavy milestone.
- **M3 — SETUP & encrypted audio in.** Parse the two-phase SETUP plists; bind
  event/data/control channels; handle RECORD / SETRATEANCHORTIME /
  FLUSHBUFFERED / TEARDOWN / feedback. Receive RTP audio and ChaCha20-Poly1305
  decrypt it. Goal: verify decrypted payloads are valid ALAC (as in v1 M2).
- **M4 — Decode & play (realtime ALAC first).** ALAC/S16/44100/2 realtime →
  reuse the v1 ALSA player. Goal: **sound**, starting with the realtime path
  (closest to v1). Then add AAC-LC buffered decode.
- **M5 — Timing & sync.** Start with NTP-mode negotiation if senders allow;
  otherwise a minimal PTP slave on 319/320. Feed SETRATEANCHORTIME anchors
  into the v1 latency-correct-start + drift model. Goal: correctly-timed,
  glitch-free playback.
- **M6 — Robustness & formats.** Buffered-audio buffer management, retransmit
  handling, volume/metadata/feedback, 48 kHz and S24, teardown edge cases,
  and graceful fallback to `_raop._tcp` (AirPlay 1) when appropriate.

Optional later: FairPlay `fp-setup` (v3), persistent (non-transient) pairing
+ Home-app add, surround, buffered lossless (ALAC/S24/48000).

## Biggest risks / open questions

- **PTP timing** — the hardest piece; may need privileged ports and tight
  scheduling. Mitigate by trying NTP mode and by deferring perfect sync
  (buffered audio can start without it).
- **AAC(-ELD) decoding** — need a decoder that handles Apple's
  floating-planar AAC; `symphonia`'s coverage of AAC-ELD is uncertain, so an
  FFmpeg binding may be unavoidable. ALAC-only realtime sidesteps this for
  first sound.
- **The `features` bitmask** — getting it wrong means the sender offers
  AirPlay 1 (or nothing) instead of AirPlay 2. Start from a known-good value.
- **SRP-6a variant** — AP2 uses a 3072-bit group with SHA-512 and specific
  padding; existing Rust SRP crates may not match, so budget for a hand-roll.
- **FairPlay** — avoidable for transient pairing + non-DRM sources, but some
  senders/content may still demand it.
- **One instance per host** — AP2 senders get confused by multiple AirPlay 2
  players at the same IP; run a single instance.

## Sources

- [shairport-sync `AIRPLAY2.md`](https://github.com/mikebrady/shairport-sync/blob/master/AIRPLAY2.md) and its [`pair_ap`](https://github.com/mikebrady/shairport-sync/tree/master/pair_ap) library — authoritative for the pairing crypto (SRP params, HKDF salt/info strings, ChaCha20 nonces) and the audio-packet AEAD layout; also documents streams, formats, and the NQPTP timing helper.
- [NQPTP](https://github.com/mikebrady/nqptp) — the PTP timing companion (ports 319/320).
- [Emanuele Cozzi, "AirPlay 2 Internals — RTSP"](https://emanuelecozzi.net/docs/airplay2/rtsp/) — RTSP methods, the two-phase SETUP plists, channel structure, `ct`/stream types, encryption model.
- [openairplay/airplay2-receiver](https://github.com/openairplay/airplay2-receiver) (Python) — a working reference: transient/non-transient/FairPlay-v3 pairing, ChaCha20-Poly1305 audio, and the useful data point that buffered audio plays before full PTP/NTP sync. Features-bit notes (transient=48, stream=59, RFC2198=61).
- [ckdo/airplay2-receiver](https://github.com/ckdo/airplay2-receiver) — the original of the above.
- [pyatv protocols](https://pyatv.dev/documentation/protocols/) — cross-check on pairing and the `features` flags.
- FairPlay/HAP background: [AirPlayAuth](https://github.com/funtax/AirPlayAuth), Apple's HomeKit Accessory Protocol (HAP) TLV8 + pairing.
- Reused precedent: `../openairplay1` (this project's AirPlay 1 receiver) for the ALSA player, ALAC decoder, jitter buffer, NTP clock model, and Avahi D-Bus registration.
