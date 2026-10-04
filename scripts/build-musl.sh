#!/usr/bin/env bash
# Static musl release build (Phase 7): produces a self-contained
# `wrosecode` that runs on any Linux without glibc version worries.
#   ./scripts/build-musl.sh
# Needs the musl target and a musl C compiler:
#   rustup target add x86_64-unknown-linux-musl
#   apt install musl-tools        # provides musl-gcc
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=x86_64-unknown-linux-musl

if ! rustup target list --installed 2>/dev/null | grep -qx "$TARGET"; then
    echo "error: rust target '$TARGET' is not installed" >&2
    echo "  rustup target add $TARGET" >&2
    exit 1
fi

if ! command -v musl-gcc >/dev/null 2>&1; then
    echo "error: musl-gcc not found (Debian/Ubuntu: apt install musl-tools)" >&2
    exit 1
fi

export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc
export CC_x86_64_unknown_linux_musl=musl-gcc

cargo build --release --target "$TARGET" --locked

BIN="target/$TARGET/release/wrosecode"
if command -v file >/dev/null 2>&1; then
    file "$BIN"
fi
echo "built $BIN"
