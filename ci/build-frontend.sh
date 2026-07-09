#!/usr/bin/env bash
# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2
#
# Build the Envelope v2 webmail SPA and verify the committed `web/build/` output
# is current. `cargo install` embeds `web/build/` via rust-embed, so the built
# bundle MUST be committed and MUST match the sources — a rebuild that changes
# anything means the committed bundle is stale.
#
# Usage: run from the repo root (or anywhere; it cd's to the web project).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
WEB_DIR="${REPO_ROOT}/crates/dashboard/web"
BUILD_DIR="${WEB_DIR}/build"

echo "==> Building v2 webmail in ${WEB_DIR}"
cd "${WEB_DIR}"

echo "==> npm ci"
npm ci

echo "==> npm run build"
npm run build

if [[ ! -f "${BUILD_DIR}/index.html" ]]; then
  echo "ERROR: ${BUILD_DIR}/index.html was not produced by the build." >&2
  exit 1
fi

# Guard: the bundle must never carry the Tailwind play-CDN.
if grep -rq "cdn.tailwindcss" "${BUILD_DIR}"; then
  echo "ERROR: build/ contains a cdn.tailwindcss reference — use the Tailwind build, not the CDN." >&2
  exit 1
fi

echo "==> Verifying committed build/ is current"
# Fail if the build produced changes that aren't committed. The committed bundle
# is what ships in the binary, so a dirty tree after build means someone forgot
# to commit the rebuilt output.
if ! git -C "${REPO_ROOT}" diff --quiet -- "${BUILD_DIR}" \
   || [[ -n "$(git -C "${REPO_ROOT}" ls-files --others --exclude-standard -- "${BUILD_DIR}")" ]]; then
  echo "ERROR: crates/dashboard/web/build/ is out of date after building." >&2
  echo "       The committed SPA bundle does not match the sources." >&2
  echo "       Commit the rebuilt output:" >&2
  echo "         git add crates/dashboard/web/build && git commit" >&2
  echo >&2
  echo "Changed/new files under build/:" >&2
  git -C "${REPO_ROOT}" status --porcelain -- "${BUILD_DIR}" >&2
  exit 1
fi

echo "==> OK: v2 webmail bundle built and committed-and-current."
