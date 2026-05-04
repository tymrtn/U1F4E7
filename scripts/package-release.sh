#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

VERSION="$(cargo pkgid -p envelope-email | sed -E 's/.*@([0-9]+\.[0-9]+\.[0-9]+)$/\1/')"
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
DIST_DIR="$ROOT_DIR/dist"
TARGET_BIN="$ROOT_DIR/target/release/envelope"
PACKAGE_ROOT="$DIST_DIR/envelope-v${VERSION}-${OS}-${ARCH}"
TARBALL="$DIST_DIR/envelope-v${VERSION}-${OS}-${ARCH}.tar.gz"

binary_size() {
    if stat -f%z "$1" >/dev/null 2>&1; then
        stat -f%z "$1"
    else
        stat -c%s "$1"
    fi
}

mkdir -p "$DIST_DIR"
# Keep dist release-facing and boring: stale package directories from older
# versions make it too easy to upload the wrong artifact.
rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR"

cargo build -p envelope-email --release

if command -v strip >/dev/null 2>&1; then
    strip "$TARGET_BIN" || true
elif command -v llvm-strip >/dev/null 2>&1; then
    llvm-strip "$TARGET_BIN" || true
fi

BIN_BYTES="$(binary_size "$TARGET_BIN")"
if [ "$BIN_BYTES" -ge $((25 * 1024 * 1024)) ]; then
    echo "release binary too large: ${BIN_BYTES} bytes (limit 26214400)" >&2
    exit 1
fi

mkdir -p "$PACKAGE_ROOT"
cp "$TARGET_BIN" "$PACKAGE_ROOT/envelope"
cp "$ROOT_DIR/LICENSE" "$PACKAGE_ROOT/LICENSE"
cp "$ROOT_DIR/README.md" "$PACKAGE_ROOT/README.md"

COPYFILE_DISABLE=1 tar -C "$DIST_DIR" -czf "$TARBALL" "$(basename "$PACKAGE_ROOT")"

TARBALL_BYTES="$(binary_size "$TARBALL")"
if [ "$TARBALL_BYTES" -ge $((20 * 1024 * 1024)) ]; then
    echo "release tarball too large: ${TARBALL_BYTES} bytes (limit 20971520)" >&2
    exit 1
fi

echo "Built $TARBALL"
echo "Binary size: ${BIN_BYTES} bytes"
echo "Tarball size: ${TARBALL_BYTES} bytes"
