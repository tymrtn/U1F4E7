#!/bin/sh
# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)
#
# Regenerate the vendored, weight-free Envelope Governor catalog projection.
#
# The vendored file crates/email/src/governor_catalog.gen.json is the single
# public projection Envelope compiles in (key/description/category only — never
# weights or thresholds). It is CHECKED IN, not generated at build time, so
# attribution validation still works when the Governor binary is absent.
#
# This script regenerates that file from the authoritative Governor catalog when
# a Governor build that supports `governor catalog --catalog envelope --json`
# (the `governor.catalog.v1` projection) is available. Until Governor ships that
# command, this script is a no-op stub that reports honestly and leaves the
# checked-in projection untouched — it never fabricates a projection.
#
# Usage: ENVELOPE_GOVERNOR_BIN=/path/to/governor scripts/gen-governor-catalog.sh

set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/crates/email/src/governor_catalog.gen.json"
OUT_DIR="$(dirname "$OUT")"

BIN="${ENVELOPE_GOVERNOR_BIN:-/Users/tylermartin/Dropbox/Code/governor/governor2/target/release/governor}"

if ! command -v jq >/dev/null 2>&1; then
  echo "gen-governor-catalog: jq is required for the allowlisted projection." >&2
  exit 1
fi

if [ ! -x "$BIN" ]; then
  echo "gen-governor-catalog: no governor binary at '$BIN'." >&2
  echo "  The checked-in projection at $OUT is authoritative; nothing regenerated." >&2
  exit 0
fi

# Private per-run temp files with guaranteed cleanup — never a predictable
# shared /tmp path that a symlink or a concurrent run could hijack. RAW holds
# the live governor output; PROJECTED is created in the destination directory so
# the final replacement is an atomic same-filesystem rename.
RAW="$(mktemp "${TMPDIR:-/tmp}/governor-catalog-raw.XXXXXX")"
# Install cleanup for RAW immediately: if the second mktemp (or anything after)
# fails under `set -e`, the EXIT trap still fires and RAW is not leaked.
trap 'rm -f "$RAW"' EXIT INT TERM
PROJECTED="$(mktemp "$OUT_DIR/governor_catalog.gen.json.XXXXXX")"
trap 'rm -f "$RAW" "$PROJECTED"' EXIT INT TERM

# `governor catalog` is the projection command (Governor Stage G). If this
# Governor build does not support it yet, do not overwrite the vendored file.
if ! "$BIN" catalog --catalog envelope --json >"$RAW" 2>/dev/null; then
  echo "gen-governor-catalog: '$BIN' does not support 'catalog --catalog envelope --json' yet." >&2
  echo "  Ship Governor Stage G (public projection command) before regenerating." >&2
  echo "  The checked-in projection at $OUT remains authoritative; nothing regenerated." >&2
  exit 0
fi

# Positive ALLOWLIST projection: keep ONLY the public, weight-free fields
# (top-level contract/catalog/catalog_version plus per-attribute key/category/
# description) and re-stamp Envelope's own source/note annotations. Any weight,
# threshold, score, or FUTURE calibration field — top-level or per-attribute —
# is dropped structurally, not by a fragile blacklist grep.
if ! jq '{
    contract,
    catalog,
    catalog_version,
    source: "checked-in public projection of the Governor envelope catalog; regenerate with scripts/gen-governor-catalog.sh when the upstream catalog changes",
    note: "Public projection only: key, description, category. Governor'"'"'s protected numeric calibration is intentionally excluded and must never be added here.",
    attributes: [ .attributes[] | { key, category, description } ]
  }' "$RAW" >"$PROJECTED"; then
  echo "gen-governor-catalog: could not project the live catalog JSON (malformed output?)." >&2
  exit 1
fi

# Defense-in-depth after the allowlist: the projected output must carry no
# calibration token and must have a non-empty attributes array.
if grep -Eq '"(weight|threshold|score)"' "$PROJECTED"; then
  echo "gen-governor-catalog: refusing to vendor a projection that contains weights/thresholds/scores." >&2
  exit 1
fi
if [ "$(jq '.attributes | length' "$PROJECTED")" -lt 1 ]; then
  echo "gen-governor-catalog: projected catalog has no attributes; refusing to overwrite $OUT." >&2
  exit 1
fi

# Atomic replacement: PROJECTED is on the same filesystem as OUT, so the rename
# is atomic — no reader ever observes a half-written vendored catalog.
mv "$PROJECTED" "$OUT"
trap 'rm -f "$RAW"' EXIT INT TERM
echo "gen-governor-catalog: regenerated $OUT from $BIN." >&2
