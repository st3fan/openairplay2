# OpenAirPlay2 — AirPlay 2 pairing & "pincode" protocol notes

These notes record what this project learned about **AirPlay 2 pairing** while
building its optional `--pincode` protection. They were written from
shairport-sync's `pair_ap` pairing library and the HomeKit Accessory Protocol
*Pairing* sub-spec, and the parts that matter for a real sender were **pinned
and validated on an iPhone (iOS 26)**. This is the AirPlay 2 counterpart of
openairplay1's `notes/protocol.md` (which records the AirPlay 1 Digest
password).

## The core fact: AirPlay 2 has no AirPlay 1-style password

AirPlay 1 gates streaming with RFC 2617 Digest auth over RTSP. AirPlay 2 has
**no such mechanism** — its access control is *pairing*, which authenticates
with the receiver's **setup code**. A receiver can either pair *transiently*
(no code from the user; SRP with a fixed code, `3939`) or require a
**pincode**. Making that "no code" default the open case and the pincode the
protected case is exactly what `--pincode` does.

## The confirmed pincode flow (iOS 26)

Two signals turn a pincode-protected receiver from "transient" into
"a code is required," and a sender then authenticates with the code:

1. **Advertise it.** The receiver sets AirPlay **status-flag bit 7**
   ("password required"), read from `GET /info` `statusFlags` and the `flags=`
   TXT. Without this bit, Apple senders silently pick transient `3939` and
   never prompt. (Shairport-sync sets the same bit when a password is
   configured.)
2. **`pair-pin-start`.** A sender that sees bit 7 sends
   **`POST /pair-pin-start`**; the receiver answers an **empty 200** — "the
   client calls `/pair-pin-start` and the device displays the code." For a
   headless receiver the code is the configured `--pincode`, which the user
   types on the sender (this is the **prompt**).
3. **SRP with the code.** The sender then does `pair-setup` M1→M4, using the
   entered code as the **SRP password**. The receiver verifies it: a wrong
   code fails SRP proof verification (`M3: SRP proof verification failed`), a
   right one passes.
4. **Encrypted channel at M4.** On M4 the receiver installs the encrypted
   control channel with the SRP session key `K` (the existing transient code
   path). iOS then proceeds to an encrypted `SETUP`, transport, and audio.

The iOS dialog is a **free-text "password"** entry, not a numeric PIN, so
`--pincode` accepts arbitrary strings ("hello" works).

## Caveats discovered on hardware (don't repeat these)

- **The `0x10` (transient) flag in `pair-setup` M1 is NOT the pincode-vs-
  transient discriminator.** iOS sets `flags=0x10` whether or not a pincode
  is in play; the discriminator is the advertised status bit 7 + the
  `pair-pin-start` prompt. Treating `0x10` as "no pincode" and refusing it is
  wrong.
- **iOS does NOT send M5/M6 in this flow.** It completes at M1→M4 + the
  encrypted channel and goes straight to the session. So the HomeKit M5/M6
  encrypted identity exchange (`Pair-Setup-Encrypt-*`, `PS-Msg05/06`,
  Ed25519 `controller/accessory-sign`) and `pair-verify` are **not
  exercised** here; they are a documented follow-up (below), not part of the
  pincode feature.

## Summary for the receiver

- AirPlay 2 pincode = **status bit 7 + `pair-pin-start` (empty 200)
  + SRP password = pincode**, and the existing M4→encrypted-channel path does
  the rest. No pincode configured ⇒ advertise nothing special ⇒ transient
  `3939` (drop-in).
- The pincode is **never** in the advertisement (only the boolean bit 7),
  never in `GET /info`, and never logged.

## Follow-up (separate, not part of the pincode)

`pair-verify` + durable paired-controller storage would let a *server-side*
"remembered device" stream without re-entering the code and support an
"unpair this device" surface. That needs the HomeKit M5/M6 identity exchange
and a controller store — the exact wire bytes for those would be pinned then.
iOS already remembers the code sender-side between runs, so it is not needed
for the pincode gate.
