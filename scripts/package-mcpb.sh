#!/usr/bin/env bash
# package-mcpb.sh — build the MCP Bundle (.mcpb) from packaged release tarballs.
#
# Usage:
#   scripts/package-mcpb.sh <version> <dist-dir> <out-dir> [--targets <t1,t2,...>]
#
# <dist-dir> must hold envelope-<version>-<target>.tar.gz and its .sha256 for
# each target, as package-release.sh writes them. The binaries inside are used
# exactly as shipped (signed and notarized on macOS); nothing is rebuilt.
#
# --targets  Comma-separated subset of the release targets, for a local
#            single-platform bundle. Default: all four release targets.
#
# Output:
#   <out-dir>/envelope-<version>.mcpb
#   <out-dir>/envelope-<version>.mcpb.sha256
#
# The bundle is validated and packed by the official MCPB CLI, pinned below.

set -euo pipefail

MCPB_CLI="@anthropic-ai/mcpb@2.1.2"
ALL_TARGETS=(
    aarch64-apple-darwin
    x86_64-apple-darwin
    x86_64-unknown-linux-gnu
    aarch64-unknown-linux-gnu
)

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEMPLATE_DIR="$ROOT_DIR/packaging/mcpb"

usage() {
    echo "Usage: $0 <version> <dist-dir> <out-dir> [--targets <t1,t2,...>]" >&2
    exit 1
}

die() {
    echo "package-mcpb: $*" >&2
    exit 1
}

# ---------------------------------------------------------------------------
# Arguments
# ---------------------------------------------------------------------------
positional=()
targets_arg=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --targets)
            [[ $# -ge 2 ]] || usage
            targets_arg="$2"
            shift 2
            ;;
        --targets=*)
            targets_arg="${1#--targets=}"
            shift
            ;;
        -*)
            echo "Unknown option: $1" >&2
            usage
            ;;
        *)
            positional+=("$1")
            shift
            ;;
    esac
done
[[ ${#positional[@]} -eq 3 ]] || usage
VERSION="${positional[0]}"
DIST_DIR="${positional[1]}"
OUT_DIR="${positional[2]}"

[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "version must be X.Y.Z, got '$VERSION'"
[[ -d "$DIST_DIR" ]] || die "dist dir not found: $DIST_DIR"

if [[ -n "$targets_arg" ]]; then
    IFS=',' read -r -a TARGETS <<< "$targets_arg"
    [[ ${#TARGETS[@]} -gt 0 ]] || die "--targets is empty"
    for target in "${TARGETS[@]}"; do
        known=false
        for t in "${ALL_TARGETS[@]}"; do
            if [[ "$target" == "$t" ]]; then known=true; fi
        done
        $known || die "unknown target '$target' (release targets: ${ALL_TARGETS[*]})"
    done
else
    TARGETS=("${ALL_TARGETS[@]}")
fi

for tool in python3 npx tar file; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is required but not on PATH"
done

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    else
        die "neither sha256sum nor shasum found"
    fi
}

# Refuse a tarball whose binary is not built for the target it is named after.
check_arch() {
    local target="$1" bin="$2" desc format cpu
    desc="$(file -b "$bin")"
    case "$target" in
        aarch64-apple-darwin) format="Mach-O"; cpu="arm64" ;;
        x86_64-apple-darwin) format="Mach-O"; cpu="x86_64" ;;
        x86_64-unknown-linux-gnu) format="ELF"; cpu="x86-64" ;;
        aarch64-unknown-linux-gnu) format="ELF"; cpu="aarch64" ;;
    esac
    [[ "$desc" == *"$format"* && "$desc" == *"$cpu"* ]] ||
        die "$target binary is not a $format $cpu executable: $desc"
}

# The CI check holds the template version to the Cargo version, so a mismatch
# here means the wrong version was requested or the template was not bumped.
template_version="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["version"])' \
    "$TEMPLATE_DIR/manifest.json")"
[[ "$template_version" == "$VERSION" ]] ||
    die "packaging/mcpb/manifest.json is version $template_version, not $VERSION"

STAGE="$(mktemp -d "${TMPDIR:-/tmp}/envelope-mcpb.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT
BUNDLE="$STAGE/bundle"
mkdir -p "$BUNDLE/server/bin"

# ---------------------------------------------------------------------------
# Verify each tarball against its .sha256 and lay out its binary.
# ---------------------------------------------------------------------------
for target in "${TARGETS[@]}"; do
    name="envelope-${VERSION}-${target}"
    tarball="$DIST_DIR/$name.tar.gz"
    sumfile="$tarball.sha256"
    [[ -f "$tarball" ]] || die "missing $tarball"
    [[ -f "$sumfile" ]] || die "missing $sumfile"

    expected=""
    listed=""
    read -r expected listed < "$sumfile" || true
    [[ "$expected" =~ ^[0-9a-f]{64}$ ]] || die "$sumfile does not start with a SHA-256 hash"
    [[ "${listed#\*}" == "$name.tar.gz" ]] || die "$sumfile names '$listed', expected $name.tar.gz"
    actual="$(sha256_of "$tarball")"
    [[ "$actual" == "$expected" ]] || die "checksum mismatch for $tarball: .sha256 says $expected, file is $actual"

    extract="$STAGE/extract/$target"
    mkdir -p "$extract"
    tar -xzf "$tarball" -C "$extract"
    bin="$extract/$name/envelope"
    [[ -f "$bin" && ! -L "$bin" ]] || die "$tarball does not contain $name/envelope"
    check_arch "$target" "$bin"

    mkdir -p "$BUNDLE/server/bin/$target"
    cp "$bin" "$BUNDLE/server/bin/$target/envelope"
    chmod 755 "$BUNDLE/server/bin/$target/envelope"
    echo "Verified $name.tar.gz ($actual)"
done

first="$STAGE/extract/${TARGETS[0]}/envelope-${VERSION}-${TARGETS[0]}"
for doc in LICENSE README.md; do
    [[ -f "$first/$doc" ]] || die "$doc missing from envelope-${VERSION}-${TARGETS[0]}.tar.gz"
    cp "$first/$doc" "$BUNDLE/$doc"
done
cp "$TEMPLATE_DIR/launcher.sh" "$BUNDLE/server/envelope"
chmod 755 "$BUNDLE/server/envelope"
cp "$TEMPLATE_DIR/icon.png" "$BUNDLE/icon.png"

# The manifest claims only the operating systems this bundle carries.
python3 - "$TEMPLATE_DIR/manifest.json" "$BUNDLE/manifest.json" "${TARGETS[@]}" <<'PY'
import json
import sys

src, dst, *targets = sys.argv[1:]
manifest = json.load(open(src))
present = {"darwin" if t.endswith("-apple-darwin") else "linux" for t in targets}
platforms = manifest["compatibility"]["platforms"]
manifest["compatibility"]["platforms"] = [p for p in platforms if p in present]
with open(dst, "w") as fh:
    json.dump(manifest, fh, indent=2)
    fh.write("\n")
PY

# ---------------------------------------------------------------------------
# Validate and pack with the official CLI, then check the archive kept the
# executable bits (a launcher or binary without them fails only on the
# user's machine).
# ---------------------------------------------------------------------------
mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"
MCPB_NAME="envelope-${VERSION}.mcpb"
MCPB_FILE="$OUT_DIR/$MCPB_NAME"

(cd "$STAGE" && npx --yes "$MCPB_CLI" validate "$BUNDLE/manifest.json")
(cd "$STAGE" && npx --yes "$MCPB_CLI" pack "$BUNDLE" "$MCPB_FILE")
[[ -f "$MCPB_FILE" ]] || die "mcpb pack did not write $MCPB_FILE"

python3 - "$MCPB_FILE" "${TARGETS[@]}" <<'PY'
import sys
import zipfile

path, *targets = sys.argv[1:]
with zipfile.ZipFile(path) as bundle:
    infos = {i.filename: i for i in bundle.infolist()}
    required = ["manifest.json", "server/envelope"] + [f"server/bin/{t}/envelope" for t in targets]
    for name in required:
        info = infos.get(name)
        if info is None:
            sys.exit(f"package-mcpb: {name} is missing from {path}")
        if name != "manifest.json" and not (info.external_attr >> 16) & 0o111:
            sys.exit(f"package-mcpb: {name} lost its executable bit in {path}")
PY

(
    cd "$OUT_DIR"
    printf '%s  %s\n' "$(sha256_of "$MCPB_NAME")" "$MCPB_NAME" > "$MCPB_NAME.sha256"
)

echo "Bundle:   $MCPB_FILE"
echo "SHA256:   $MCPB_FILE.sha256"
echo "Targets:  ${TARGETS[*]}"
echo "Size:     $(wc -c < "$MCPB_FILE" | tr -d ' ') bytes"
