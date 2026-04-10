# Envelope Email

**BYO mailbox email client with agent-native primitives.** Turn any
IMAP/SMTP account into a programmable email interface your agents can
drive. Give it an email + password, it figures out the rest.

Licensed under [FSL-1.1-ALv2](LICENSE).

## Why

- **One command**: `envelope accounts add --email you@example.com --password ...` — no manual host/port/TLS wrangling.
- **Auto-discovery**: SRV → MX → common patterns → TCP probe. Knows Gmail, Outlook/Office 365, Microsoft Workmail, Migadu, Fastmail, self-hosted Dovecot, generic IMAP.
- **Agent-native**: every command supports `--json`. Pipe to `jq`, feed to an LLM, whatever.
- **Batteries included**: snooze, threading, reply/reply-all, attachments, drafts, search. Ships with a local dashboard at [http://localhost:3141](http://localhost:3141).
- **Single-purpose**: Envelope does email. Nothing else.

## Install

```bash
# Cargo (installs a binary called `envelope`)
cargo install envelope-email

# Homebrew (macOS/Linux)
brew install tymrtn/tap/envelope

# From source
git clone https://github.com/tymrtn/envelope-email
cd envelope-email
cargo build --release
# binary lands at target/release/envelope
```

> **Note on binary name:** the Cargo package is `envelope-email` (for the
> crates.io slot) but the binary target is `envelope`. After `cargo install`
> you type `envelope …`, not `envelope-email …`. v0.3.0 renamed the binary.

## Quick start

```bash
# Add an account — Envelope auto-discovers IMAP/SMTP from the email domain
envelope accounts add --email you@gmail.com --password <app-password>

# List accounts
envelope accounts list --json | jq '.[] | .username'

# See folders with unread/total counts
envelope folders

# Read the inbox
envelope inbox --limit 20

# Read a single message
envelope read 42

# Send an email with a PDF attachment
envelope send \
  --to recipient@example.com \
  --subject "Q2 report" \
  --body "Attached." \
  --attach ~/reports/q2.pdf

# Snooze a message until Monday morning
envelope snooze set 42 --until monday --reason waiting-reply

# Check for any snoozes that should return now
envelope unsnooze --once

# Open the local dashboard (inbox, read, compose, reply, drafts, snooze)
envelope serve
# → http://localhost:3141
```

## Provider support

Envelope discovers IMAP/SMTP server endpoints via DNS (SRV → MX → common
patterns). Once connected, it detects the provider type from folder
layout and resolves canonical folder names (`drafts`, `sent`, `trash`,
`spam`, `archive`) to the actual IMAP names your provider uses.

| Provider | Auth | Notes |
|---|---|---|
| **Gmail** | App password | Folders use `[Gmail]/` prefix (`[Gmail]/Drafts`, `[Gmail]/Sent Mail`, `[Gmail]/Trash`). |
| **Outlook.com / Office 365** | App password | Exchange IMAP quirks handled (`Deleted Items`, `Junk E-mail`). |
| **Microsoft Workmail** | App password | Same Exchange-style folder names. |
| **Migadu** | Password | Standard folder names (`Drafts`, `Sent`, `Trash`). |
| **Fastmail** | App password | Standard folder names. |
| **Self-hosted Dovecot** | Password | `INBOX.` dot-separator namespace detected automatically. |
| **Generic IMAP** | Password | Anything conforming to RFC 3501. |

OAuth flows (for providers that require them) are not supported in
v0.3.0 — use an app password or provider-specific password. OAuth support
is on the v0.4 roadmap.

## Dashboard

`envelope serve` starts a localhost-only web UI on port 3141.
The dashboard talks to the same IMAP/SMTP code as the CLI.

**Features:**

- Account switcher, stats strip (accounts / snoozed / drafts)
- Folder sidebar with live unread counts
- Inbox list with sender / subject / date columns
- Message reader with sandboxed HTML body rendering
- Reply / Reply-all with automatic header threading (`In-Reply-To`, `References`)
- Compose with text/html toggle and file attachments
- ★ Snoozed virtual folder with overdue highlighting and one-click unsnooze
- IMAP search (any IMAP SEARCH query: `FROM alice`, `SUBJECT invoice`, etc.)

**Security:**

- Binds to `127.0.0.1` only — not reachable from other machines
- CORS locked to `http://localhost:*` / `http://127.0.0.1:*` origins
- HTML email bodies render inside a `<iframe sandbox="">` (no scripts, no same-origin, no forms)
- No authentication — relies on the OS user boundary. Don't `envelope serve` on a shared box.

## CLI reference

| Command | Description |
|---|---|
| `envelope accounts add/list/remove` | Manage accounts (add auto-discovers hosts) |
| `envelope folders [--json]` | List folders with unread/total counts |
| `envelope inbox [--folder INBOX] [--limit N]` | List messages |
| `envelope read <uid> [--folder INBOX]` | Read a single message (uses IMAP BODY.PEEK — does not auto-mark read) |
| `envelope search "<query>" [--folder INBOX]` | IMAP search |
| `envelope send --to <addr> --subject <s> --body <b> [--attach <path>]` | Send |
| `envelope move <uid> --to-folder <name>` | Move a message |
| `envelope copy <uid> --to-folder <name>` | Copy a message |
| `envelope delete <uid>` | Delete a message |
| `envelope flag add/remove <uid> <flag>` | Manage IMAP flags |
| `envelope attachment list/download <uid>` | List or download attachments |
| `envelope draft create/list/send/discard` | Draft management (IMAP-backed) |
| `envelope snooze set <uid> --until <time>` | Move a message to the Snoozed folder and record a return time |
| `envelope snooze list` | List snoozed messages |
| `envelope snooze cancel <uid>` | Unsnooze immediately |
| `envelope unsnooze [--once]` | Sweep the snooze queue and return messages whose time has come |
| `envelope thread show <uid>` | Show the full conversation thread |
| `envelope thread list` | List recent threads |
| `envelope thread build` | Build / refresh the thread index from IMAP |
| `envelope serve [--port 3141]` | Start the localhost dashboard |

### `--until` time format for snooze

- **ISO 8601:** `2026-04-15T09:00:00`
- **Relative:** `2h`, `3d`, `1w`, `30m`
- **Natural:** `tomorrow`, `monday`, `tuesday`, …, `next week`

### JSON output

Every command supports `--json` for agent consumption:

```bash
envelope inbox --json | jq '.[] | select(.subject | contains("invoice"))'
envelope folders --json | jq '.[] | {name, unseen}'
envelope snooze list --json
```

## Credentials

Envelope encrypts stored passwords with AES-256-GCM in
`~/.config/envelope-email/envelope.db`. Two backends:

- **file** (default): master passphrase from `ENVELOPE_MASTER_KEY` env var,
  or a machine-specific seed (hostname + username). Works on headless
  Linux, locked-screen macOS, servers — zero external dependencies.
- **keychain** (opt-in): OS keychain via the `keyring` crate. Enable with
  `--credential-store keychain`. Use for interactive desktop setups.

## Agent integration

Envelope is designed for agentic users. Every command outputs JSON, the
CLI has no interactive prompts (all inputs are flags), and operations
are idempotent where possible. Typical patterns:

```bash
# Check for urgent mail
envelope inbox --json | jq '[.[] | select(.from_addr | contains("@boss.co"))]'

# Auto-snooze low-priority notifications
for uid in $(envelope search "FROM notifications@" --json | jq -r '.[].uid'); do
  envelope snooze set "$uid" --until "+3d" --reason defer
done

# Sweep due snoozes (cron-friendly)
envelope unsnooze --once
```

## Development

```bash
cargo build           # Build all crates
cargo build --release # Optimized release binary
cargo test            # Run the test suite (113 tests, 0 failures)
cargo clippy          # Lint
./ci/check-orphans.sh # Verify every .rs file is reachable via `mod`
```

The repo has four crates:

```
crates/
├── cli/       # The `envelope` binary (clap-based)
├── email/     # IMAP client, SMTP sender, discovery, threading, reply headers
├── store/     # SQLite persistence, AES-GCM credential encryption
└── dashboard/ # Axum localhost web UI with embedded static assets (rust-embed)
```

See [CHANGELOG.md](CHANGELOG.md) for per-release notes.

## License

[FSL-1.1-ALv2](LICENSE) — source-available, non-commercial use allowed,
no competing services. Becomes Apache 2.0 two years after each release.

Copyright © 2026 Tyler Martin.
