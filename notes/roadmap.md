# Roadmap

What each release contains, and what the next two are for. This is the
feature-level view; the milestone-by-milestone development history is in
[`notes/status.md`](status.md), the per-change plans in `plans/`, and anything
not scheduled is a [GitHub issue](https://github.com/st3fan/openairplay2/issues).

The through-line from here: **0.4 makes the project safe to hand to a stranger,
0.5 tells that stranger it exists and how to install it.** Everything else waits.

## 0.4.0 — installable by someone who isn't us (shipped in 0.5.0)

The first release aimed at people other than its author. The bar is not new
protocol features; it is that someone who has never seen the source can install
the package, start it, and understand what happened. **Everything below landed
on `main`, but no final 0.4.0 was ever published — the 0.4.0 version number was
spent on crates.io by the `v0.4.0-rc1` release candidate (crates.io versions
are immutable), so all of it ships as part of 0.5.0.**

### The now-playing display

- **Position that follows the audio.** A sender's `progress:` line arrives at
  track start and essentially never again, so `Event::Progress` is reported once
  a second by the playback thread from the RTP timestamp it is feeding the sink.
  `SessionStarted` also carries the sender's address now. (#53)
- **A now-playing endpoint.** `--tui-listen ADDR` serves a WebSocket: a snapshot
  on connect, then one message per change, over a bounded broadcast channel so a
  slow display can never stall the audio path. (#54)
- **`openairplay2-tui-protocol`** — the serde-only wire format the receiver and
  the display share, with every message's exact JSON asserted in its own tests.
  (#54)
- **`openairplay2-tui`** — the full-screen now-playing display: title / artist /
  album, cover art as a real image via Kitty and iTerm2 terminal graphics,
  position, volume, format, sender address. Depends on neither the library nor
  ALSA, so it builds and runs on macOS; CI enforces that. (#55)
- **Cover art through tmux.** The display wraps what it emits in tmux's DCS
  passthrough envelope, probes for capability *through* that envelope, and takes
  its image down when the pane stops being visible — tmux tracks no images and
  would otherwise leave the artwork floating over the next window. It draws
  nothing at all under GNU screen rather than emitting garbage. (#61, #63, #67)

### Installable and configurable by a stranger

- **The release is publishable again.**
  [#65](https://github.com/st3fan/openairplay2/issues/65) was a hard blocker:
  `openairplay2-receiver` depended on `openairplay2-tui-protocol` by path with
  no version, so `cargo publish` failed its manifest check — and `cargo.yml`
  publishes the library *first*, so tagging 0.4.0 would have immutably published
  `openairplay2 0.4.0` to crates.io and then failed on the binary. Fixed by
  publishing the protocol crate (its JSON was already a fixture-pinned public
  format), a `cargo package --workspace` gate that verifies every publishable
  crate before any upload, and per-crate publishes that skip versions already on
  the index — so the release is idempotent and safe to re-run. The gate also
  runs in CI, so a manifest that cannot publish fails a PR, not a release.
  (#73, #74)
- **`openairplay2-tui` is packaged as a `.deb`** for amd64, arm64 and armhf —
  an interactive terminal program, so no unit or system user, `libc6` its only
  dependency, with the ARMv6 guard. A release now attaches six `.deb`s; the
  receiver `Suggests` the display. So "install the package" gets you the screen,
  not just the daemon. (#76, #77)
- **Command-line table stakes.** Both binaries gained `--version` and a real
  `--help` (stdout, exit 0, every flag with its default); the receiver gained
  `--list-devices` and a startup ALSA probe that turns a typo'd `--alsa-device`
  into a named error pointing at `--list-devices` (it used to start silently
  decode-only — music on the phone, silence in the room), plus a clearer
  address-in-use message. The hand-rolled parsers became `parse → Result`, so
  they are tested, with a help-drift guard. (#79, #80)
- **Every option configurable from `/etc/default/openairplay2-receiver`** by its
  own named `OPENAIRPLAY2_*` variable, validated with the same validators as the
  flags, a flag still winning over its variable; `%h` in the name becomes the
  hostname, so one file deploys unedited across machines. This is a security fix
  too: options used to travel on the command line, where `/proc/<pid>/cmdline`
  is world-readable and leaked a `--pincode` to `ps`; `/proc/<pid>/environ` is
  owner-and-root only. The opaque `OPENAIRPLAY2_ARGS` blob was **removed**, and
  the upgrade announces itself three ways so a preserved conffile that silently
  stops working can't surprise anyone — `NEWS.Debian`, a postinst notice in the
  apt output, and an error from the daemon that keeps running on defaults.
  (#82, #83)
- **A password on the now-playing endpoint** — new functionality, since the
  WebSocket had no authentication at all. With `OPENAIRPLAY2_TUI_PASSWORD` (or
  `--tui-password`) set, the receiver requires `Authorization: Bearer` before
  the upgrade, compared in constant time; `openairplay2-tui` presents it with
  `--password`. It is a handshake header, so the published protocol crate is
  untouched. (#84)
- **Docs corrected against the code** by an audit, the "skipped work goes in a
  labeled issue" convention, and the Linux build dependencies written down.
  (#66, #59, #57)

### Safe to run on a stranger's network

- **A security review** of the whole network surface, written up in
  [`notes/security-review-0.4.md`](security-review-0.4.md). No memory-safety
  defect or remote-code-execution path; the crypto (SRP-6a, the AEAD audio
  path) is correct and `cargo audit` is clean. Two availability/disclosure
  findings were fixed with regression tests — an unauthenticated
  connection/memory DoS ([#87](https://github.com/st3fan/openairplay2/issues/87):
  a connection cap and a handshake timeout) and the identity private key written
  world-readable ([#88](https://github.com/st3fan/openairplay2/issues/88): now
  0600) — one hardening gap (pincode brute-force lockout,
  [#89](https://github.com/st3fan/openairplay2/issues/89)) was deferred as not
  blocking, and `cargo audit` was added to CI. (#86, #90)

## 0.6.0 — telling people it exists

0.5 makes the software ready; 0.6 makes it findable and installable without a
GitHub releases page and a `dpkg` incantation.

- **End-user documentation on the project wiki**, so the README can go back to
  being a README and the operational detail lives somewhere it can grow.
- **The Debian packages published to an apt repository**, so installing is
  `apt-get install openairplay2-receiver` and upgrades arrive on their own,
  rather than downloading a file per release.
- **A website with a landing page** — what this is, what it does, and how to get
  it, for someone who arrived from a link rather than from the source tree.

## Released

### 0.5.0 — everything since 0.3.0, plus a day of listening to it (2026-08-08)

Carries the entire 0.4 block above (the display, the packaging revamp, the
security fixes, the CLI) — see that section — plus what a day of actually
using it on real hardware produced:

- **Hardware mixer volume.** `--mixer CONTROL` drives the sound card's own
  mixer control from the sender's slider instead of scaling PCM samples in
  software — full sample resolution at low volume, and a DAC or amp with its
  own volume control follows the slider. `--list-mixers` lists the controls;
  the mixer device defaults to the card of `--alsa-device`. (#107, #108)
- **The tick fix.** The ALSA stream is kept running for the whole session
  (never dropped/restarted), which removed the audible pop on every pause,
  resume and track switch. (#104, #105)
- **The on-device display just works.** The receiver serves the now-playing
  WebSocket on a local Unix socket by default (`/run/openairplay2/tui.sock`,
  file permissions as the access control) and `openairplay2-tui` with no
  arguments finds it on its own; `--tui-listen` TCP remains for remote
  displays. The display's endpoint is now a positional argument, not
  `--connect`. (#111–#113, #115)
- **The pairing pincode is now the password** — Apple's own word: iOS and
  macOS show a password dialog and accept alphanumerics. `--password` /
  `OPENAIRPLAY2_PASSWORD`; the 0.4 spellings are still honored so an upgraded
  box keeps its protection. (#116)

### 0.3.0 — Pairing and Debian Packages (2026-08-05)

- **`--pincode CODE`.** A sender must enter a code before it can pair. Getting a
  real sender to *ask* hinges on advertising AirPlay status-flag bit 7
  ("password required") in both the mDNS TXT record and `GET /info`; from there
  `pair-pin-start` is answered with an empty 200 and the code becomes the SRP
  password for M1–M4. iOS completes at M4 with no M5/M6. Validated on iOS 26 and
  written up in [`notes/protocol.md`](protocol.md).
- **`.deb` packages for amd64, arm64 and armhf.** A systemd service, enabled and
  started, running as its own `openairplay2` system user, configured through
  `/etc/default/openairplay2-receiver` (a conffile, since a pincode is a
  secret), with the pairing identity in `/var/lib/openairplay2` so upgrades
  never force a re-pair. Upgrades swap the running daemon. Each package carries
  a signed build-provenance attestation. ARMv6 boards are refused at install
  time — dpkg cannot tell Debian armhf from Raspbian armhf, and the alternative
  is an illegal instruction at runtime.
- **crates.io publishing and release automation.** Publishing a GitHub Release
  is the only event that ships anything: `release.yml` checks the tag against
  the crate version and dispatches `cargo.yml` (crates.io, trusted publishing,
  no stored token) and `debian.yml` (the architecture matrix) in parallel.
  0.3.0 was the first automated release.
- **A documentation pass** under `warn(missing_docs)`.
- **A logging pass**: `info` is startup and problems only, every per-request,
  per-packet and per-session diagnostic moved to `debug`.
- **SIGTERM handling**, which is how systemd stops a service.

### 0.2.0 (2026-08-02)

- **Split into a workspace**: `openairplay2`, the embeddable library that goes
  from network to PCM, and `openairplay2-receiver`, the standalone Linux/ALSA
  binary built on nothing but the library's public API.
- **The sink seam.** `AudioSink` / `SinkFactory` for PCM out, an `Event` stream
  for session milestones, and volume delivered in dB for the host to apply — the
  library has no audio-output dependency and builds and tests on macOS.
- **The embedding facade**: `Receiver::builder()` → `build()` →
  `run(sink_factory, events)` on the caller's runtime, plus `advertise(false)`
  with `Receiver::txt_records()` for hosts that own their own mDNS.
- **Pause holds the buffer.** Pause used to *drop* audio the sender believed was
  safely delivered, and a sticky flush boundary discarded audio it re-sent
  (~47 s measured in one session), producing delayed, mispositioned, stuttery
  resumes. Pause now parks audio and backpressure freezes the sender at the
  pause point; `FLUSHBUFFERED` discards exactly `seq < flushUntilSeq` and the
  boundary self-clears when the stream reaches it.
- **Track metadata and cover art.** `SET_PARAMETER` is dispatched on
  `Content-Type`: DMAP track metadata becomes `Event::Metadata`, cover art
  becomes `Event::Artwork`. Payloads pushed before the session exists are
  latched and replayed right after `SessionStarted`. Senders silently skip
  sending either unless the metadata bits (15/16/17) are advertised in the
  `features` mask.

### 0.1.0 — It works well enough 🎶 (2026-08-01)

Milestones 1–7: a working single-stream receiver, end to end.

- **Discovery and `/info`** — `_airplay._tcp` advertised via Avahi with the
  AirPlay 2 `features` bitmask and an Ed25519 public key, an HTTP/RTSP control
  server on port 7000, and the device plist.
- **Transient pairing and an encrypted channel** — HomeKit `pair-setup` over
  SRP-6a (3072-bit group, SHA-512, code `3939`), after which everything on the
  socket is ChaCha20-Poly1305 in HomeKit block framing with HKDF-SHA512 keys
  derived per direction.
- **The FairPlay `fp-setup` handshake** — the canned interop tables, which a
  real sender requires after pairing before it will send `SETUP`.
- **Two-phase `SETUP`** — phase 1 binds the event channel and reports
  `eventPort`; phase 2 captures `shk` and `audioFormat`, binds the data and
  control channels, and starts the pipeline. `timingPort: 0`: no PTP, by intent.
- **Buffered AAC playback** — the type 103 data channel is TCP, framed into
  packets, each decrypted with ChaCha20-Poly1305, decoded from raw AAC-LC (no
  ADTS) via `symphonia`, and played to ALSA through a prebuffered output thread.
- **Soft timing: pause, resume, seek** — transport control bypasses the ~2 s
  audio buffer, because a flush that waits behind it is a flush that arrives too
  late. The TCP reader backpressures on the queue depth, so latency and memory
  stay bounded and the sound card's drain rate sets the pace.
- **Volume** — `SET_PARAMETER volume: <dB>` converted to a linear gain
  (`10^(dB/20)`, `-144 dB` = mute) and applied live through a shared atomic, so
  slider moves take effect mid-stream.
- **Licensing and provenance** written down: LICENSE, NOTICE, and
  [`notes/licensing.md`](licensing.md), which is required reading before
  touching the FairPlay path.

## 0.7 and later

Everything here has been deliberately postponed rather than forgotten.

### Wanted, not yet scheduled

Weighed against
[shairport-sync](https://github.com/mikebrady/shairport-sync)'s feature set and
kept; the rest of that list was considered and declined, which is recorded below
so it does not get re-proposed.

- **DAC standby prevention.** Keep the device open, or feed it silence, so a DAC
  does not fall asleep between tracks and swallow the first fraction of a
  second of the next one.
- **Statistics.** Buffer occupancy, drift and underrun counts — the numbers
  needed when someone reports stuttering that cannot be reproduced — logged
  periodically, and shown in `openairplay2-tui` so they are visible while it is
  happening rather than only afterwards in a journal.

### Deliberately declined

The one that shapes everything else: **PTP**, and therefore **multi-room
grouping**. This receiver targets one sender → one stream → one output; PTP
exists to align *multiple* outputs to a shared clock, and for a single output
the sender's buffering plus our backpressure suffice. See
[`notes/milestone-6.md`](milestone-6.md).

Considered from shairport-sync and not wanted here: additional output backends
(PipeWire, PulseAudio, pipe/stdout), resampling, ALSA period and buffer tuning,
configurable idle and session timeouts, and the whole integration surface —
shell hooks, a metadata pipe or UDP feed, MQTT with Home Assistant
autodiscovery, and D-Bus/MPRIS. Also out of scope by design rather than by
scheduling: AirPlay 1 (that is
[openairplay1](https://github.com/st3fan/openairplay1)), multichannel 5.1/7.1,
and AirPlay video or photo streaming.

### Postponed

- **Fail loudly on what we do not support.** A stranger's sender may pick
  ALAC, realtime (type 96) audio, or 48 kHz / S24, and `aac_params()` hard-codes
  44.1 kHz stereo AAC-LC. Untested too: a second sender connecting mid-stream —
  the accept loop spawns a task per connection with no session arbitration — and
  a sender vanishing mid-stream. Silence with no explanation is the worst
  outcome for someone who cannot read the source.
- **Proving the packages install** — `testing/vm` boots Debian under QEMU,
  installs the `.deb` the way the README tells users to, and asserts what only a
  real install can show (PRs #69, #70, open). Not gated on 0.4, but the sooner a
  release is blocked on it, the sooner "hardware validation pending" stops being
  true of the Debian packages in [`notes/status.md`](status.md).
- **Kitty unicode placeholders** so tmux redraws cover art itself
  ([#64](https://github.com/st3fan/openairplay2/issues/64)).
- **`pair-verify` / persistent pairing**, so a sender does not pair afresh each
  session.
- **A brute-force lockout on pincode pairing**
  ([#89](https://github.com/st3fan/openairplay2/issues/89)) — the one 0.4
  security finding judged not release-blocking: SRP makes each guess an online
  round, and the connection cap already slows it, but nothing caps attempts.
