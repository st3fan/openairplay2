#!/bin/sh
# Builds openairplay2-receiver_<version>-1_<arch>.deb and
# openairplay2-tui_<version>-1_<arch>.deb on Debian 13.
#
# amd64 and arm64 build natively (both have runners and hardware). armhf —
# Debian's ARMv7 port — has no 32-bit ARM runners anywhere, so it is
# cross-compiled from an amd64 host against a Debian multiarch sysroot.
#
# One-time setup: ./setup-build.sh [cross], rustup, cargo install cargo-deb.

set -eu

cd "$(dirname "$0")/.."

HOST=$(dpkg --print-architecture)
ARCH="${1:-$HOST}"

case "$ARCH" in
amd64) TRIPLE=x86_64-unknown-linux-gnu ;;
arm64) TRIPLE=aarch64-unknown-linux-gnu ;;
armhf) TRIPLE=armv7-unknown-linux-gnueabihf ;;
*)
    echo "usage: $0 [amd64|arm64|armhf]" >&2
    exit 2
    ;;
esac

command -v cargo-deb >/dev/null || {
    echo "build-deb.sh: cargo-deb not found — run: cargo install cargo-deb" >&2
    exit 1
}

if [ "$ARCH" = "$HOST" ]; then
    cargo deb -p openairplay2-receiver
    cargo deb -p openairplay2-tui
    RECEIVER_DEB=$(ls target/debian/openairplay2-receiver_*_"$ARCH".deb)
    TUI_DEB=$(ls target/debian/openairplay2-tui_*_"$ARCH".deb)
else
    if [ "$ARCH" != armhf ]; then
        echo "build-deb.sh: this host is $HOST; $ARCH must be built natively" >&2
        exit 2
    fi
    if [ "$HOST" != amd64 ]; then
        echo "build-deb.sh: cross builds are only supported from amd64 hosts" >&2
        exit 2
    fi

    rustup target list --installed | grep -qx "$TRIPLE" || rustup target add "$TRIPLE"

    # Debian multiarch is the sysroot: the linker comes from
    # crossbuild-essential-armhf, and the pkg-config wrapper from pkgconf:armhf
    # points alsa-sys at the target's libasound rather than the host's. No
    # bindgen anywhere in the tree, so no clang target flags are needed. (The
    # tui needs none of the pkg-config machinery — its only native library is
    # libc — but the exports are harmless there.)
    export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc
    export CC_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-gcc
    export PKG_CONFIG_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-pkg-config

    cargo deb -p openairplay2-receiver --target "$TRIPLE"
    cargo deb -p openairplay2-tui --target "$TRIPLE"
    RECEIVER_DEB=$(ls "target/$TRIPLE/debian/openairplay2-receiver_"*_"$ARCH".deb)
    TUI_DEB=$(ls "target/$TRIPLE/debian/openairplay2-tui_"*_"$ARCH".deb)
fi

echo "receiver deb: $RECEIVER_DEB"
echo "tui deb:      $TUI_DEB"
# Native and cross builds land in different directories; hand the paths to the
# workflow rather than making it guess with globs.
if [ -n "${GITHUB_OUTPUT:-}" ]; then
    echo "receiver_deb=$RECEIVER_DEB" >>"$GITHUB_OUTPUT"
    echo "tui_deb=$TUI_DEB" >>"$GITHUB_OUTPUT"
fi
