#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
CONFIG_DIR="$HOME/.config/envelope-email"
ENGINE_ENV="$CONFIG_DIR/engine.env"

mkdir -p "$CONFIG_DIR"
if [[ ! -e "$ENGINE_ENV" ]]; then
  umask 077
  {
    printf '%s\n' '# Envelope engine secrets and runtime settings. Keep this file mode 600.'
    printf '%s\n' 'OPENROUTER_API_KEY='
    printf 'ENVELOPE_MASTER_PASSPHRASE_FILE=%s\n' "$CONFIG_DIR/passphrase"
  } > "$ENGINE_ENV"
fi
chmod 600 "$ENGINE_ENV"

case "$(uname -s)" in
  Linux)
    UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
    mkdir -p "$UNIT_DIR"
    install -m 0644 "$SCRIPT_DIR/systemd/envelope-engine.service" "$UNIT_DIR/envelope-engine.service"
    printf '%s\n' "Installed $UNIT_DIR/envelope-engine.service"
    printf '%s\n' "1. Add OPENROUTER_API_KEY to $ENGINE_ENV"
    printf '%s\n' "2. Ensure $CONFIG_DIR/passphrase exists with mode 600"
    printf '%s\n' '3. Enable explicitly: systemctl --user daemon-reload && systemctl --user enable --now envelope-engine.service'
    ;;
  Darwin)
    LIBEXEC_DIR="$HOME/.local/libexec"
    AGENT_DIR="$HOME/Library/LaunchAgents"
    mkdir -p "$LIBEXEC_DIR" "$AGENT_DIR"
    install -m 0755 "$SCRIPT_DIR/launchd/envelope-engine-run" "$LIBEXEC_DIR/envelope-engine-run"
    install -m 0644 "$SCRIPT_DIR/launchd/com.tymrtn.envelope-engine.plist" "$AGENT_DIR/com.tymrtn.envelope-engine.plist"
    printf '%s\n' "Installed $AGENT_DIR/com.tymrtn.envelope-engine.plist"
    printf '%s\n' "1. Add OPENROUTER_API_KEY to $ENGINE_ENV"
    printf '%s\n' "2. Ensure $CONFIG_DIR/passphrase exists with mode 600"
    printf '%s\n' '3. Enable explicitly: launchctl bootstrap "gui/$UID" "$HOME/Library/LaunchAgents/com.tymrtn.envelope-engine.plist"'
    ;;
  *)
    printf '%s\n' 'Unsupported platform: install `envelope engine run --interval-seconds 300 --deliver --json` with your service manager.' >&2
    exit 64
    ;;
esac

printf '%s\n' 'The service was installed but not enabled. Automatic junk moves are not enabled by this service.'
