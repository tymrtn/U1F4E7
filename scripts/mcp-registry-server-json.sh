#!/usr/bin/env bash
# mcp-registry-server-json.sh — write server.json for the official MCP Registry
# from a published GitHub release.
#
# Usage:
#   scripts/mcp-registry-server-json.sh <tag> [output]
#
# Reads the MCP Bundle's SHA-256 from the release's envelope-<version>.mcpb.sha256
# asset, checks it against GitHub's own digest of the .mcpb asset, and writes
# server.json (default: ./server.json). It publishes nothing: run
# `mcp-publisher publish` yourself afterwards.

set -euo pipefail

REPO="tymrtn/U1F4E7"
SCHEMA="https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json"

die() {
    echo "mcp-registry-server-json: $*" >&2
    exit 1
}

if [[ $# -lt 1 || $# -gt 2 ]]; then
    echo "Usage: $0 <tag> [output]" >&2
    exit 1
fi
TAG="$1"
OUT="${2:-server.json}"

[[ "$TAG" =~ ^v([0-9]+\.[0-9]+\.[0-9]+)$ ]] || die "tag must look like vX.Y.Z, got '$TAG'"
VERSION="${BASH_REMATCH[1]}"
MCPB_NAME="envelope-${VERSION}.mcpb"

for tool in gh python3; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is required but not on PATH"
done

digest="$(gh release view "$TAG" --repo "$REPO" --json assets \
    --jq ".assets[] | select(.name == \"$MCPB_NAME\") | .digest")" ||
    die "cannot read release $TAG on $REPO"
[[ -n "$digest" ]] || die "release $TAG has no $MCPB_NAME asset"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/envelope-server-json.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
gh release download "$TAG" --repo "$REPO" --pattern "$MCPB_NAME.sha256" --dir "$tmp" ||
    die "cannot download $MCPB_NAME.sha256 from release $TAG"

sha=""
listed=""
read -r sha listed < "$tmp/$MCPB_NAME.sha256" || true
[[ "$sha" =~ ^[0-9a-f]{64}$ ]] || die "$MCPB_NAME.sha256 does not start with a SHA-256 hash"
[[ "${listed#\*}" == "$MCPB_NAME" ]] || die "$MCPB_NAME.sha256 names '$listed', expected $MCPB_NAME"
[[ "$digest" == "sha256:$sha" ]] ||
    die "$MCPB_NAME.sha256 says $sha but GitHub reports $digest for the asset"

python3 - "$SCHEMA" "$REPO" "$TAG" "$VERSION" "$MCPB_NAME" "$sha" > "$tmp/server.json" <<'PY'
import json
import sys

schema, repo, tag, version, mcpb_name, sha = sys.argv[1:]
server = {
    "$schema": schema,
    "name": "io.github.tymrtn/envelope",
    "title": "Envelope",
    "description": "Your own IMAP/SMTP mailboxes as agent tools, with per-agent policy and human-approved sends.",
    "repository": {"url": f"https://github.com/{repo}", "source": "github"},
    "websiteUrl": "https://u1f4e7.com",
    "version": version,
    "packages": [
        {
            "registryType": "mcpb",
            "identifier": f"https://github.com/{repo}/releases/download/{tag}/{mcpb_name}",
            "fileSha256": sha,
            "transport": {"type": "stdio"},
        }
    ],
}
# Field limits from the 2025-12-11 server.json schema.
for field in ("title", "description"):
    if not 1 <= len(server[field]) <= 100:
        sys.exit(f"mcp-registry-server-json: {field} must be 1-100 characters")
json.dump(server, sys.stdout, indent=2)
sys.stdout.write("\n")
PY
cp "$tmp/server.json" "$OUT"

echo "Wrote $OUT for $TAG (fileSha256 $sha)" >&2
