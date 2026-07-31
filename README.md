# OpenAirPlay 2

An **AirPlay 2 audio receiver** for Linux, written in Rust — the AirPlay 2
counterpart of [openairplay](https://github.com/st3fan/openairplay) (a working
AirPlay 1 / RAOP receiver).

AirPlay 2 is a substantially different protocol from AirPlay 1: HomeKit-style
pairing (SRP + Curve25519 + Ed25519), a ChaCha20-Poly1305-encrypted control
channel carrying binary plists, per-packet ChaCha20-Poly1305 audio, AAC as
well as ALAC, and PTP timing.

**Status: design phase.** See [`notes.md`](notes.md) for the protocol research
and the milestone plan. Implementation has not started yet.
