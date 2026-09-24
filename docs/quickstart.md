# Envelope quickstart — the 10-minute path

This guide covers macOS and Linux from scratch to a working inbox and wired
MCP server.

---

## Before you start: which mailboxes work

Envelope signs in to IMAP and SMTP with a password or an app password. It has
no OAuth sign-in yet.

| Provider | What to use |
|---|---|
| **Fastmail** | App password: [app.fastmail.com/settings/security/devicekeys](https://app.fastmail.com/settings/security/devicekeys) |
| **iCloud Mail** | App-specific password: [appleid.apple.com](https://appleid.apple.com) → Sign-In and Security → App-Specific Passwords |
| **Gmail** | App password: [myaccount.google.com/apppasswords](https://myaccount.google.com/apppasswords). Google requires 2-Step Verification first, and [some accounts cannot create one](https://support.google.com/accounts/answer/185833). If Google won't let you create an app password for your account, Gmail can't connect to Envelope yet (OAuth sign-in isn't supported). |
| **Migadu / self-hosted** | Your regular mailbox password |
| **Outlook.com, Hotmail, Live, Microsoft 365** | Not supported yet. Microsoft turned off password sign-in for IMAP on [Outlook.com](https://support.microsoft.com/en-us/office/modern-authentication-methods-now-needed-to-continue-syncing-outlook-email-in-non-microsoft-email-apps-c5d65390-9676-4763-b41f-d7986499a90d) and [Exchange Online](https://learn.microsoft.com/en-us/exchange/clients-and-mobile-in-exchange-online/deprecation-of-basic-authentication-exchange-online), and Envelope cannot do the OAuth sign-in Microsoft requires. |

---

## Install

### macOS or Linux (release binary)

```bash
curl -fsSL https://raw.githubusercontent.com/tymrtn/U1F4E7/main/install.sh | bash
```

The script picks the release tarball for your OS and CPU, checks it against the
release's `.sha256` file, installs `envelope` to `~/.local/bin` without sudo,
and tells you if that directory is missing from your `PATH`. Pin a version with
`bash -s -- --version v1.3.2`.

### macOS (Homebrew)

```bash
brew install tymrtn/u1f4e7/envelope
which envelope   # verify it's on PATH
```

The formula builds from source, so Homebrew installs Rust as a build
dependency and the first install takes a few minutes.

### Build from source

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
git clone https://github.com/tymrtn/U1F4E7
cd U1F4E7
cargo build --release
install -m 755 target/release/envelope ~/.local/bin/envelope
```

---

## Add an account

Envelope encrypts saved credentials with a passphrase. Run interactively and it
asks for one (and asks again on later commands):

```bash
envelope accounts add --email you@fastmail.com
```

Scripts, agents, and headless servers have no terminal to ask from. Give them a
passphrase file readable only by you, then pipe the password in:

```bash
(umask 077 && openssl rand -base64 32 > "$HOME/.envelope-passphrase")
export ENVELOPE_MASTER_PASSPHRASE_FILE="$HOME/.envelope-passphrase"   # add to your shell profile
printf '%s\n' "$APP_PASSWORD" | envelope accounts add --email you@fastmail.com --password-stdin
```

Back up the passphrase file. Without it the saved credentials cannot be
decrypted and you would have to add the accounts again.

Envelope auto-discovers IMAP and SMTP servers from the email domain via DNS.
If discovery fails, it falls back to `imap.<domain>:993` and `smtp.<domain>:587`.

Before saving, `accounts add` logs in to IMAP. A rejected login exits non-zero,
saves nothing, and prints provider-specific steps. For offline setup, pass
`--skip-login-check` to save without logging in.

---

## Verify the setup

```bash
envelope quickstart
```

Runs 4 phases: paths → account → IMAP auth → inbox peek.

- Exit 0: everything works. Next steps are printed.
- Exit 2: no account configured yet — run `accounts add` first.
- Exit 3: IMAP auth failed — check the `remediation` field for provider-specific
  steps. In JSON mode: `envelope quickstart --json | jq '.phases[] | select(.error)'`

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
