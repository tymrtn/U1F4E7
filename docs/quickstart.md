# Envelope quickstart — the 10-minute path

This guide covers macOS (Homebrew) and Linux (source build) from scratch to a
working inbox and wired MCP server.

---

## Before you start — get an app password

Most major providers reject your account login password for IMAP access. Generate
an app password before running `accounts add`:

| Provider | Where |
|---|---|
| **Gmail** | [myaccount.google.com/apppasswords](https://myaccount.google.com/apppasswords) (2FA required) |
| **Fastmail** | [app.fastmail.com/settings/security/devicekeys](https://app.fastmail.com/settings/security/devicekeys) |
| **iCloud** | [appleid.apple.com](https://appleid.apple.com) → Sign-In and Security → App-Specific Passwords |
| **Outlook/Hotmail** | [account.microsoft.com/security](https://account.microsoft.com/security) → Advanced security → App passwords |
| **Migadu / self-hosted** | Use your regular password |

---

## Install

### macOS

```bash
brew install tymrtn/u1f4e7/u1f4e7
# Installs the binary named `envelope`
which envelope   # verify it's on PATH
```

### Linux / VPS

```bash
# 1. Install Rust if not already installed
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# 2. Clone and build
git clone https://github.com/tymrtn/U1F4E7
cd U1F4E7
cargo build --release

# 3. Install the binary
install -m 755 target/release/envelope ~/.local/bin/envelope
# Make sure ~/.local/bin is on PATH
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc && source ~/.bashrc
```

---

## Add an account

```bash
# Interactive (prompts for password on stderr — do not pipe when using this form)
envelope accounts add --email you@gmail.com

# Non-interactive (CI, scripts, headless VPS — opt into stdin explicitly)
printf '%s\n' "$APP_PASSWORD" | envelope accounts add --email you@gmail.com --password-stdin
```

Envelope auto-discovers IMAP and SMTP servers from the email domain via DNS.
If discovery fails, it falls back to `imap.<domain>:993` and `smtp.<domain>:587`.

---

## Verify the setup

```bash
envelope quickstart
```

Runs 4 phases: paths → account → IMAP auth → inbox peek.

- Exit 0: everything works. Next steps are printed.
- Exit 2: no account configured yet — run `accounts add` first.
- Exit 3: IMAP auth failed — check the `remediation` field for provider-specific
  app-password URLs. In JSON mode: `envelope quickstart --json | jq '.phases[] | select(.error)'`

---

## Read your inbox

```bash
envelope inbox --limit 20

# JSON output for scripting
envelope inbox --limit 10 --json | jq '.[0].subject'

# Read a specific message (does not mark it as read)
envelope read 42
```

---

## Wire MCP into Claude Code

```bash
# Print the ready-to-paste config snippet
envelope mcp --config

# The output includes a claudeCode.snippet field. Run it directly:
envelope mcp --config --json | jq -r '.envelopeAgentSetup.claudeCode.snippet' | sh

# Or paste manually into Claude Code:
claude mcp add-json envelope '{"command":"/path/to/envelope","args":["mcp"],"env":{"HOME":"/your/home"}}'
```

Verify the MCP server starts:

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | envelope mcp
# Should return a list of 22 tools
```

---

## What's next

```bash
# Watch for new mail in real time (IMAP IDLE)
envelope watch --json

# Extract a verification code
CODE=$(envelope code --wait 60)

# Open the local dashboard
envelope serve   # http://localhost:3141

# See all available commands
envelope --help

# Get help for any subcommand
envelope accounts --help
envelope draft --help
```

For multiple agents sharing one inbox, see [docs/agent-fleet-shared-inbox.md](agent-fleet-shared-inbox.md).

For credential backend options (passphrase file, Keychain, Secret Service), see
[docs/credential-backends.md](credential-backends.md).

For Linux VPS setup with systemd, see [docs/install-linux.md](install-linux.md).
