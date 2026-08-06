# Licensing, provenance & attribution

This document records where openairplay2 took inspiration and code from, the
licenses of those sources (verified against the upstream files, not assumed),
the recommended license for this project, and the caveats — especially around
FairPlay. **This is not legal advice; if distribution or anything commercial
matters, get a real legal opinion, particularly regarding FairPlay.**

## Sources drawn on, and how

| Project | How it was used | Nature |
|---|---|---|
| **shairport-sync** (James Laird, Mike Brady) | Primary protocol reference: `rtsp.c`, `ap2_buffered_audio_processor.c`, the `_airplay._tcp` TXT-record layout, the `get_info` template, and the `features` bitmask constant (`0x00018340405C4A00`) — which this project then **modified** to `0x00018340405FCA00`, setting metadata bits 15/16/17 (see [`plans/20260802-01`](../plans/20260802-01-metadata-artwork.md)). The buffered block framing and the ChaCha20-Poly1305 nonce/AAD construction were **translated into Rust** — protocol logic, not verbatim C. | Reference + translated logic + one magic constant, modified |
| **openairplay/airplay2-receiver** (Python) | Cited as a co-source of the FairPlay `fp-setup` tables and referenced conceptually for buffered-audio behaviour. Its FairPlay *decryption* code was **not** used or translated. | Reference only |
| **FairPlay `fp-setup` tables** (`openairplay2/src/fairplay.rs`: `REPLY0..3`, 4×142 bytes, + the 12-byte phase-2 header) | **Copied verbatim** as binary constants. | Third-party, Apple-derived data (see caveat) |
| **openairplay1** (this author) | `openairplay2-tui` is a port of its now-playing dashboard — the screen, the layout maths and the terminal-graphics approach ([plan](../plans/20260805-05-tui.md)). Same author, MIT. | Own prior work |
| Rust crates: `chacha20poly1305`, `hkdf`, `sha2`, `ed25519-dalek`, `num-bigint`, `alsa`, `plist`, `zbus`, `tokio`, `ratatui`, `crossterm`, … | Normal dependencies (crypto, ALSA, mDNS/D-Bus, async, terminal UI). | Dependencies, permissive licenses (MIT / Apache-2.0 / BSD / Zlib) |
| **`symphonia`** (AAC decode) | Normal dependency, but the only non-permissive license in the tree. | **MPL-2.0** — weak, file-level copyleft: modifications to symphonia's *own* files would have to be published. We make none, and merely linking it imposes nothing on this project's code. |

Everything else is original Rust written for this project, informed by the
protocol but not copied: the server/session structure, the pairing/SRP/cipher
wiring, the pause-hold + sequence-boundary flush transport model, the queue-depth
backpressure, and the software-volume path.

## Verified upstream licenses

- **shairport-sync — MIT.** Its source headers (e.g. `rtsp.c`) carry the MIT
  text, © James Laird 2013 and Mike Brady 2014-2026. Its top-level `COPYING`
  says "refer to the individual source files for licenses" because it bundles
  some third-party components under other licenses, but the core code referenced
  here is MIT.
  - **Correction:** earlier drafts of `notes/milestone-3.md` and
    `notes/milestone-5.md` described shairport-sync as "GPL". That is wrong; it
    is MIT. Those notes should be (or have been) corrected.
- **openairplay/airplay2-receiver — FairPlay file is GPLv2.** `ap2/fairplay3.py`
  is headed "GPLv2", by systemcrash (2022), derived from the "OmgHax" FairPlay
  reverse-engineering (credit to Foxsen / the original C author). This project
  did **not** translate that decryption code — openairplay2 only does the canned
  `fp-setup` handshake, not FairPlay key decryption.
- **The `fp-setup` tables themselves** are reverse-engineered **Apple** FairPlay
  constants that circulate across many receivers (shairport-sync, UxPlay,
  RPiPlay, airplay2-receiver) under differing licenses.

## The FairPlay caveat (matters more than MIT-vs-GPL)

The `fp-setup` tables in `openairplay2/src/fairplay.rs` are **Apple-derived, reverse-
engineered material**. Their real legal exposure is Apple's copyright and the
DMCA §1201 anti-circumvention provisions — **not** which open-source license
this project picks. A license only governs *our* code; it cannot grant rights to
Apple's data.

Every hobby AirPlay receiver ships something equivalent, and Apple has not
pursued them, but it is a genuine gray area. Using openairplay2 to receive audio
streamed to your own device is the intended interop use. The tables are kept
isolated in one file and attributed; treat them as external data, not as
original project code.

## What this means for openairplay2's own code

- The protocol *facts* (which bytes are the nonce, the TXT keys, the SETUP flow)
  are generally not copyrightable, and where expressive logic was translated,
  the source (shairport-sync) is MIT — so it is MIT-compatible.
- The one thing to keep clean for the GPL question: ensure the `fp-setup` byte
  tables are treated as the well-known shairport-sync (MIT) / Apple-derived
  interop constants, not as a translation of the GPLv2 `fairplay3.py`. They are
  the same widely-circulated constants; `fairplay.rs` does the canned handshake,
  not the GPLv2 decryption. Attributing them as third-party Apple-derived data
  avoids any GPL-contamination reading.

## Recommended license: MIT

MIT for this project's own code is the natural, defensible choice: it matches
shairport-sync (the primary reference) and the Rust crates, and `Cargo.toml`
already declares `license = "MIT"`.

To make that real and honest, the repo carries — as of the packaging work —
both of the things this section used to ask for:

1. A top-level [**`LICENSE`**](../LICENSE) with the MIT text (© 2026 Stefan
   Arentz). The `Cargo.toml` field alone is not the license grant; the file is.
2. A [**`NOTICE.md`**](../NOTICE.md) that:
   - attributes protocol reference to shairport-sync (MIT, Laird/Brady);
   - quarantines the FairPlay `fp-setup` tables as third-party, reverse-
     engineered, Apple-derived interop data — not original to this project,
     included solely for interoperability, with the caveat above.

All four workspace crates declare `license = "MIT"`.

## Copyright status of AI-assisted code

openairplay2 was written with heavy AI assistance under human direction.

- Under current US Copyright Office guidance, output with *no* human authorship
  is generally not copyrightable; human-authored, -selected, -arranged, or
  -modified portions do qualify. Copyrightability of purely machine-generated
  passages is thin or absent.
- This project had substantial **human authorship and direction**: the
  architecture decisions, the deliberate no-PTP scope, the milestone planning,
  the hardware testing that drove each fix, and the review. That is a real
  human-authored layer, so it is not "purely AI-generated".
- You can still license the repo. For parts that are copyrightable, MIT grants
  permission as usual; for parts that are not, the license simply has nothing to
  grant (they are effectively public domain), which is harmless — and MIT's
  warranty disclaimer still applies either way.
- If you wanted to explicitly disclaim copyright instead, CC0 or the Unlicense
  are options, but MIT is the conventional, low-friction choice and is
  recommended here.

## Summary

- **Project license:** MIT — `LICENSE` and `NOTICE.md` are both in place.
- **shairport-sync:** MIT — protocol reference and one constant (modified
  here); MIT-compatible.
- **`symphonia`:** MPL-2.0 — the one non-permissive dependency; weak file-level
  copyleft, and we modify none of its files.
- **airplay2-receiver FairPlay code:** GPLv2 — *not* used/translated here.
- **FairPlay `fp-setup` tables:** third-party Apple-derived interop data; the
  real caveat is Apple/DMCA, independent of the OSS license chosen.
- **AI-assisted authorship:** licensing is fine; substantial human direction
  gives a real authored layer, and MIT governs what is protectable.
- Not legal advice.
