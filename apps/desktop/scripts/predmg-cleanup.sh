#!/usr/bin/env bash
# Detach leftover build volumes before bundling.
#
# bundle_dmg.sh fails outright if a previous run left /Volumes/dmg.* mounted,
# which happens whenever a build is interrupted. The failure surfaces as an
# opaque "error running bundle_dmg.sh" with nothing pointing at the cause.
set -euo pipefail

shopt -s nullglob
for volume in /Volumes/dmg.*; do
  [ -d "$volume" ] || continue
  echo "==> detaching leftover build volume $volume"
  hdiutil detach "$volume" -force >/dev/null 2>&1 || true
done
