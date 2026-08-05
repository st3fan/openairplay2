# Debian packages: `openairplay2-receiver` .deb for amd64 and arm64

- **Date:** 2026-08-05
- **Status:** proposed
- **Scope:** the standalone receiver binary only — packaging, a systemd unit, and
  the release workflow that builds and attaches the `.deb`s. The library crate is
  not packaged (it ships on crates.io).

## Background

The only ways to install the receiver today are `cargo build --release` or
`cargo install openairplay2-receiver` — both require a Rust toolchain on the
target machine, and neither gives you a service that survives a reboot. The
natural home for this receiver is a small always-on Linux box (a Pi or similar
arm64 board, or an amd64 mini-PC) plugged into an amplifier, where the right
shape is `apt install ./openairplay2-receiver_X.Y.Z-1_arm64.deb` and a systemd
unit that starts at boot.

The sibling [radio](https://github.com/st3fan/radio) project already does
exactly this for `radiod` (`cargo-deb` + `[package.metadata.deb]`, a
`deploy/debian/` maintainer-script directory, a systemd unit, and a
release-triggered workflow that builds one `.deb` per architecture, attests
provenance, and attaches them plus `SHA256SUMS` to the GitHub Release). This
plan applies that proven arrangement here, minus the parts radio needs and we
don't: radio cross-compiles for armhf and links ffmpeg; we target only amd64
and arm64, both of which build **natively** on free GitHub runners, and we link
nothing but ALSA and libc.

## Goals

- `openairplay2-receiver_X.Y.Z-1_amd64.deb` and `…_arm64.deb`, built from the
  release tag, attached to the GitHub Release with `SHA256SUMS` and a signed
  build-provenance attestation.
- Installing one gives a running, enabled service: `systemctl status
  openairplay2-receiver`, output to ALSA, discoverable by a real Mac after a
  reboot, with configuration in `/etc/default/openairplay2-receiver`.
- The AirPlay identity survives upgrades and reboots (senders remember the
  receiver by it) — a packaging-visible instance of an existing invariant.
- Building a `.deb` locally is one command (`packaging/build-deb.sh`), the same
  script CI runs.
- No change to how the crates are published: crates.io publishing stays in the
  same workflow file (its Trusted Publishing config is keyed on the filename).

## Design

### Layout

```
packaging/
  openairplay2-receiver.service   systemd unit (cargo-deb systemd-units)
  openairplay2-receiver.default   /etc/default/… — the CLI flags, a conffile
  debian/postinst                 create the system user, add it to audio
  debian/postrm, prerm            (mostly #DEBHELPER# placeholders)
  build-deb.sh                    native build wrapper around `cargo deb`
```

`[package.metadata.deb]` lives in `openairplay2-receiver/Cargo.toml` (that is
the packaged crate), with asset paths reaching up into `../packaging/` — the
same relative arrangement radio uses. The metadata is inert for `cargo publish`;
the published crate simply does not carry the packaging directory, which is
fine because nobody builds a `.deb` from a registry tarball.

Sketch:

```toml
[package.metadata.deb]
name = "openairplay2-receiver"
maintainer = "Stefan Arentz <stefan.arentz@gmail.com>"
copyright = "2026 Stefan Arentz"
section = "sound"
priority = "optional"
# Pinned to trixie's names rather than cargo-deb's shlibdeps guess: the same
# short list is correct on both architectures and reproducible in CI.
depends = "libasound2t64, libc6"
# The receiver runs without Avahi (it warns and is simply undiscoverable, and
# --no-avahi is a supported mode for hosts that own their own mDNS), so this
# is Recommends, not Depends — apt installs it by default anyway.
recommends = "avahi-daemon"
assets = [
    ["target/release/openairplay2-receiver", "usr/bin/", "755"],
    ["../packaging/openairplay2-receiver.default", "etc/default/openairplay2-receiver", "640"],
]
conf-files = ["/etc/default/openairplay2-receiver"]
maintainer-scripts = "../packaging/debian"

[package.metadata.deb.systemd-units]
unit-scripts = "../packaging"
enable = true
start = true
restart-after-upgrade = true
```

### The service

```ini
[Unit]
Description=OpenAirPlay 2 audio receiver
Documentation=https://github.com/st3fan/openairplay2
After=network-online.target avahi-daemon.service
Wants=network-online.target avahi-daemon.service

[Service]
EnvironmentFile=-/etc/default/openairplay2-receiver
ExecStart=/usr/bin/openairplay2-receiver --identity-file /var/lib/openairplay2/identity $OPENAIRPLAY2_ARGS
User=openairplay2
Group=openairplay2
SupplementaryGroups=audio
StateDirectory=openairplay2
Restart=always
RestartSec=5
NoNewPrivileges=true
ProtectSystem=full
ProtectHome=true
PrivateTmp=true
```

Four things here are deliberate, not boilerplate:

- **`--identity-file` is in `ExecStart`, not in the env file.** The binary's
  default identity path is `$HOME/.config/openairplay2/identity`, and a systemd
  system unit has no `HOME` — the fallback would drop `openairplay2.identity`
  into the working directory. `StateDirectory=openairplay2` gives us
  `/var/lib/openairplay2`, owned by the service user and preserved across
  upgrades and purges-that-aren't; the identity must be stable across restarts
  (senders remember the receiver by its `pk`/`pi`).
- **No `PrivateDevices=`/`DeviceAllow=`** — the sink opens `/dev/snd/*`. The
  supplementary `audio` group is what grants that access; `postinst` creates
  the `openairplay2` system user and adds it to `audio`.
- **`EnvironmentFile` mode `0640`, root-owned.** `--pincode` is a secret and
  this is where it goes; systemd reads the file as root before dropping
  privileges, so the service user never needs to read it. (The receiver already
  never logs the value.)
- **`avahi-daemon.service` in `After`/`Wants`, not a hard dependency** —
  matching the Recommends above and the library's warn-and-continue behavior.

`/etc/default/openairplay2-receiver` ships commented-out examples of the full
flag set (`--name`, `--alsa-device`, `--port`, `--pincode`, `--no-avahi`) with
one active line, e.g. `OPENAIRPLAY2_ARGS="--name Living Room"`. Unquoted `$…`
expansion in `ExecStart` word-splits, which is what we want; no config-file
parsing is added to the binary.

### The one code change

`main.rs` currently waits on `tokio::signal::ctrl_c()` only. Under systemd,
`systemctl stop` sends **SIGTERM**, which has no handler and so terminates the
process abruptly, skipping the "shutting down" path. The unit warrants handling
it: select on SIGTERM alongside SIGINT (`tokio::signal::unix::signal`, behind
`cfg(unix)`; the `signal` feature is already enabled). This is the only Rust
change in the plan — everything else is packaging.

### Architectures and how they're built

amd64 and arm64 only, both **native**: `ubuntu-latest` and the free
`ubuntu-24.04-arm` runners, each in a `debian:trixie` container so the produced
binary links against trixie's glibc/ALSA (a Debian 13 baseline; older releases
are not a target). No cross-compilation anywhere — the multiarch sysroot dance
radio needs for armhf simply does not arise here. `packaging/build-deb.sh` is
therefore a thin script: refuse a requested arch that isn't the host's, check
`cargo-deb` is present, run `cargo deb -p openairplay2-receiver`, print the
resulting path. (If cross builds are ever wanted, radio's `service/build-deb.sh`
is the reference implementation to copy.)

One thing to verify early: how `cargo-deb` resolves the
`target/release/openairplay2-receiver` asset path in a **workspace** (the target
directory is at the workspace root, not the package root). If the relative path
doesn't resolve, the fallback is an explicit workspace-relative path or
`--target-dir`; either way it is a one-line fix in the metadata, discovered on
the first local build rather than at release time.

### Release workflow

`release.yml` moves from `on: push: tags: [v*]` to `on: release: [published]`,
matching radio's rule that **publishing a GitHub Release is the only event that
ships anything**. That keeps one workflow file (so the crates.io Trusted
Publishing configuration, which is keyed on `release.yml`, is untouched) and
gives the `.deb` jobs a release to attach to. The tag-push path is retired
rather than adapted — it has never actually run a release (both crates were
published by hand at 0.2.0), so nothing is being destabilized.

Jobs:

1. **`version`** — assert the tag (minus `v`) equals
   `openairplay2-receiver/Cargo.toml`'s version; warn-only for prereleases so
   `vX.Y.Z-rcN` tags can exercise the workflow end to end.
2. **`crates-io`** — the existing steps unchanged: test, OIDC auth, publish the
   library, then the binary.
3. **`deb` (matrix: amd64, arm64)** — `debian:trixie` container, apt
   `build-essential pkg-config libasound2-dev git curl ca-certificates`, rustup
   (trixie's rustc predates our `rust-version = 1.88`), `Swatinem/rust-cache`,
   `cargo test --workspace`, `cargo install cargo-deb --locked`,
   `packaging/build-deb.sh <arch>`, `actions/attest-build-provenance`, upload
   the artifact. `fail-fast: false` — one runner flake must not cancel the other
   architecture.
4. **`release-assets`** — download both, `sha256sum > SHA256SUMS`,
   `gh release upload --repo "$GITHUB_REPOSITORY" "$TAG" --clobber`.

Permissions per job, least-privilege: `id-token`/`attestations: write` on the
build jobs, `contents: write` only on the upload job.

### Docs

- **`runbooks/releasing.md`** is rewritten around "publish a GitHub Release":
  version-bump PR → `gh release create vX.Y.Z --generate-notes` → watch the
  workflow → verify crates.io **and** install the `.deb` on a real box. The
  existing "if a release goes wrong" and autopilot sections carry over, plus
  radio's failure procedure (`gh release delete vX.Y.Z --cleanup-tag`, fix on
  main, re-create the release) and the prerelease-tag trick.
- **README** gains an "Install" section: download the `.deb` for your
  architecture, `apt-get install ./…deb`, edit `/etc/default/…`, `systemctl
  restart`, plus the `gh attestation verify` one-liner — ahead of the existing
  build-from-source instructions.
- **CLAUDE.md** gets a one-line pointer to `packaging/` in the workspace
  description.

## Out of scope

- **armhf / 32-bit ARM**, and any cross-compilation — amd64 and arm64 build
  natively on GitHub runners, which is the whole hardware story here.
- **An APT repository** (`apt.example/…`, signed `Release` files, `apt update`
  upgrades). Downloading a `.deb` from the GitHub Release is the install path;
  a repo can come later without invalidating any of this.
- **Official Debian packaging** — no `debian/` source package, no `dh-cargo`,
  no vendored-crate tarball, no upload to Debian proper.
- **Packaging the library crate** (`openairplay2`) — it is a Rust dependency and
  belongs on crates.io only.
- **Config-file support in the binary**, beyond the `/etc/default` flag string;
  a real config file (like radio's `config.toml`) is a separate change if the
  flag list ever outgrows one line.
- Docker/OCI images, RPMs, Arch packages, Homebrew.
- Any change to the protocol, audio path, or library API.

## Phases

Each phase is one PR stacked on this plan.

**Phase 1 — package it.** `packaging/` (unit, `/etc/default` file, maintainer
scripts, `build-deb.sh`), `[package.metadata.deb]` in the receiver's
`Cargo.toml`, and the SIGTERM handler in `main.rs`. Ends with a `.deb` built
locally on amd64 and installed in a Debian 13 container/VM.

**Phase 2 — ship it.** `release.yml` rewritten (version check, crates.io, the
amd64/arm64 `.deb` matrix, attestation, asset upload), `runbooks/releasing.md`
rewritten, README install section, CLAUDE.md pointer. Ends with a prerelease
tag (`vX.Y.Z-rcN`) driving a full workflow run whose artifacts are inspected
and then deleted.

## Test strategy

Nothing here is unit-testable; the verification is the artifact itself.

- **Static checks:** `cargo test --workspace`, `clippy --all-targets -D
  warnings`, `fmt --check` stay green (the SIGTERM change is the only code
  touched, and it must not break the macOS library build — it is `cfg(unix)` in
  the *binary*, which is Linux-only anyway). `shellcheck` on `build-deb.sh` and
  the maintainer scripts.
- **Package inspection:** `dpkg -c` (paths, modes: binary `0755`, env file
  `0640`), `dpkg -I` (Depends/Recommends/section/version), `lintian` (clean or
  with each remaining tag explained in the PR).
- **Install, in a Debian 13 container:** `apt-get install ./…deb` creates the
  `openairplay2` user in `audio`, installs and enables the unit, and
  `systemctl status` shows it running (with `--no-audio` in the env file where
  the container has no sound device). `apt-get remove` stops it; `purge` leaves
  `/var/lib/openairplay2` and the user, per Debian practice.
- **Upgrade path:** install version N, then N+1 — the running daemon restarts
  (`restart-after-upgrade`), a locally edited `/etc/default/…` is preserved
  (conffile), and the identity in `/var/lib/openairplay2` is unchanged, so a
  Mac that paired before the upgrade still sees the same receiver.
- **On real hardware (the acceptance check this project always requires):**
  install the arm64 `.deb` on an arm64 Linux box wired to an amp, reboot, and
  from a real Mac/iPhone discover it, pair, play, pause/seek/volume. Confirm
  `journalctl -u openairplay2-receiver` shows the startup lines and stays quiet
  at `info` while streaming (the logging convention from plan `20260805-02`),
  and that `systemctl stop` shuts down cleanly via the new SIGTERM path.
- **The workflow itself** is tested with a prerelease tag before any real
  release, then the prerelease and its tag are deleted.

## Acceptance criteria

- Publishing a GitHub Release produces, on the release page:
  `openairplay2-receiver_X.Y.Z-1_amd64.deb`,
  `openairplay2-receiver_X.Y.Z-1_arm64.deb`, `SHA256SUMS`, each `.deb` with a
  provenance attestation that `gh attestation verify --repo st3fan/openairplay2`
  accepts — and both crates published to crates.io by the same run.
- On a fresh Debian 13 arm64 box: one `apt-get install ./…deb` yields a
  receiver that a real Mac discovers, pairs with, and plays through, and that
  comes back by itself after a reboot — with configuration limited to editing
  `/etc/default/openairplay2-receiver` and restarting the unit.
- Upgrading preserves the identity (no re-pairing) and the local config; the
  new binary is the one running afterwards.
- `packaging/build-deb.sh` builds the same package on a developer machine as CI
  produces.
- `cargo test --workspace`, clippy, and fmt green; no new Rust dependencies; the
  library remains ALSA-free and macOS-green.
- `runbooks/releasing.md` describes the shipped procedure exactly, and the
  README's install section is the first thing a new user can follow.
