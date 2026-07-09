#!/usr/bin/env bash
# package-release.sh — package an already-built envelope binary for distribution.
#
# Usage:
#   scripts/package-release.sh [--target <triple>]
#
# --target  Rust target triple to package (default: host triple from `rustc -vV`).
#           This script does NOT compile — it expects the binary to already exist
#           at target/<triple>/release/envelope (or target/release/envelope when
#           the triple matches the host).  Building is CI's job.
#
# Output:
#   dist/envelope-<version>-<triple>.tar.gz
#   dist/envelope-<version>-<triple>.tar.gz.sha256
#
# Artifact naming is stable and must match the release.yml upload/attach steps.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
TARGET=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --target)
            TARGET="$2"
            shift 2
            ;;
        --target=*)
            TARGET="${1#--target=}"
            shift
            ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 [--target <triple>]" >&2
            exit 1
            ;;
    esac
done

# Default to the host triple when --target is not supplied.
if [[ -z "$TARGET" ]]; then
    TARGET="$(rustc -vV 2>/dev/null | awk '/^host:/ { print $2 }')"
    if [[ -z "$TARGET" ]]; then
        echo "Cannot determine host triple; pass --target explicitly." >&2
        exit 1
    fi
fi

# ---------------------------------------------------------------------------
# Resolve binary path.  Cargo places cross-compiled outputs under
# target/<triple>/release/; native builds also land under target/release/.
# We try the triple-qualified path first so cross-compiled artifacts work,
# then fall back to the short path for host builds.
# ---------------------------------------------------------------------------
HOST_TRIPLE="$(rustc -vV 2>/dev/null | awk '/^host:/ { print $2 }')"

TRIPLE_BIN="$ROOT_DIR/target/${TARGET}/release/envelope"
HOST_BIN="$ROOT_DIR/target/release/envelope"

if [[ -f "$TRIPLE_BIN" ]]; then
    TARGET_BIN="$TRIPLE_BIN"
elif [[ "$TARGET" == "$HOST_TRIPLE" && -f "$HOST_BIN" ]]; then
    TARGET_BIN="$HOST_BIN"
else
    echo "Binary not found for target ${TARGET}." >&2
    echo "Expected one of:" >&2
    echo "  $TRIPLE_BIN" >&2
    [[ "$TARGET" == "$HOST_TRIPLE" ]] && echo "  $HOST_BIN" >&2
    echo "Build with: cargo build --release --bin envelope --target ${TARGET}" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Version from workspace manifest (stable via cargo pkgid)
# ---------------------------------------------------------------------------
VERSION="$(cargo pkgid -p envelope-email | sed -E 's/.*@([0-9]+\.[0-9]+\.[0-9]+)$/\1/')"
if [[ -z "$VERSION" ]]; then
    echo "Could not determine version from cargo pkgid." >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Artifact paths (naming convention: envelope-<version>-<triple>)
# ---------------------------------------------------------------------------
DIST_DIR="$ROOT_DIR/dist"
PACKAGE_NAME="envelope-${VERSION}-${TARGET}"
PACKAGE_ROOT="$DIST_DIR/${PACKAGE_NAME}"
TARBALL="$DIST_DIR/${PACKAGE_NAME}.tar.gz"
SHA256_FILE="${TARBALL}.sha256"

binary_size() {
    if stat -f%z "$1" >/dev/null 2>&1; then
        stat -f%z "$1"
    else
        stat -c%s "$1"
    fi
}

# ---------------------------------------------------------------------------
# Clean stale release artifacts so older versions/targets don't ship.
# Only envelope-* entries are removed — dist/ also holds tracked non-artifact
# content (dist/systemd/ unit templates) that must survive packaging.
# ---------------------------------------------------------------------------
mkdir -p "$DIST_DIR"
rm -rf "$DIST_DIR"/envelope-*

# ---------------------------------------------------------------------------
# Strip symbols (best-effort; cross-compiled binaries may need llvm-strip
# with a prefixed toolchain; skip silently when unavailable for the target)
# ---------------------------------------------------------------------------
if command -v strip >/dev/null 2>&1; then
    strip "$TARGET_BIN" 2>/dev/null || true
elif command -v llvm-strip >/dev/null 2>&1; then
    llvm-strip "$TARGET_BIN" 2>/dev/null || true
fi

# ---------------------------------------------------------------------------
# Size guard (25 MiB binary, 20 MiB tarball)
# ---------------------------------------------------------------------------
BIN_BYTES="$(binary_size "$TARGET_BIN")"
if [ "$BIN_BYTES" -ge $((25 * 1024 * 1024)) ]; then
    echo "release binary too large: ${BIN_BYTES} bytes (limit 26214400)" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Assemble package directory
# ---------------------------------------------------------------------------
mkdir -p "$PACKAGE_ROOT"
cp "$TARGET_BIN"        "$PACKAGE_ROOT/envelope"
cp "$ROOT_DIR/LICENSE"  "$PACKAGE_ROOT/LICENSE"
cp "$ROOT_DIR/README.md" "$PACKAGE_ROOT/README.md"

# ---------------------------------------------------------------------------
# Create tarball
# ---------------------------------------------------------------------------
COPYFILE_DISABLE=1 tar -C "$DIST_DIR" -czf "$TARBALL" "$(basename "$PACKAGE_ROOT")"

TARBALL_BYTES="$(binary_size "$TARBALL")"
if [ "$TARBALL_BYTES" -ge $((20 * 1024 * 1024)) ]; then
    echo "release tarball too large: ${TARBALL_BYTES} bytes (limit 20971520)" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# SHA-256 checksum (sha256sum on Linux; shasum -a 256 on macOS).
# Filename-only in the checksum line so users can run `sha256sum -c` in the
# directory holding the downloaded tarball.
# ---------------------------------------------------------------------------
(
    cd "$DIST_DIR"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$(basename "$TARBALL")" > "$SHA256_FILE"
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$(basename "$TARBALL")" > "$SHA256_FILE"
    else
        echo "Neither sha256sum nor shasum found; skipping checksum." >&2
    fi
)

# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------
echo "Target:        ${TARGET}"
echo "Version:       ${VERSION}"
echo "Tarball:       ${TARBALL}"
echo "SHA256:        ${SHA256_FILE}"
echo "Binary size:   ${BIN_BYTES} bytes"
echo "Tarball size:  ${TARBALL_BYTES} bytes"
