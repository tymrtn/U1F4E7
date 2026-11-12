#!/usr/bin/env bash
# Build the workspace `envelope` binary and stage it as the Tauri sidecar.
#
# Tauri resolves externalBin entries by appending the target triple, so the file
# must be named `envelope-<triple>`. At bundle time Tauri copies it to
# Contents/MacOS/envelope, which is where the supervisor's Sidecar resolution
# looks. Without this the bundle has no server to launch.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
desktop_dir="$(dirname "$here")"
repo_root="$(cd "$desktop_dir/../.." && pwd)"
triple="${TARGET_TRIPLE:-$(rustc -vV | sed -n 's/^host: //p')}"
dest_dir="$desktop_dir/src-tauri/binaries"

echo "==> building envelope (release) for $triple"
# Build for the triple the file is named after. Without --target this produced a
# host binary under a cross-named filename, and the --version check below could
# not catch it because the wrong binary still runs on the host.
cargo build --release --bin envelope --target "$triple" --manifest-path "$repo_root/Cargo.toml"

mkdir -p "$dest_dir"
# Replace rather than overwrite: the running app may hold the old inode.
rm -f "$dest_dir/envelope-$triple"
cp "$repo_root/target/$triple/release/envelope" "$dest_dir/envelope-$triple"
chmod +x "$dest_dir/envelope-$triple"

echo "==> staged $dest_dir/envelope-$triple"
"$dest_dir/envelope-$triple" --version
