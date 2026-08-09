# Restructure request handling into handlers and commands

A structural refactor of the library's control-protocol dispatch, modeled on
[tqs](https://github.com/st3fan/tqs) and [tdb](https://github.com/st3fan/tdb):
every HTTP/RTSP verb gets a **handler** (the web layer: parse the wire, build
params, shape the response) that calls a **command** (the business logic: a
`Params` struct validated with `validator::Validate`, applied to session
state), with one `CommandError` type propagating from commands back to the web
layer, where a single mapping turns it into a status code. No behavior change —
the wire protocol a real Mac sees stays byte-identical.

## Background

### What tqs/tdb do that this codebase doesn't

Both reference projects are axum services with the same three-layer shape:

- **`handlers/<verb>.rs`** — one file per endpoint. Extracts the request
  (path/JSON), builds the command's `Params` struct verbatim (no validation,
  no policy), calls the command, wraps the result in a response type. The
  handler is boring on purpose; you can read the whole web layer in a glance.
- **`commands/<verb>.rs`** — one file per operation. Owns a
  `#[derive(Validate)] pub struct <Verb>Params`, and a
  `pub async fn <verb>(state, params) -> Result<T, CommandError>` whose first
  line is `params.validate()?`. All business logic and all validation live
  here, unit-tested directly against the params — no web layer in the loop
  (tdb's `commands/create_domain.rs` has seven such tests against an in-memory
  db via `commands/test_helpers.rs`).
- **`errors.rs`** — one `CommandError` enum (`thiserror`), with
  `From<validator::ValidationErrors>` so `?` works, and **one** `IntoResponse`
  impl mapping every variant to a status code and body. No handler ever picks
  a status code ad hoc.
- **`types.rs`** — validated newtypes (`QueueName`, `DomainName`) that
  implement `Validate` themselves and are pulled into params via
  `#[validate(nested)]`, plus `new_unchecked` constructors so tests can build
  invalid values to prove validation catches them.
- **`mod.rs` re-exports** so call sites read
  `commands::{send_message, SendMessageParams}`.

### What this codebase does today

[server.rs](../openairplay2/src/server.rs) holds two dispatch functions —
`dispatch_session` (a match over RTSP methods calling `Session` methods) and
`dispatch` (a match over HTTP method+target) — plus inline special cases for
`/pair-setup` and `/pair-pin-start` in the connection loop.
[session.rs](../openairplay2/src/session.rs) is a ~600-line module (plus ~570
lines of tests) where wire parsing, validation, session semantics, and the
audio-pipeline plumbing interleave:

- `handle_setup` walks the SETUP plist inline; errors are `io::Error`s that
  `dispatch_session` turns into a 400.
- `set_parameter` dispatches on `Content-Type`, then `set_text_parameters`
  parses `volume:`/`progress:` lines, sanitizes, and applies — parse,
  validate, and apply in one function.
- `parse_rate_anchor` / `parse_flush_until_seq` are free functions next to
  the state they feed.
- Status codes are chosen in four different places (`dispatch_session`,
  `dispatch`, and the two inline blocks in `handle_connection`).

Nothing is *wrong* — the tests are strong and the behavior is
hardware-verified — but every new verb smears itself across `server.rs` and
`session.rs`, and validation policy (what is refused, what is clamped, what is
silently tolerated) is discoverable only by reading each parser.

### What maps, and what deliberately doesn't

This is not an axum service and won't become one: the control connection is a
hybrid HTTP/RTSP protocol on one socket with a mid-stream cipher install, so
[crypto_stream.rs](../openairplay2/src/crypto_stream.rs),
[http.rs](../openairplay2/src/http.rs) and the connection loop in
[server.rs](../openairplay2/src/server.rs) stay exactly what they are — they
are the "framework" layer axum plays in tqs. What maps is everything above
that:

| tqs/tdb | here |
|---|---|
| axum `Router` in `main.rs` | a dispatch table in `handlers/mod.rs` |
| axum extractors (`Path`, `Json`) | plist / `text/parameters` / TLV parsing in the handler |
| `handlers/<verb>.rs` | same, taking `&Request` (+ session/context) |
| `commands/<verb>.rs` with `Params` + `Validate` | same, taking `&mut Session` |
| `CommandError` + `IntoResponse` | `CommandError` + one `fn response(&self, protocol) -> Response` |
| `types.rs` validated newtypes | same (`VolumeDb` first) |
| sqlx pool as command state | `Session` as command state |

One AirPlay-specific twist has no tqs equivalent and must be stated up front:
**for several verbs, the correct response to garbage is 200 OK.** Metadata is
decoration (an unparseable DMAP blob is dropped with a debug log, never an
error to the sender); an unusable `volume:` leaves the knob where it was; a
malformed `progress:` is ignored — all hardware-verified behavior, some of it
regression-tested against the "one bad line poisons the session" failure (a
malformed `GET_PARAMETER` answer makes a real sender abort before SETUP
phase 2). So errors always *propagate* to the handler, but the handler owns
the **tolerance policy**: some map errors through the shared status-code
mapping (SETUP → 400), others deliberately answer 200 and log
(SET_PARAMETER). Each handler states its policy in one visible place instead
of it being implicit in a parser returning `Option`.

## Scope

**In scope** — all of it inside the `openairplay2` library crate, all new
modules private (the documented public API does not change):

1. **`errors.rs`** — `CommandError` (`thiserror`), with at least:
   `Validation(validator::ValidationErrors)` (via `#[from]`),
   `MalformedBody(&'static str, String)` (what failed to parse, and why),
   `MissingField(&'static str)`, `UnsupportedStream`, `Io(io::Error)`. One
   `response(&self, protocol: &str) -> Response` mapping every variant to a
   status — the only place in the crate that chooses an error status code.
   (`protocol` is a parameter because a response must echo the request's own
   `HTTP/1.1` vs `RTSP/1.0` token — the reason this can't be a plain
   `IntoResponse` clone.)
2. **`types.rs`** — validated newtypes, starting with `VolumeDb`: carries the
   `[-144.0, 0.0]` invariant from plan `20260809-02`. Construction refuses
   non-finite and clamps finite values (normalization, exactly today's
   `sanitize_volume`); its `Validate` impl asserts the invariant so params
   embedding it via `#[validate(nested)]` re-prove it. A
   `new_unchecked` constructor for tests, per tdb. Other fields stay plain
   (`u64` rate, `u32` RTP timestamps) with `#[validate(range(...))]`
   attributes where a range exists — newtypes only where an invariant is
   worth a name.
3. **`commands/`** — one file per operation, each with a
   `#[derive(Validate)] <Verb>Params` struct and a function
   `(&mut Session, params) -> Result<T, CommandError>` whose first line is
   `params.validate()?`:
   - `setup_timing.rs` (`SetupTimingParams`) and `setup_streams.rs`
     (`SetupStreamsParams { stream_type, audio_format, shared_key, ... }`) —
     the two SETUP phases, today's `setup_timing`/`setup_streams`.
   - `set_rate_anchor.rs` (`SetRateAnchorParams { rate, rtp_time }`).
   - `flush_buffered.rs` (`FlushBufferedParams { until_seq: Option<u64> }`).
   - `set_volume.rs` (`SetVolumeParams { db: VolumeDb }`).
   - `set_progress.rs` (`SetProgressParams { start, current, end }`).
   - `set_metadata.rs` / `set_artwork.rs` (parsed DMAP fields / content type
     + bytes).
   - `get_volume.rs` (answers the `GET_PARAMETER volume` query).
   - `teardown.rs`.
4. **`handlers/`** — one file per verb/endpoint plus the dispatch table:
   - `mod.rs` — `async fn dispatch(&Request, &mut Session, &Context) ->
     Response`, the router analog: a match on method (RTSP) and
     method+target (HTTP) that replaces `dispatch_session` + `dispatch` in
     `server.rs`. Unknown verbs 501 here, `finalize` (CSeq/Server echo) stays
     in `server.rs` — it applies to every response including pairing.
   - `setup.rs` — parses the SETUP plist, picks phase 1/2 by the presence of
     `streams`, builds the params, maps command errors → 400.
   - `set_parameter.rs` — the `Content-Type` dispatch and the
     `text/parameters` line splitter; builds volume/progress/metadata/artwork
     params; **tolerance policy: always 200**, command errors are logged.
   - `get_parameter.rs` — parses the query, answers `text/parameters`;
     unknown parameters answer 200 with an empty body (today's behavior).
   - `set_rate_anchor.rs`, `flush_buffered.rs` — parse the plist, build
     params; unparseable bodies keep today's warn-and-200.
   - `teardown.rs`, `record.rs` (RECORD/SETPEERS/SETPEERSX acks).
   - `info.rs`, `fp_setup.rs`, `feedback.rs` (also `/command`,
     `/audioMode`), `pair_pin_start.rs` — the stateless endpoints.
   - `pair_setup.rs` — builds the TLV response via `PairSetup::handle` and
     returns `(Response, Option<shared secret>)`; the connection loop keeps
     the cipher install (a connection-level side effect, not a web-layer
     one). Likewise the SETUP takeover claim stays in the loop — it needs
     the connection id and eviction handle.
5. **`session.rs` shrinks to state + pipeline** — the `Session` struct
   (fields as today), the audio pipeline (`start_buffered_audio`,
   `buffered_audio`, `skip_before_boundary`, the channel tasks), events, and
   `Drop`. Its RTSP-facing methods dissolve into the commands; the free
   parsers (`parse_rate_anchor`, `parse_flush_until_seq`, the plist walking
   in `handle_setup`) move to their handlers.
6. **`validator` dependency** (`features = ["derive"]`) in the library crate
   — pure Rust, keeps the macOS build green and `cargo tree -p openairplay2`
   free of audio deps. tqs/tdb pin 0.18; we take current 0.20 (same API for
   this usage).
7. **CLAUDE.md** — the Architecture section's request-flow paragraph updated
   to name the handler/command layers.

**Out of scope**

- **Any wire-visible behavior change.** No new rejections, no changed status
  codes, no changed response bodies (`volume: %.6f` echo included). Stricter
  validation that would refuse input today's code tolerates is explicitly
  *not* a goal of this refactor; if the new structure surfaces a value that
  *should* be refused, that's an issue to file, not a change to sneak in.
- axum, hyper, or any HTTP framework. The hybrid protocol and mid-stream
  cipher install rule them out; the point is the layering, not the framework.
- `openairplay2-receiver`, `openairplay2-tui`, the protocol crate — untouched.
- The public API. `Receiver`/`AudioSink`/`Event` etc. are unchanged;
  `commands`/`handlers`/`errors`/`types` are private modules (not even
  `#[doc(hidden)]` — nothing outside the crate needs them).
- The pairing state machine ([pairing.rs](../openairplay2/src/pairing.rs)),
  crypto, decode, player, takeover — internals untouched; only their call
  sites move.
- tqs's `constants.rs` and `utils.rs` conventions — adopted only if a phase
  actually accumulates shared constants/loaders; not created empty.

### Where validation lives (the "one place" rule)

Three distinct jobs, each in exactly one layer:

- **Parsing** (wire bytes → typed fields) — handlers. A parse failure is a
  `CommandError::MalformedBody`/`MissingField`, and the handler's tolerance
  policy decides whether the sender sees it.
- **Normalization** (clamping a finite volume into `[-144, 0]`) — the
  `types.rs` newtype constructor, because it's part of *reading* the value,
  and hardware-verified behavior clamps rather than refuses.
- **Validation** (invariants on the assembled params) — the command, via
  `params.validate()?`, always its first line. Handlers never validate;
  commands never parse.

## Test strategy

- **Existing tests move with the code, none are weakened.** The ~25 session
  tests split naturally: parse-edge tests (`parses_real_setrateanchortime`,
  `parses_real_flushbuffered`, malformed progress/metadata, the non-finite
  volume table) become handler tests — same captured real-Mac bodies, now
  asserting the built params or the tolerance policy; behavior tests
  (pause gating, latched metadata, flush boundary, event ordering) become
  command tests driving `Session` state through `Params` structs, tdb-style.
- **New tests where the structure creates seams**: `CommandError::response`
  status mapping (one table test); `VolumeDb` construction + `Validate`
  (including `new_unchecked` proving validation catches what construction
  would have refused); per-command `params.validate()` rejections.
- **The integration tests are the behavior lock.** `openairplay2/tests/`
  drives the real server over TCP through pairing and encrypted requests;
  they must pass **unchanged** in every phase — any edit to them is a smell
  that wire behavior moved.
- Per phase: `cargo build --release && cargo test && cargo clippy
  --all-targets && cargo fmt --check`, and `cargo test -p openairplay2` on
  macOS via CI.
- **Hardware check in the final phase**: a full real-Mac session against
  skynet — pair, play, pause/resume, seek (FLUSHBUFFERED), volume from the
  sender (including an out-of-range value if scriptable), metadata + artwork
  on the display, `GET_PARAMETER volume` echo intact, teardown. Anything
  touching this much of the request path re-earns its hardware pass.

## Acceptance criteria

- Every HTTP/RTSP endpoint is reached through `handlers/`; every
  state-touching verb has a `<Verb>Params` + command pair; every command's
  first line is `params.validate()?`.
- `server.rs` contains no per-verb logic beyond the two connection-level
  special cases (pair-setup cipher install, SETUP takeover claim) and
  `finalize`.
- Exactly one place in the crate maps `CommandError` → status code; handlers
  that tolerate errors do so explicitly and visibly.
- Wire behavior is unchanged: integration tests pass unmodified; the
  hardware session above behaves as today.
- The library builds and tests green on macOS; `cargo tree -p openairplay2`
  gains only `validator` and its pure-Rust deps.
- CLAUDE.md's request-flow description matches the new layout.

## Phases

Each phase is one PR stacked on the previous, buildable and green on its own;
the wire behavior is identical after every phase.

1. **`commands-foundations`** — `errors.rs`, `types.rs` (`VolumeDb`),
   the `validator` dependency, and `handlers/` with the dispatch table.
   The stateless endpoints move first (`info.rs`, `fp_setup.rs`,
   `feedback.rs`, `pair_pin_start.rs`); `dispatch` disappears from
   `server.rs`; `dispatch_session` survives temporarily behind the new
   `handlers::dispatch`.
2. **`commands-parameters`** — the SET_PARAMETER/GET_PARAMETER family:
   `set_volume`, `set_progress`, `set_metadata`, `set_artwork`,
   `get_volume` commands; `set_parameter.rs`/`get_parameter.rs` handlers
   with the content-type dispatch and the stated always-200 policy;
   `VolumeDb` replaces `sanitize_volume`; the volume/progress/metadata
   session tests migrate.
3. **`commands-transport`** — `set_rate_anchor`, `flush_buffered`,
   `teardown`, and the RECORD/SETPEERS acks; the plist parsers move into
   the handlers; `dispatch_session` is deleted.
4. **`commands-setup`** — the big one last, alone in its PR:
   `setup_timing`/`setup_streams` commands and the `setup.rs` handler;
   `pair_setup.rs`; `session.rs` reduced to state + pipeline; CLAUDE.md
   updated; the full hardware check.
