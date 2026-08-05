#!/bin/sh
# Installs the system packages needed to build openairplay2-receiver .debs on
# Debian 13 (trixie) — a build box or a debian:trixie CI container. Idempotent.
#
# Usage:
#   ./setup-build.sh           native build dependencies only (what the amd64
#                              and arm64 legs use)
#   ./setup-build.sh cross     also the armhf (ARMv7) cross toolchain
#                              (amd64 hosts only: the build box and the armhf
#                              CI job)
#
# Rust itself (rustup/cargo, cargo-deb) is deliberately not installed here: it
# is per-user, not system, state. See runbooks/releasing.md.

set -eu

SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO="sudo"

NATIVE_PACKAGES="build-essential pkg-config git ca-certificates curl
    libasound2-dev"

# libasound2-dev is Multi-Arch: same, so the armhf copy co-installs beside the
# native one. pkgconf:armhf provides arm-linux-gnueabihf-pkg-config, whose
# personality points at armhf's multiarch paths — that is what makes alsa-sys'
# pkg-config probe find the target's libasound instead of the host's. armhf is
# Debian's ARMv7 port, NOT the ARMv6 Raspbian world (see debian/preinst).
CROSS_PACKAGES="crossbuild-essential-armhf binutils-arm-linux-gnueabihf
    pkgconf:armhf libasound2-dev:armhf"

case "${1:-native}" in
native)
    $SUDO apt-get update
    # shellcheck disable=SC2086
    $SUDO apt-get install -y $NATIVE_PACKAGES
    ;;
cross)
    host=$(dpkg --print-architecture)
    if [ "$host" != "amd64" ]; then
        echo "setup-build.sh: cross setup is for amd64 hosts (this is $host)" >&2
        exit 2
    fi
    dpkg --print-foreign-architectures | grep -qx armhf ||
        $SUDO dpkg --add-architecture armhf
    $SUDO apt-get update
    # shellcheck disable=SC2086
    $SUDO apt-get install -y $NATIVE_PACKAGES $CROSS_PACKAGES
    ;;
*)
    echo "usage: $0 [native|cross]" >&2
    exit 2
    ;;
esac

echo "setup-build.sh: done"
