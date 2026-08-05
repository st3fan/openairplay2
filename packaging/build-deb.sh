#!/bin/sh
# Builds openairplay2-receiver_<version>-1_<arch>.deb on Debian 13.
#
# Native builds only: amd64 and arm64 both have runners (and hardware), and
# the receiver links nothing but ALSA and libc, so there is no reason to
# cross-compile. If a cross build is ever needed, radio's service/build-deb.sh
# is the reference (Debian multiarch sysroot + per-target linker/pkg-config).
#
# One-time setup: apt install build-essential pkg-config libasound2-dev,
# rustup, and `cargo install cargo-deb`.

set -eu

cd "$(dirname "$0")/.."

HOST=$(dpkg --print-architecture)
ARCH="${1:-$HOST}"

case "$ARCH" in
amd64 | arm64) ;;
*)
    echo "usage: $0 [amd64|arm64]" >&2
    exit 2
    ;;
esac

if [ "$ARCH" != "$HOST" ]; then
    echo "build-deb.sh: this host is $HOST; $ARCH must be built natively" >&2
    exit 2
fi

command -v cargo-deb >/dev/null || {
    echo "build-deb.sh: cargo-deb not found — run: cargo install cargo-deb" >&2
    exit 1
}

cargo deb -p openairplay2-receiver
echo "deb: $(ls target/debian/openairplay2-receiver_*_"$ARCH".deb)"
