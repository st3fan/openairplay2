# armhf: a third .deb for 32-bit ARM (ARMv7), cross-compiled

- **Date:** 2026-08-05
- **Status:** proposed
- **Scope:** packaging and the release workflow only, extending
  [20260805-03](20260805-03-debian-packages.md). No change to the receiver, the
  library, the unit, or the package's contents — only a third architecture.

## Background

Plan `20260805-03` shipped `.deb`s for amd64 and arm64 and put armhf, along
with all cross-compilation, explicitly **out of scope**: both architectures
have native GitHub runners, so nothing had to be cross-built and the packaging
stayed as simple as `cargo deb`. That reasoning still holds for those two — and
does not extend to armhf, because **32-bit ARM runners do not exist**. Adding
armhf means reintroducing cross-compilation, which is the real content of this
plan and the reason it gets one rather than a bare PR.

Two things make it cheap here. The receiver's only non-Rust dependency is ALSA,
and `alsa-sys`' build script is plain `pkg-config` with no bindgen — so the
`BINDGEN_EXTRA_CLANG_ARGS` handling radio needs for ffmpeg has no counterpart
here; a linker, a `pkg-config` wrapper, and the armhf `libasound2-dev` are the
whole sysroot story. And radio already runs this exact arrangement
(`service/build-deb.sh`, `service/setup-build.sh`, the ARMv6 `preinst` guard),
so this is a port of working code, not a design.

Note what it costs: armhf is the first architecture whose **tests never run**
(you cannot execute an armv7 binary on an amd64 host), so its coverage is "it
compiles and links, and someone installed it once on real hardware".

## Goals

- `openairplay2-receiver_X.Y.Z-1_armhf.deb` built by the same release, attached
  to the same GitHub Release with the same provenance attestation, installing
  to the same service.
- `packaging/build-deb.sh armhf` works on any amd64 Debian 13 box, not just in
  CI.
- The package refuses to install on **pre-ARMv7** hardware instead of SIGILLing
  at runtime.
- amd64 and arm64 keep building exactly as they do now: native, no cross
  toolchain involved.

## Design

**Debian multiarch as the sysroot.** On an amd64 host: `dpkg --add-architecture
armhf`, then `crossbuild-essential-armhf`, `pkgconf:armhf`, and
`libasound2-dev:armhf` (Multi-Arch: same, so it co-installs beside the native
copy). `pkgconf:armhf` provides `arm-linux-gnueabihf-pkg-config`, whose
personality points at the armhf multiarch paths — that is what lets
`alsa-sys`' `pkg_config::probe_library("alsa")` find the right `libasound.so`.

**`packaging/setup-build.sh`** (new, ported from radio) installs those system
packages: `native` for the build dependencies both native legs need, `cross`
for the armhf toolchain on top (amd64 hosts only, rejected elsewhere). Rust
itself stays out of it — that is per-user state, and CI installs rustup
separately. `debian.yml`'s inline apt line collapses into a call to it, so the
build box and CI install the same set.

**`packaging/build-deb.sh armhf`** grows a cross path beside the native one:
`rustup target add armv7-unknown-linux-gnueabihf`, then

```
CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc
CC_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-gcc
PKG_CONFIG_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-pkg-config
```

and `cargo deb --target armv7-unknown-linux-gnueabihf`. Cross builds are
refused on non-amd64 hosts with a clear message. The script keeps printing the
resulting path, which now lives under `target/<triple>/debian/` rather than
`target/debian/` — so the artifact and attestation globs in `debian.yml` must
cover both. Rather than gluing two globs together in YAML, `build-deb.sh` will
write the path it produced to `$GITHUB_OUTPUT`-style stdout that the workflow
captures into a step output, keeping one code path for all three legs.

**`debian.yml`** gains a third matrix entry — `armhf`, `ubuntu-latest`,
`setup: cross` — and the `Test` step becomes conditional on the leg being
native. Everything downstream (attestation, artifact upload, the `assets` job's
`SHA256SUMS` and `gh release upload`) already globs per architecture and needs
no restructuring. `fail-fast: false` keeps an armhf failure from cancelling the
architectures people actually run.

**`packaging/debian/preinst`** gains radio's guard: dpkg cannot tell Debian
armhf (ARMv7) from Raspbian armhf (ARMv6), so a Pi Zero W or Pi 1 passes the
architecture check and then dies on an illegal instruction. `preinst` refuses
`armv[1-6]*` with an explanatory message; amd64 and arm64 pass through
untouched.

Package metadata (`depends`, `recommends`, conffile, unit, system user) is
architecture-independent and unchanged — trixie's `libasound2t64` / `libc6` /
`adduser` names are identical on armhf.

## Out of scope

- ARMv6 (Raspbian, Pi Zero W / Pi 1) — explicitly refused, not supported.
- Any other architecture (riscv64, i386, ppc64el).
- Cross-compiling amd64 or arm64: they stay native.
- Running the test suite for armhf (qemu-user emulation in CI) — the leg builds
  only; see the risk noted above.
- An APT repository, official Debian packaging, or any change to the receiver
  itself.

## Phases

Single implementation phase, one PR stacked on this plan: `setup-build.sh`,
the `build-deb.sh` cross path, the `debian.yml` matrix entry, the `preinst`
guard, and the README/runbook lines that mention which architectures exist.

## Test strategy

- **Local, on this amd64 Debian 13 box:** `packaging/setup-build.sh cross` then
  `packaging/build-deb.sh armhf` produces the package; `file` on the extracted
  binary reports `ELF 32-bit LSB … ARM, EABI5`, and `dpkg -I` reports
  `Architecture: armhf` with the same Depends as the other two.
- **Regression:** `packaging/build-deb.sh` (native amd64) still works with the
  cross toolchain installed, and the amd64/arm64 CI legs are unchanged.
- **`preinst` guard:** exercised directly (`sh preinst install` with a stubbed
  `uname`) for both the refusing and the passing case, plus `shellcheck` on
  every script.
- **CI:** `debian.yml` dispatched by hand from the Actions tab builds all three
  legs; the armhf artifact is downloaded and inspected as above, and its
  attestation verified with `gh attestation verify`.
- **On real hardware — the acceptance gate:** install the armhf `.deb` on an
  ARMv7 Debian 13 board, confirm the service starts, and pair and play from a
  real Mac. **This plan is blocked on having such a board**; without one the
  work should not merge, because an architecture nobody has run is worse than
  one we never shipped.

## Acceptance criteria

- A published release carries three `.deb`s (amd64, arm64, armhf) plus
  `SHA256SUMS`, each with a verifying provenance attestation.
- The armhf package installs on an ARMv7 Debian 13 board and yields a receiver
  that a real Mac discovers, pairs with, and plays through, surviving a reboot.
- Installing it on ARMv6 hardware fails in `preinst` with the explanatory
  message, leaving nothing behind.
- `packaging/build-deb.sh armhf` produces the same package on a developer amd64
  box as CI does; native amd64/arm64 builds are unaffected.
- `cargo test --workspace`, clippy, fmt, and `shellcheck` green; no new Rust
  dependencies; the package's contents are byte-for-byte the same set of files
  as the other architectures.
