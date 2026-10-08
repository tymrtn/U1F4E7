#!/bin/sh
# Stand-in for the Governor CLI in governor_gate.rs. Each test symlinks to this
# script; the link's "<link>.verdict" file holds the canned JSON to print, and
# the invocation's arguments are recorded to "<link>.argv".
printf '%s\n' "$*" > "$0.argv"
cat "$0.verdict"
