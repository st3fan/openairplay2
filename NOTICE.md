# NOTICE

openairplay2 is licensed under the MIT License (see `LICENSE`). This file
records third-party material and attributions. A fuller discussion, including
the verified upstream licenses and the FairPlay/DMCA caveats, is in
[`notes/licensing.md`](https://github.com/st3fan/openairplay2/blob/main/notes/licensing.md).
**This is not legal advice.**

## Protocol reference — shairport-sync (MIT)

The AirPlay 2 protocol handling was developed with reference to
[shairport-sync](https://github.com/mikebrady/shairport-sync), © James Laird
and Mike Brady, MIT-licensed. Specifically: the buffered-audio block framing
and the ChaCha20-Poly1305 nonce/AAD construction were translated into Rust from
its source, and the `features` capability bitmask constant was taken from it
and then modified here (metadata bits 15/16/17 set on top of shairport's
value).
shairport-sync's core is MIT; some components it bundles carry other licenses
(none of those were used here).

## FairPlay `fp-setup` tables — third-party, Apple-derived

The library's `fairplay` module (`openairplay2/src/fairplay.rs` in the
repository) contains fixed FairPlay `fp-setup` response tables (four 142-byte
tables and a 12-byte header) copied verbatim. **These are not original
to this project.** They are well-known reverse-engineered FairPlay interop
constants derived from Apple's proprietary FairPlay implementation, circulated
across many AirPlay receiver projects (shairport-sync, UxPlay, RPiPlay,
openairplay/airplay2-receiver).

Their legal status is governed by Apple's copyright and the DMCA anti-
circumvention provisions, **not** by this project's MIT license — the MIT grant
does not, and cannot, extend to this third-party Apple-derived data. They are
included solely to interoperate with Apple senders (to receive audio streamed
to your own device).

## Dependencies

Third-party Rust crates are under their own licenses (predominantly MIT /
Apache-2.0); see `Cargo.toml` and their respective repositories. One is not
permissive: `symphonia` (AAC decode) is MPL-2.0, a weak file-level copyleft —
no symphonia file is modified here, and linking it imposes nothing on this
project's own code.
