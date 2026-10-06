#!/bin/sh
# Starts `envelope mcp` inside the container.
#
# With ENVELOPE_AGENT_TOKEN set, it runs the server as given: mount a data
# directory at $ENVELOPE_HOME and pass the token of an agent created there.
#
# Without a token, it only starts when the container has no Envelope database
# yet. It then creates a throwaway agent in that empty store, so an MCP client
# or a directory check can start the server and list its tools. The store has
# no mail accounts, so the token reaches no mail, and it disappears with the
# container. A database that already exists always needs its own token.
set -eu

home="${ENVELOPE_HOME:?ENVELOPE_HOME must be set}"
db="$home/envelope-email/envelope.db"

if [ -z "${ENVELOPE_AGENT_TOKEN:-}" ]; then
    if [ -e "$db" ]; then
        echo "envelope: $db already exists; set ENVELOPE_AGENT_TOKEN to the token of an agent in it." >&2
        exit 1
    fi
    mkdir -p "$home"
    chmod 700 "$home"
    passphrase="$home/.passphrase"
    (umask 077 && od -An -tx1 -N32 /dev/urandom | tr -d ' \n' > "$passphrase")
    export ENVELOPE_MASTER_PASSPHRASE_FILE="$passphrase"
    created=$(envelope agent create container --credential-store file --json)
    ENVELOPE_AGENT_TOKEN=$(printf '%s' "$created" | sed -n 's/.*"token"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
    if [ -z "$ENVELOPE_AGENT_TOKEN" ]; then
        echo "envelope: could not read the token from 'envelope agent create'." >&2
        exit 1
    fi
    export ENVELOPE_AGENT_TOKEN
    echo "envelope: no token given; started with a throwaway agent in an empty store (no mail accounts)." >&2
fi

exec envelope mcp "$@"
