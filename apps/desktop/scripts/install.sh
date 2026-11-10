#!/usr/bin/env bash
# Install the built app into /Applications.
#
# macOS refuses to replace a running .app, and dragging it in Finder fails with
# an unhelpful error. Quit it first, then replace rather than copy over: writing
# into the existing bundle leaves the old inode in place, so a running or
# recently-run copy can serve stale binaries.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
desktop_dir="$(dirname "$here")"
app="$desktop_dir/src-tauri/target/release/bundle/macos/Envelope.app"
dest="${INSTALL_DIR:-/Applications}/Envelope.app"

if [ ! -d "$app" ]; then
  echo "error: no built app at $app" >&2
  echo "       run 'npm run stage-sidecar && npm run tauri build' first" >&2
  exit 1
fi

if pgrep -f 'Envelope.app/Contents/MacOS/envelope-desktop' >/dev/null 2>&1; then
  echo "==> quitting the running Envelope"
  osascript -e 'quit app "Envelope"' >/dev/null 2>&1 || true
  for _ in $(seq 1 40); do
    pgrep -f 'Envelope.app/Contents/MacOS/envelope-desktop' >/dev/null 2>&1 || break
    sleep 0.25
  done
  # Its server is a child; if the app was killed rather than quit, it outlives
  # the parent and keeps the mail database open.
  pkill -f 'Envelope.app/Contents/MacOS/envelope serve' >/dev/null 2>&1 || true
fi

if pgrep -f 'Envelope.app/Contents/MacOS/envelope-desktop' >/dev/null 2>&1; then
  echo "error: Envelope is still running and will not be replaced. Quit it and retry." >&2
  exit 1
fi

echo "==> installing to $dest"
rm -rf "$dest"
cp -R "$app" "$dest"

echo "==> installed $("$dest/Contents/MacOS/envelope" --version 2>/dev/null | tail -1)"
echo "    open -a Envelope"
