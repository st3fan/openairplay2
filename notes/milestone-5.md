# Milestone 5 — Receive, decode & play the buffered AAC stream

Goal: **sound out.** The milestone-4 hardware test showed a real macOS sender
completing the whole handshake and settling into feedback heartbeats, having
chosen **buffered AAC** (`type 103`, `audioFormat 0x400000` =
AAC-LC/44.1 kHz/F24/stereo, `spf 1024`, 32-byte `shk`). But no audio arrived
because the buffered data channel is **TCP** and we bound UDP. This milestone
receives that TCP stream, decrypts and decodes it, and plays it to ALSA.

## Scope

In:

- **Buffered data channel = TCP** (`session.rs`): for `type 103`, bind the data
  port as a **TCP listener** (not UDP), accept the sender's connection, and
  read the framed audio stream. (Realtime `type 96` would stay UDP; we target
  the format the Mac actually uses.)
- **Block framing + decrypt** (`buffered.rs`): each block is
  `[u16 len (BE, includes the 2 length bytes)][packet]`; the packet is a
  12-byte RTP-ish header (`seq&0xFFFFFF`, `timestamp`, `ssrc`) then the
  encrypted payload. Decrypt with **ChaCha20-Poly1305 (IETF)**: key = `shk`,
  nonce = 4 zero bytes ‖ the last 8 bytes of the packet, AAD = `packet[4..12]`,
  ciphertext+tag = `packet[12 .. len-8]`. Yields a raw AAC-LC frame.
- **AAC decode** (`decode.rs`): build a `symphonia` AAC decoder from a 2-byte
  **AudioSpecificConfig** (AAC-LC, the stream's sample-rate index, channel
  config) passed as `extra_data`, then feed each raw frame directly — no ADTS
  header needed — and copy to interleaved **`i16`** PCM.
- **ALSA playback** (`player.rs`, ported from the AirPlay 1 receiver): a
  dedicated thread with a fixed prebuffer, blocking writes, `S16_LE`/2ch/44100.
- **CLI**: `--alsa-device` (default `default`), `--no-audio`.

Out: PTP-accurate timing / drift (milestone 6) — we buffer and play with a
simple prebuffer for now, as the Python receiver did before it had PTP;
realtime-ALAC path; multi-channel/48 kHz beyond what the Mac sends.

## Wire details (from shairport-sync `ap2_buffered_audio_processor.c`)

- Block: `[u16 data_len BE]` then `data_len-2` bytes.
- Packet header: `[0..4]` seq (24-bit, mask `0xFFFFFF`), `[4..8]` timestamp,
  `[8..12]` SSRC (identifies the format).
- Decrypt: `chacha20poly1305_ietf`, key=`shk`, nonce = `00 00 00 00` ‖
  `packet[len-8..len]`, AAD = `packet[4..12]` (8 bytes), ciphertext (with the
  16-byte tag appended) = `packet[12..len-8]`.
- Payload = raw AAC-LC; symphonia decodes it directly given an
  AudioSpecificConfig (no ADTS framing required).

## Module layout

```
src/buffered.rs — TCP block reader + ChaCha20-Poly1305 audio decrypt (new)
src/decode.rs   — AAC-LC decode via symphonia (AudioSpecificConfig) (new)
src/player.rs   — ALSA output thread + prebuffer (ported from v1) (new)
src/session.rs  — bind buffered data as TCP; wire decrypt → decode → player
src/lib.rs/Config/main.rs — --alsa-device / --no-audio
```

## Test strategy

- **Unit**: block-framing parser (length prefix, header fields); ChaCha
  decrypt round-trip (encrypt a known AAC-sized payload with the same
  key/nonce/AAD scheme → decrypt → original); the AudioSpecificConfig bytes for
  44.1 kHz / stereo; decoding golden AAC-LC frames to PCM.
- **Integration**: feed a synthetic buffered block (encrypted with a known
  `shk`) through the decrypt path and assert the recovered payload; the ALSA
  layer isn't exercised (no hardware in tests).
- **Manual (you-run-it)**: a real sender streams; the log shows blocks
  arriving on the TCP data port, decrypting, and decoding; **audio plays**.
  (Timing may be rough until milestone 6.)

## Acceptance criteria

- `cargo test` / `cargo clippy` clean, no hardware needed.
- Framing/decrypt/ADTS/prebuffer unit tests pass.
- Hardware: buffered AAC from a real sender is received on the TCP data port,
  decrypts and decodes, and audio comes out of the ALSA device.

## Provenance / deps

New deps: `symphonia` (aac, MIT/pure-Rust) for AAC decode, `alsa` for output.
The framing/crypto scheme is from shairport-sync (MIT) — interop, same footing
as the rest of the project. See notes/licensing.md.
