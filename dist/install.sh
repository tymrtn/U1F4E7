#!/usr/bin/env bash
# install.sh — Envelope curl-pipe installer
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/tymrtn/U1F4E7/main/dist/install.sh | bash
#   curl -fsSL ... | bash -s -- --version v1.2.3
#   curl -fsSL ... | bash -s -- --bin-dir ~/.local/bin
#
# Flags:
#   --version <vX.Y.Z>   Install a specific version instead of latest
#   --bin-dir <dir>      Installation directory (default: ~/.local/bin)
#   --allow-root         Allow running as root (not recommended)
#
# Safety: set -euo pipefail, cleanup trap, refuses root unless --allow-root,
#         sha256 checksum verified before extraction, no sudo anywhere.

set -euo pipefail

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------
REPO="tymrtn/U1F4E7"
BINARY_NAME="envelope"
GITHUB_API="https://api.github.com/repos/${REPO}/releases/latest"
GITHUB_RELEASES="https://github.com/${REPO}/releases/download"

# ---------------------------------------------------------------------------
# Defaults (may be overridden by flags)
# ---------------------------------------------------------------------------
REQUESTED_VERSION=""
BIN_DIR="${HOME}/.local/bin"
ALLOW_ROOT=false

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
    case "$1" in
        --version)
            REQUESTED_VERSION="$2"
            shift 2
            ;;
        --version=*)
            REQUESTED_VERSION="${1#--version=}"
            shift
            ;;
        --bin-dir)
            BIN_DIR="$2"
            shift 2
            ;;
        --bin-dir=*)
            BIN_DIR="${1#--bin-dir=}"
            shift
            ;;
        --allow-root)
            ALLOW_ROOT=true
            shift
            ;;
        -h|--help)
            grep '^#' "$0" | grep -v '^#!/' | sed 's/^# //' | sed 's/^#//'
            exit 0
            ;;
        *)
            echo "Unknown flag: $1" >&2
            echo "Usage: install.sh [--version vX.Y.Z] [--bin-dir DIR] [--allow-root]" >&2
            exit 1
            ;;
    esac
done

# ---------------------------------------------------------------------------
# Root guard
# ---------------------------------------------------------------------------
if [[ "${EUID:-$(id -u)}" -eq 0 ]] && [[ "$ALLOW_ROOT" != "true" ]]; then
    echo "ERROR: Running as root is not recommended and is disabled by default." >&2
    echo "       Use --allow-root to override, or run as a normal user (preferred)." >&2
    echo "       Envelope installs to ~/.local/bin — no sudo needed." >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Detect OS and arch; map to Rust target triple
# ---------------------------------------------------------------------------
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Darwin)
        # macOS: point at Homebrew and exit. The Homebrew tap keeps pace with
        # releases, handles upgrades gracefully, and puts the binary on PATH.
        echo ""
        echo "  Envelope on macOS is installed via Homebrew:"
        echo ""
        echo "    brew install tymrtn/u1f4e7/u1f4e7"
        echo ""
        echo "  If you already have it:"
        echo ""
        echo "    brew upgrade tymrtn/u1f4e7/u1f4e7"
        echo ""
        echo "  After installing, run:  envelope accounts add"
        echo "  Then:                   envelope quickstart"
        echo ""
        exit 0
        ;;
    Linux)
        case "$ARCH" in
            x86_64)
                TARGET="x86_64-unknown-linux-gnu"
                ;;
            aarch64|arm64)
                TARGET="aarch64-unknown-linux-gnu"
                ;;
            *)
                echo "ERROR: Unsupported architecture: ${ARCH}" >&2
                echo "       Envelope provides Linux x86_64 and aarch64 binaries." >&2
                echo "       To build from source: https://github.com/${REPO}" >&2
                exit 1
                ;;
        esac
        ;;
    *)
        echo "ERROR: Unsupported OS: ${OS}" >&2
        echo "       Envelope supports macOS (Homebrew) and Linux (x86_64, aarch64)." >&2
        exit 1
        ;;
esac

# ---------------------------------------------------------------------------
# Temp dir with cleanup trap
# ---------------------------------------------------------------------------
TMPDIR_WORK="$(mktemp -d "${TMPDIR:-/tmp}/envelope-install.XXXXXX")"
cleanup() {
    rm -rf "$TMPDIR_WORK"
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Resolve version
# ---------------------------------------------------------------------------
if [[ -n "$REQUESTED_VERSION" ]]; then
    VERSION="$REQUESTED_VERSION"
    echo "Installing Envelope ${VERSION} (requested)..."
else
    echo "Resolving latest Envelope release..."
    RELEASES_RESPONSE="$(curl -fsSL "$GITHUB_API" 2>/dev/null || true)"

    # Detect "Not Found" (404) or missing tag_name — no releases published yet
    if [[ -z "$RELEASES_RESPONSE" ]] || echo "$RELEASES_RESPONSE" | grep -q '"message".*"Not Found"'; then
        echo ""
        echo "  No Envelope releases have been published yet." >&2
        echo ""
        echo "  Envelope is in active development. Watch for the first release at:" >&2
        echo "    https://github.com/${REPO}/releases" >&2
        echo ""
        echo "  To build from source now:" >&2
        echo "    git clone https://github.com/${REPO}.git" >&2
        echo "    cd U1F4E7 && cargo build --release --bin envelope" >&2
        echo ""
        exit 1
    fi

    VERSION="$(echo "$RELEASES_RESPONSE" | grep '"tag_name"' | head -1 | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"

    if [[ -z "$VERSION" ]]; then
        echo "ERROR: Could not parse release tag from GitHub API response." >&2
        echo "       Response snippet: $(echo "$RELEASES_RESPONSE" | head -3)" >&2
        exit 1
    fi

    # Detect releases with no binary assets
    ASSET_COUNT="$(echo "$RELEASES_RESPONSE" | grep -c '"browser_download_url"' || true)"
    if [[ "$ASSET_COUNT" -eq 0 ]]; then
        echo ""
        echo "  Release ${VERSION} exists but has no binary assets attached." >&2
        echo ""
        echo "  This can happen if the release workflow has not completed yet," >&2
        echo "  or if this is a source-only release." >&2
        echo ""
        echo "  Check: https://github.com/${REPO}/releases/tag/${VERSION}" >&2
        echo ""
        echo "  To build from source:" >&2
        echo "    git clone https://github.com/${REPO}.git" >&2
        echo "    cd U1F4E7 && cargo build --release --bin envelope" >&2
        echo ""
        exit 1
    fi

    echo "Latest release: ${VERSION}"
fi

# Strip leading 'v' for the version number used in filenames
VERSION_NUM="${VERSION#v}"

# ---------------------------------------------------------------------------
# Construct artifact URLs
# ---------------------------------------------------------------------------
PACKAGE_NAME="envelope-${VERSION_NUM}-${TARGET}"
TARBALL_FILE="${PACKAGE_NAME}.tar.gz"
SHA256_FILE="${TARBALL_FILE}.sha256"
TARBALL_URL="${GITHUB_RELEASES}/${VERSION}/${TARBALL_FILE}"
SHA256_URL="${GITHUB_RELEASES}/${VERSION}/${SHA256_FILE}"

# ---------------------------------------------------------------------------
# Download tarball and checksum
# ---------------------------------------------------------------------------
echo "Downloading ${TARBALL_FILE}..."
if ! curl -fsSL --retry 3 --retry-delay 2 -o "${TMPDIR_WORK}/${TARBALL_FILE}" "${TARBALL_URL}"; then
    echo "ERROR: Download failed: ${TARBALL_URL}" >&2
    echo "       Check that release ${VERSION} has a ${TARGET} binary:" >&2
    echo "       https://github.com/${REPO}/releases/tag/${VERSION}" >&2
    exit 1
fi

echo "Downloading checksum..."
if ! curl -fsSL --retry 3 --retry-delay 2 -o "${TMPDIR_WORK}/${SHA256_FILE}" "${SHA256_URL}"; then
    echo "ERROR: Checksum download failed: ${SHA256_URL}" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Verify checksum
# ---------------------------------------------------------------------------
echo "Verifying checksum..."
cd "${TMPDIR_WORK}"
if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "${SHA256_FILE}"
elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -c "${SHA256_FILE}"
else
    echo "ERROR: Neither sha256sum nor shasum found; cannot verify download." >&2
    echo "       Install one of these tools and retry." >&2
    exit 1
fi
cd - >/dev/null

# ---------------------------------------------------------------------------
# Extract and install
# ---------------------------------------------------------------------------
echo "Extracting..."
tar -C "${TMPDIR_WORK}" -xzf "${TMPDIR_WORK}/${TARBALL_FILE}"

EXTRACTED_BIN="${TMPDIR_WORK}/${PACKAGE_NAME}/${BINARY_NAME}"
if [[ ! -f "$EXTRACTED_BIN" ]]; then
    echo "ERROR: Expected binary not found after extraction: ${EXTRACTED_BIN}" >&2
    echo "       Archive contents:" >&2
    ls "${TMPDIR_WORK}" >&2
    exit 1
fi

mkdir -p "$BIN_DIR"
cp "$EXTRACTED_BIN" "${BIN_DIR}/${BINARY_NAME}"
chmod +x "${BIN_DIR}/${BINARY_NAME}"

# ---------------------------------------------------------------------------
# PATH check and advice
# ---------------------------------------------------------------------------
SHELL_CONFIG=""
case "${SHELL:-}" in
    */zsh)  SHELL_CONFIG="${HOME}/.zshrc" ;;
    */bash) SHELL_CONFIG="${HOME}/.bashrc" ;;
    *)      SHELL_CONFIG="your shell config" ;;
esac

if ! echo ":${PATH}:" | grep -q ":${BIN_DIR}:"; then
    echo ""
    echo "  NOTE: ${BIN_DIR} is not on your PATH."
    echo ""
    echo "  Add it to ${SHELL_CONFIG}:"
    echo "    export PATH=\"${BIN_DIR}:\$PATH\""
    echo ""
    echo "  Then reload your shell:"
    echo "    source ${SHELL_CONFIG}"
    echo ""
fi

# ---------------------------------------------------------------------------
# Smoke test
# ---------------------------------------------------------------------------
echo "Verifying installation..."
INSTALLED_VERSION="$("${BIN_DIR}/${BINARY_NAME}" --version 2>&1)"
echo "  ${INSTALLED_VERSION}"

# ---------------------------------------------------------------------------
# Next steps
# ---------------------------------------------------------------------------
echo ""
echo "  Envelope ${VERSION} installed to ${BIN_DIR}/${BINARY_NAME}"
echo ""
echo "  Next steps:"
echo "    1. Add an account:  envelope accounts add"
echo "    2. Run quickstart:  envelope quickstart"
echo "    3. Check your inbox: envelope inbox"
echo ""
echo "  Docs: https://github.com/${REPO}#readme"
echo ""
