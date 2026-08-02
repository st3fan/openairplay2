# Track metadata and artwork events (requested by radio)

Goal: surface the **track metadata and cover art the sender already
sends** so embedding hosts can display them. The requirements here were
written by the consumer: [st3fan/radio](https://github.com/st3fan/radio) embeds
this library (its `radiod` daemon is an AirPlay receiver + internet
radio), and its web dashboard's AirPlay view currently shows a
`— NO TRACK INFO —` placeholder and an animated stand-in where cover
art could be. Everything below is what that consumer needs; the exact
API shapes are proposals — improve them if the library has better
idioms, but keep the integration contract (events on the existing
channel) intact.

## Background: where the data already is

AirPlay senders push rich now-playing info over the RTSP control
channel via `SET_PARAMETER`, distinguished by `Content-Type`:

- `application/x-dmap-tagged` — **track metadata** as a DMAP/DAAP
  binary payload: 4-byte tag codes with big-endian u32 lengths, nested
  in an `mlit` (dmap.listingitem) container. The tags the consumer
  cares about:
  - `minm` (dmap.itemname) → **title**
  - `asar` (daap.songartist) → **artist**
  - `asal` (daap.songalbum) → **album**
- `image/jpeg` / `image/png` (also seen: `image/none` with an empty
  body meaning "no art") — **cover art**, raw bytes, typically tens to
  a few hundred KB.
- `text/parameters` with `progress: start/current/end` — playback
  position as RTP timestamps. (Milestone 7 already handles the
  `volume:` flavor of `text/parameters`.)

Senders transmit these at session start and on every track change —
this is **push, not poll**. The consumer explicitly considered asking
for a polling accessor ("periodically poll for metadata changes") and
concluded push fits better: the library already owns an event channel,
`Receiver::run` consumes the receiver so there is no handle left to
poll, and the radio's dashboard polls its *own* daemon status anyway —
so events land in the daemon's shared state and the dashboard picks
them up on its existing 2.5 s cycle. No new polling surface needed.

## The requested API

Two additive `Event` variants (the enum is `#[non_exhaustive]`, so
this is a compatible change for existing embedders):

```rust
/// SET_PARAMETER application/x-dmap-tagged. Fields the payload did not
/// carry are None; a new event replaces the previous one wholesale.
Event::Metadata {
    title: Option<String>,   // minm
    artist: Option<String>,  // asar
    album: Option<String>,   // asal
},

/// SET_PARAMETER image/*. `data` empty (or content_type image/none)
/// means "the sender cleared the artwork".
Event::Artwork {
    content_type: String,    // "image/jpeg" | "image/png"
    data: Vec<u8>,
},
```

Contract details the consumer depends on:

- **Ordering:** both events arrive only between `SessionStarted` and
  `SessionEnded` for their session. The library must *enforce* this,
  because the wire does not: `SessionStarted` is only emitted at SETUP
  phase 2, and a sender may push metadata earlier in the handshake. So
  the session should **latch** the most recent metadata/artwork that
  arrives while no session is active and emit them immediately after
  `SessionStarted`, rather than dropping them (dropping would lose the
  first track's info) or emitting them early (which would break this
  contract). Note the existing `Volume` event is *not* gated this way —
  fine to leave as is; its contract predates this milestone. The
  consumer clears its display state on `SessionEnded`/session switch
  itself — the library does not need to send explicit clear events (but
  `image/none` should still be forwarded as the empty-artwork case,
  since it can happen mid-track).
- **Replacement semantics:** each `Metadata` event is a complete
  statement (not a delta). If a sender sends title-only, artist/album
  are `None` and the consumer will blank them. That matches how DMAP
  payloads actually arrive (one `mlit` per track change).
- **Duplicates are fine** — the consumer is idempotent.
- **Unknown DMAP tags are skipped silently**; the parser should only
  need tag-code + length walking, not a general DAAP implementation.
  Watch for the container: the three wanted tags sit *inside* `mlit`.
- **Strings are UTF-8** (lossy conversion acceptable).
- Artwork should be delivered as-is (no decoding/resizing in the
  library). A payload-size cap already exists: the encrypted channel
  reads each body to its exact `Content-Length` under an 8 MB `MAX_BODY`
  limit ([crypto_stream.rs](../openairplay2/src/crypto_stream.rs)), so
  malformed-sender protection is covered without new code.

Out of scope for the consumer (do only if trivial): `progress`
forwarding; a sender-displayed-name field on `SessionStarted` (noted
in earlier plans as wanted, separate concern); any artwork endpoint —
serving the bytes to browsers is the consumer's job.

## Fit with the current code (library-side survey)

Checked against the code as of 2026-08-02:

- **The dispatch gap is the main change.** Today
  [server.rs](../openairplay2/src/server.rs) routes `SET_PARAMETER` as
  `session.set_parameter(&request.body)` — the `Content-Type` header
  never reaches the session, and the handler blindly scans the body for
  `volume:` text lines. The handler must start receiving the content
  type (`request.headers.get("Content-Type")` exists) and branch:
  `text/parameters` → existing volume path, `application/x-dmap-tagged`
  → metadata, `image/*` → artwork, anything else → debug-log and 200 OK
  as today.
- **Large bodies already work.** `ControlConnection` reassembles a body
  to its exact `Content-Length` across encrypted HAP frames, so a
  multi-hundred-KB JPEG arrives whole; nothing to change there.
- **The DMAP walker is a new private module** (`dmap.rs`), hand-rolled
  tag-code + big-endian-u32-length walking only — no new dependencies
  (keeps the library dependency-light, which the consumer also cares
  about).
- **No public-API wire types leak:** `Event::Metadata`/`Event::Artwork`
  carry plain `String`/`Vec<u8>`, consistent with the invariant that
  the documented API stays free of AirPlay wire types.
- **Found during hardware validation: the `features` bitmask must
  advertise metadata.** A first capture against a real iPhone showed
  zero metadata/artwork `SET_PARAMETER`s — senders check the receiver's
  advertised features and our bitmask had bits 15/16/17
  (AudioMetaCovers, AudioMetaProgress, AudioMetaTxtDAAP) cleared.
  Setting them (low word `0x405C4A00` → `0x405FCA00`, which is exactly
  shairport-sync's shipped value) is part of this change; without it the
  whole feature is dead on the wire. The receiver binary now also logs
  `Metadata`/`Artwork` at info level, so hardware runs show
  `now playing: Artist — Title (Album)` lines directly.

## Testing suggestions

The cheapest path is the one milestone 7's volume already uses: the
unit tests in [session.rs](../openairplay2/src/session.rs) call
`set_parameter` directly and assert on the event channel with
`try_recv` — that covers content-type branching, the latch-until-
`SessionStarted` behavior, and replacement semantics without a socket.
The DMAP walker gets its own unit tests with hand-built payloads (the
format is simple enough to construct inline: `mlit` wrapping
`minm`/`asar`/`asal` entries). On top of that, one integration-test
fixture through the `#[doc(hidden)]` sender-side modules (`server`,
`cipher`, `srp`, `tlv`) sending a real encrypted `SET_PARAMETER` with a
DMAP body and an `image/png` body proves the header plumbing end to
end. Malformed-payload cases (truncated length, unknown tags, empty
body) should be skipped without killing the session — metadata is
decoration, never worth a teardown. And as with every milestone, a real
sender (Mac/iPhone) on hardware is part of acceptance — in particular
to confirm *when* senders actually push metadata relative to SETUP
phase 2 (the latch design assumes it can be early).

## How the consumer will use it (context, not tasks)

radiod's event task maps `Metadata` into its shared status (the same
path its ICY radio titles use) and stores the latest `Artwork` bytes to
serve at an internal endpoint; the dashboard's AirPlay view replaces
its placeholder title line and its animated art stand-in. That side is
already shaped for this and needs no library knowledge beyond the two
events. When this milestone ships (a git-dependency bump on the radio
side), the radio work is roughly a day.

## Scope

In scope: the two `Event` variants above, the `Content-Type` dispatch in
`SET_PARAMETER`, a private DMAP walker, the latch-until-`SessionStarted`
gating, and tests as described.

Out of scope (per the consumer): `progress:` forwarding; a
sender-displayed-name field on `SessionStarted`; any artwork-serving
endpoint (the consumer's job); changing the `Volume` event's (ungated)
timing.

## Phases

One implementation phase — the change is contained (one new module, two
touched files, additive API):

1. **Metadata and artwork events** — `dmap.rs` walker,
   `Content-Type` plumbing through `server.rs` → `Session::set_parameter`,
   the two `Event` variants with latch-until-`SessionStarted` gating,
   unit + integration tests.

## Acceptance criteria

- `Event::Metadata` and `Event::Artwork` delivered per the contract
  above (replacement semantics, only between `SessionStarted` and
  `SessionEnded`, `image/none`/empty forwarded as artwork-cleared).
- Existing behavior unchanged: volume path, session flow, and every
  existing test still green; no new dependencies; library still
  ALSA-free and macOS-green (`cargo test -p openairplay2`).
- Malformed metadata payloads never kill a session.
- Hardware validation against a real sender (Mac/iPhone): title, artist,
  album, and cover art observed in the event stream at session start and
  on track change, including *when* metadata arrives relative to SETUP
  phase 2.

## Status

Approved; phase 1 implemented (PR #23). Unit + integration tests green,
clippy/fmt clean. First hardware capture found the missing metadata
feature bits (see above), fixed in the same PR. Second capture
(iPhone, Music.app playlist, 2026-08-02) validated the whole contract:
title/artist/album parsed for every track across seven track changes
with zero unrecognized payloads, `image/jpeg` artwork (~180 KB) on each
change, and a real mid-session `image/none` clear. This sender pushes
metadata *after* SETUP phase 2 (~1 s after the pipeline starts), so the
early-metadata latch stayed idle — kept as armor for other senders.
