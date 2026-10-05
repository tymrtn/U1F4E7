#!/bin/sh
# MCP Bundle entry point (server/envelope inside the .mcpb). The bundle holds
# one envelope binary per release target, and MCPB platform overrides key on
# the OS only, so this picks the binary for this OS and CPU and execs it.
set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
os=$(uname -s)
cpu=$(uname -m)

case "$os/$cpu" in
    Darwin/arm64) target=aarch64-apple-darwin ;;
    Darwin/x86_64) target=x86_64-apple-darwin ;;
    Linux/x86_64 | Linux/amd64) target=x86_64-unknown-linux-gnu ;;
    Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-gnu ;;
    *)
        echo "envelope: this bundle has no binary for $os $cpu (it ships macOS arm64/x86_64 and Linux x86_64/aarch64)." >&2
        echo "envelope: install Envelope another way: https://github.com/tymrtn/U1F4E7#install" >&2
        exit 1
        ;;
esac

bin="$here/bin/$target/envelope"
if [ ! -x "$bin" ]; then
    echo "envelope: bundled binary for $target is missing or not executable: $bin" >&2
    exit 1
fi

exec "$bin" "$@"
