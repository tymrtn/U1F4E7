#!/usr/bin/env bash
# ci/check-glibc-floor.sh — fail when a Linux release binary needs a newer
# glibc than the oldest one we support.
#
#   ci/check-glibc-floor.sh <binary> [floor]
#
# The floor defaults to 2.31 (Debian 11, Ubuntu 20.04). Weak symbol versions
# count too: the loader prints a warning for each one it can't find, on every
# start, and agents read that stderr.
#
# Rationale: a native `cargo build` on ubuntu-latest links against the runner's
# glibc. v1.3.18's x86_64 binary needed 2.34 to start at all and 2.39 weakly.

set -euo pipefail

bin="${1:?usage: ci/check-glibc-floor.sh <binary> [floor]}"
floor="${2:-2.31}"

versions="$(objdump -T "$bin" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' | sort -uV)"
newest="$(printf '%s\n' "$versions" | tail -1)"
if [[ -z "$newest" ]]; then
    echo "error: no GLIBC symbol versions found in $bin; is it a glibc-linked ELF binary?" >&2
    exit 1
fi

if [[ "$(printf '%s\n%s\n' "$floor" "$newest" | sort -V | tail -1)" != "$floor" ]]; then
    echo "error: $bin needs glibc $newest; the supported floor is $floor." >&2
    echo "Symbols above the floor:" >&2
    objdump -T "$bin" | awk -v floor="$floor" '
        match($0, /GLIBC_[0-9][0-9.]*/) {
            v = substr($0, RSTART + 6, RLENGTH - 6)
            split(v, a, "."); split(floor, f, ".")
            if (a[1] > f[1] || (a[1] == f[1] && a[2] + 0 > f[2] + 0)) print "  " $NF " (GLIBC_" v ")"
        }' >&2
    exit 1
fi

echo "$bin needs glibc $newest (floor $floor)"
