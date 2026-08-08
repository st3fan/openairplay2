# Test plan — 0.4

Manual acceptance pass before tagging 0.4.0. Every test is numbered `N.M`;
report results by number. Format per test: **do the action**, then **what you
must see / hear / have**. If the "must" doesn't happen, the test fails.

## Setup (do once)

- Flash a Raspberry Pi with **Raspberry Pi OS (Raspbian) Trixie**. Use the
  64-bit image on a Pi 3/4/5 (installs the `arm64` `.deb`); the 32-bit image on
  a Pi 3/4 gives `armv7l` (installs the `armhf` `.deb`).
- Boot it, then `sudo apt update && sudo apt full-upgrade -y` and reboot.
- Give it audio out: connect a USB or HAT sound card, **or** plug headphones
  into the 3.5 mm jack on models that have one (Pi 3/4 — the Pi 5 and the Zeros
  have no jack, so those need a sound card).
- `systemctl is-active avahi-daemon` → `active` (install `avahi-daemon` if not).
- Have the 0.4.0 `.deb`s for the Pi's architecture plus `SHA256SUMS`, from the
  release page (or a local `./packaging/build-deb.sh`).
- A Mac (macOS) and an iPhone (iOS) on the same network.
- A terminal that draws inline images for the display tests: Ghostty, Kitty,
  WezTerm, iTerm2, or Konsole.
- Shorthand below: `RECV` = the Pi's address, `ARCH` = its architecture.

---

## 1. Installation (Debian package)

**1.1** `sha256sum -c SHA256SUMS 2>&1 | grep _ARCH_`
Must have: `OK` for both `openairplay2-receiver_0.4.0-1_ARCH.deb` and
`openairplay2-tui_0.4.0-1_ARCH.deb`.

**1.2** `sudo apt-get install -y ./openairplay2-receiver_0.4.0-1_ARCH.deb`
Must have: install succeeds, no dependency errors.

**1.3** `systemctl is-enabled openairplay2-receiver && systemctl is-active openairplay2-receiver`
Must have: `enabled` then `active`.

**1.4** `id openairplay2`
Must have: the user exists and is in the `audio` group.

**1.5** `sudo apt-get install -y ./openairplay2-tui_0.4.0-1_ARCH.deb`
Must have: install succeeds; pulls in `libc6` only (no ALSA/Avahi).

**1.6** `dpkg -L openairplay2-tui | grep bin`
Must have: `/usr/bin/openairplay2-tui` is present.

**1.7** `apt-get install --dry-run ./openairplay2-receiver_0.4.0-1_ARCH.deb 2>&1 | grep -i suggest`
Must have: `openairplay2-tui` listed as a suggestion.

**1.8** `gh attestation verify openairplay2-receiver_0.4.0-1_ARCH.deb --repo st3fan/openairplay2`
Must have: verification succeeds (a signed build-provenance attestation).

**1.9** (ARMv6 board only, e.g. Pi Zero W / Pi 1) `sudo apt-get install -y ./openairplay2-receiver_0.4.0-1_armhf.deb`
Must have: install is refused with a message naming ARMv7 / "not supported".

---

## 2. Service management (systemd)

**2.1** `systemctl status openairplay2-receiver`
Must have: `active (running)`, running as `User=openairplay2`.

**2.2** `journalctl -u openairplay2-receiver -b --no-pager | head`
Must have: a `starting AirPlay 2 receiver "<name>" …` line; no crash/panic.

**2.3** `sudo systemctl restart openairplay2-receiver`
Must have: service comes back `active` within a second or two.

**2.4** `sudo systemctl stop openairplay2-receiver`
Must have: stops cleanly (no timeout, no `SIGKILL` in the journal).

**2.5** Reboot the Pi. After it comes up, `systemctl is-active openairplay2-receiver`
Must have: `active` — it started at boot.

**2.6** `journalctl -u openairplay2-receiver -b --no-pager | grep -i avahi`
Must have: after the reboot, no "avahi advertisement disabled" — it advertised.

**2.7** `sudo kill -9 $(systemctl show -p MainPID --value openairplay2-receiver)`,
wait ~6 s, `systemctl is-active openairplay2-receiver`.
Must have: `active` — a crash (killed process) is restarted automatically.

---

## 3. Configuration (`/etc/default/openairplay2-receiver`)

Edit the file, `sudo systemctl restart openairplay2-receiver`, then check.

**3.1** Set `OPENAIRPLAY2_NAME=Living Room`.
Must have: journal startup line shows name `Living Room`; it appears as
"Living Room" in the Mac/iPhone AirPlay menu.

**3.2** Set `OPENAIRPLAY2_NAME=Studio %h`.
Must have: the name shown is `Studio <hostname>` (the `%h` became the hostname).

**3.3** `openairplay2-receiver --list-devices` (as any user), pick a device, set
`OPENAIRPLAY2_ALSA_DEVICE=<that device>`.
Must have: journal shows `audio output: <that device> (<friendly name>)`; audio
plays from it (see §7).

**3.4** Set `OPENAIRPLAY2_AUDIO=off`, restart.
Must have: journal shows `audio output: disabled`; a stream is accepted but silent.
Then set it back to `on`.

**3.5** Set `OPENAIRPLAY2_AVAHI=off`, restart.
Must have: the receiver no longer appears in the AirPlay menu. Set it back to `on`.

**3.6** Set `OPENAIRPLAY2_AVAHI=maybe`, restart, then `systemctl is-active
openairplay2-receiver` and re-check it a few seconds later.
Must have: the service is `failed` and **stays** failed — it does not
restart-loop; journal names `OPENAIRPLAY2_AVAHI` and the bad value once, not
repeatedly. Fix it back to `on` and restart.

**3.7** Set `OPENAIRPLAY2_PORT=7010`, restart, `ss -ltnp | grep 7010`.
Must have: the control server is listening on 7010. Set it back / remove it.

**3.8** Leave a line empty, e.g. `OPENAIRPLAY2_NAME=`, restart.
Must have: the default name `OpenAirPlay2 (<hostname>)` is used — empty means
unset (fall back to the default), not a blank name.

---

## 4. Command-line interface

Run against the installed binary (`openairplay2-receiver`, `openairplay2-tui`).

**4.1** `openairplay2-receiver --version`
Must have: `openairplay2-receiver 0.4.0`, exit code 0.

**4.2** `openairplay2-tui --version`
Must have: `openairplay2-tui 0.4.0`, exit code 0.

**4.3** `openairplay2-receiver --help`
Must have: multi-line help to stdout describing every flag; exit code 0.

**4.4** `openairplay2-receiver --help | head` (piped)
Must have: no panic / "failed printing to stdout"; clean output.

**4.5** `openairplay2-receiver --list-devices`
Must have: a short list — `default` plus one friendly entry per sound card
(e.g. `plughw:CARD=Headphones` / "bcm2835 Headphones"), not the full ALSA dump.

**4.5a** `openairplay2-receiver --list-all-devices`
Must have: the full ALSA playback list (many more entries, plugins included).

**4.6** `openairplay2-receiver --no-avahi --alsa-device nonsense`
Must have: exits non-zero with `error: ALSA device "nonsense" does not exist …`
pointing at `--list-devices`.

**4.7** With the service running, `openairplay2-receiver --no-audio` (default port 7000)
Must have: `error: cannot bind control port 7000: Address already in use — is another receiver already running?`

**4.8** `openairplay2-receiver --frobnicate`
Must have: `error: unknown argument: --frobnicate` + a usage line; exit code 2.

**4.9** `openairplay2-receiver --port notanumber`
Must have: an error naming `--port` and the bad value; exit code 2.

---

## 5. Security

**5.1** Set `OPENAIRPLAY2_PINCODE=4821`, restart. Find the PID
(`systemctl show -p MainPID --value openairplay2-receiver`), then
`sudo tr '\0' ' ' < /proc/<pid>/cmdline; echo`.
Must have: the command line does **not** contain `4821` (the pincode rides in
the environment, not `ps`).

**5.2** `sudo ls -l /var/lib/openairplay2/identity`
Must have: mode `-rw-------` (0600), owned by `openairplay2` — the private key
is owner-only.

**5.3** Set `OPENAIRPLAY2_TUI_LISTEN=0.0.0.0:7392` and `OPENAIRPLAY2_TUI_PASSWORD=s3cret`, restart.
From another machine: `openairplay2-tui --connect ws://RECV:7392` (no password).
Must have: the display shows "the receiver wants a password — start with --password"; no metadata leaks.

**5.4** `openairplay2-tui --connect ws://RECV:7392 --password s3cret`
Must have: the display connects and shows the now-playing snapshot.

**5.5** `openairplay2-tui --connect ws://RECV:7392 --password wrong`
Must have: refused, same "wants a password" state — a wrong password is not accepted.

**5.6** (DoS cap, optional) Open 40 idle TCP connections to port 7000
(`for i in $(seq 40); do (exec 3<>/dev/tcp/RECV/7000; sleep 60) & done`), then
pair a real Mac.
Must have: the receiver stays responsive and the Mac can still connect/stream —
excess connections are capped, not fatal.

---

## 6. Discovery & pairing (iPhone / Mac)

Do each on **both** the Mac and the iPhone unless noted.

**6.1** Open the AirPlay / audio-output picker.
Must have: the receiver appears under its configured name.

**6.2** (No pincode configured) Select it and start playing.
Must have: it pairs silently and audio starts — no code prompt.

**6.3** Set `OPENAIRPLAY2_PINCODE=1234`, restart. Select the receiver on the iPhone.
Must have: iOS prompts for a code.

**6.4** Enter the **wrong** code first.
Must have: pairing is refused (and you are still in the prompt state, so 6.5 can proceed).

**6.5** Now enter `1234`.
Must have: pairing succeeds and audio starts.

**6.6** After a successful pair, lock/unlock the phone and select the receiver again.
Must have: it connects without asking to pair again (stable identity).

---

## 7. Playback (you must hear it)

**7.1** Play a track from Apple Music / a local file on the Mac to the receiver.
Must hear: clean audio from the ALSA device, no stutter, correct pitch/speed.

**7.2** Same from the iPhone.
Must hear: clean audio.

**7.3** Let a track play to its natural end into the next track.
Must hear: continuous playback across the track boundary, no gap-crash.

**7.4** Play for ~2 minutes.
Must hear: no drift, no periodic glitch; `journalctl -u openairplay2-receiver -f`
shows no warnings during steady play.

---

## 8. Transport control

**8.1** Press pause on the sender.
Must hear: audio stops promptly.

**8.2** Press play again.
Must hear: it resumes from where it paused — nothing skipped, nothing repeated.

**8.3** Pause, wait 30 seconds, resume.
Must hear: resumes cleanly from the pause point (the buffer was held, not dropped).

**8.4** Skip to the next track.
Must hear: the new track starts promptly; no leftover audio from the old one.

**8.5** Scrub/seek within a track.
Must hear: playback jumps to the new position without a long delay.

**8.6** Move the sender's volume slider up and down.
Must hear: playback volume follows the slider, live, including near-silence at the bottom.

---

## 9. Upgrade & persistence

**9.1** With 0.4 installed, paired, and streaming (§6–§7), note
`ls -l /var/lib/openairplay2/identity`. Reinstall the same `.deb`
(`sudo apt-get install --reinstall -y ./…receiver…deb`).
Must have: the identity file is unchanged; the Mac/iPhone still sees the
receiver and streams **without re-pairing**.

**9.2** (Simulated legacy upgrade) Add `OPENAIRPLAY2_ARGS="--name Old"` to
`/etc/default/openairplay2-receiver`, then reinstall the `.deb`.
Must have: the apt output prints a migration notice mentioning
`OPENAIRPLAY2_ARGS` and NEWS.Debian.

**9.3** With that `OPENAIRPLAY2_ARGS` still set, `sudo systemctl restart openairplay2-receiver`
then `journalctl -u openairplay2-receiver -b | grep OPENAIRPLAY2_ARGS`.
Must have: an ERROR line saying it is set but no longer read — and the service
is still `active`. Remove the line afterwards.

**9.4** `zcat /usr/share/doc/openairplay2-receiver/NEWS.Debian* 2>/dev/null || cat /usr/share/doc/openairplay2-receiver/NEWS.Debian`
Must have: the 0.4.0 entry describing the `OPENAIRPLAY2_*` variables.

**9.5** `sudo apt-get remove openairplay2-receiver` then `ls /var/lib/openairplay2`
Must have: the package is gone but the identity file is kept (no forced re-pair
on reinstall). Reinstall to continue.

---

## 10. Now-playing display (`openairplay2-tui`)

Start the receiver with the endpoint on (`OPENAIRPLAY2_TUI_LISTEN=127.0.0.1:7392`
or run it on the Pi), and `openairplay2-tui --connect ws://RECV:7392` in an
image-capable terminal. Play a track with cover art.

**10.1** While a track plays.
Must see: title, artist, album centered; the sender's address; the stream
format; the volume.

**10.2** Cover art.
Must see: the real album image drawn in the terminal (not text/placeholder).

**10.3** Watch the position line for ~10 s.
Must see: the elapsed time advances about once a second.

**10.4** Pause on the sender.
Must see: the display indicates paused and the position clock stops advancing.

**10.5** Resume.
Must see: the clock advances again.

**10.6** Change track.
Must see: title/artist/album and cover art update to the new track.

**10.7** Stop the receiver while the display is open.
Must see: the display shows a connection-lost / retrying state, not a crash or a frozen screen.

**10.8** Restart the receiver.
Must see: the display reconnects on its own and shows the current state.

**10.9** Run the display over SSH from the Mac (`ssh RECV`, then `openairplay2-tui`).
Must see: it renders; cover art draws if the local terminal supports it.

---

## 11. Build from source / crates.io (optional)

**11.1** `cargo install openairplay2-receiver` on a clean box with the build deps.
Must have: it builds and installs from crates.io.

**11.2** `cargo build --release -p openairplay2-tui` on macOS.
Must have: it compiles (the display's macOS portability holds).

**11.3** `RUST_LOG=debug openairplay2-receiver --name Test` and pair once.
Must have: per-request protocol logging appears (the wire trace), and the
pincode is never printed even when set.

---

## Sign-off

- All §1–§10 tests pass on a real Raspberry Pi (Raspbian Trixie), one Mac, and
  one iPhone.
- No panic or crash in the journal across the whole pass.
- Any failure is filed as a `bug` issue with its test number before tagging.
