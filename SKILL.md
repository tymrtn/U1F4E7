---
name: envelope-email
description: "BYO-mailbox CLI email client for IMAP/SMTP. Use `envelope-email` to read, send, search, organize, and draft emails from any standard mailbox. Supports multiple accounts with auto-discovery, JSON output on every command, and a localhost dashboard. Replaces Himalaya for OpenClaw agents."
homepage: https://github.com/tymrtn/envelope-email
metadata:
  {
    "openclaw":
      {
        "emoji": "📨",
        "requires": { "bins": ["envelope-email"] },
        "install":
          [
            {
              "id": "brew",
              "kind": "brew",
              "formula": "tymrtn/tap/envelope-email",
              "bins": ["envelope-email"],
              "label": "Install Envelope Email (brew)",
            },
            {
              "id": "cargo",
              "kind": "shell",
              "command": "cargo install envelope-email",
              "bins": ["envelope-email"],
              "label": "Install Envelope Email (cargo)",
            },
          ],
      },
  }
---

# Envelope Email CLI

BYO-mailbox email client with agent-native primitives. Turn any IMAP/SMTP account into a programmable email interface with JSON output on every command.

## When to Use

✅ **USE this skill when:**

- Reading, sending, or searching email
- Managing multiple email accounts
- Organizing messages (move, copy, delete, flag)
- Creating and managing drafts for human review
- Downloading attachments
- Checking inbox status during heartbeats

## When NOT to Use

❌ **DON'T use this skill when:**

- User asks to send a WhatsApp message → use `wacli` skill
- User needs calendar/scheduling → use a calendar tool
- Task requires rendering HTML email in a browser → open the dashboard with `serve`

## Account Setup

```bash
# Add account (auto-discovers IMAP/SMTP hosts via DNS)
envelope-email accounts add --email you@gmail.com --password <app-password>

# Add with explicit server settings (if auto-discovery fails)
envelope-email accounts add --email you@custom.com --password <pass> \
  --imap-host imap.custom.com --imap-port 993 \
  --smtp-host smtp.custom.com --smtp-port 587

# List configured accounts
envelope-email accounts list --json

# Remove an account
envelope-email accounts remove <id-or-email>
```

**Note:** Passwords are stored in the OS keychain (macOS Keychain, etc.), not in plaintext. For Gmail, use an App Password — not your main password.

## Reading Email

### List Inbox

```bash
# Default inbox (25 most recent)
envelope-email inbox --json

# Specific folder and limit
envelope-email inbox --folder "Sent" --limit 50 --json

# Specific account
envelope-email inbox --account work@example.com --json
```

### Read a Message

```bash
# Read by UID
envelope-email read 42 --json

# From a specific folder
envelope-email read 42 --folder "Archive" --json
```

### Search Messages

Uses IMAP search syntax:

```bash
# Search by sender
envelope-email search "FROM john@example.com" --json

# Search by subject
envelope-email search "SUBJECT invoice" --json

# Combine criteria
envelope-email search "FROM john@co.com SINCE 01-Mar-2026" --limit 10 --json

# Search in a specific folder
envelope-email search "UNSEEN" --folder "INBOX" --json
```

### List Folders

```bash
envelope-email folders --json
envelope-email folders --account work@example.com --json
```

## Sending Email

```bash
# Simple send
envelope-email send --to someone@example.com --subject "Hello" --body "Hi there"

# With CC, BCC, and reply-to
envelope-email send --to someone@example.com --subject "Meeting" \
  --body "See you at 3pm" --cc boss@example.com --bcc archive@example.com \
  --reply-to noreply@example.com

# HTML body
envelope-email send --to someone@example.com --subject "Update" \
  --html "<h1>Status Report</h1><p>All good.</p>"

# From a specific account
envelope-email send --to someone@example.com --subject "Hello" \
  --body "Sent from work" --account work@example.com --json
```

## Draft Management

Drafts are stored locally in SQLite. Create a draft, let the human review, then send or discard.

```bash
# Create a draft
envelope-email draft create --to someone@example.com \
  --subject "Proposal" --body "Draft body here" --json

# List all drafts
envelope-email draft list --json

# Send a draft (delivers via SMTP)
envelope-email draft send <draft-id> --json

# Discard a draft
envelope-email draft discard <draft-id> --json
```

**Agent pattern:** When composing email on behalf of a user, always create a draft first. Only call `draft send` after explicit human approval.

## Agent Safety Model (read before sending)

Envelope is the **single canonical mail control plane** for agentic email work. It replaces Himalaya and any raw IMAP/SMTP client for reading, searching, threading, drafting, sending, organizing, and rules. Other IMAP/SMTP tools, if used at all, are **read-only diagnostics only** — never a peer mail client, never a draft or send path.

Binding rules:

- **Drafts only via `envelope-email draft create`.** Never write a loose `.eml` file as a draft. `.eml` is allowed only for archive/evidence/export artifacts — never as a draft substitute.
- **Inspect the thread before drafting a reply.** Use `envelope-email read`/`thread` (or `draft reply`) so the reply has real context.
- **Never send without explicit approval.** Agent/MCP contexts default to `--send-mode draft-only`. Escalation modes (`confirm-send`, `allowlisted-send`, `autonomous-send`) require an explicitly approved automation policy; do not assume one exists.
- **Discover accounts; never hard-code them.** Use `envelope-email accounts list --json` and pass explicit `--account`/`--from`. Do not treat any one personal account as a universal sender/CC fallback — CC must be account/profile-scoped. If no approved CC exists for the active sending account, stop and ask rather than guessing.
- **Use `--json` whenever available.**

## Message Management

### Move / Copy / Delete

```bash
# Move message to a folder
envelope-email move 42 --to-folder "Archive" --folder INBOX

# Copy message to a folder
envelope-email copy 42 --to-folder "Important" --folder INBOX

# Delete a message
envelope-email delete 42 --folder INBOX
```

### Flags

```bash
# Mark as read
envelope-email flag add 42 "\\Seen"

# Star / flag a message
envelope-email flag add 42 "\\Flagged"

# Remove a flag
envelope-email flag remove 42 "\\Seen"

# With folder and account
envelope-email flag add 42 "\\Answered" --folder "Sent" --account work@example.com
```

Common IMAP flags: `\\Seen`, `\\Flagged`, `\\Answered`, `\\Draft`, `\\Deleted`

## Attachments

```bash
# List attachments on a message
envelope-email attachment list 42 --json

# Download a specific attachment
envelope-email attachment download 42 "report.pdf" --output ~/Downloads/report.pdf

# From a specific folder/account
envelope-email attachment download 42 "photo.jpg" \
  --folder "Sent" --account personal@gmail.com
```

## Dashboard

```bash
# Start localhost web UI (default port 3141)
envelope-email serve

# Custom port
envelope-email serve --port 8080

# Expose to agents/devices on your tailnet — configure auth FIRST
envelope-email config set dashboard.tailscale_allow "you@your-tailnet.ts.net"   # humans, no token
envelope-email config set dashboard.auth_token "$(openssl rand -hex 32)"        # agents/scripts (Bearer)
tailscale serve --bg 3141
envelope-email config set dashboard.base_url https://your-node.your-tailnet.ts.net
```

Opens an account management dashboard at `http://localhost:3141`. JSON `ui`
metadata uses `ENVELOPE_DASHBOARD_BASE_URL` first, then the legacy
`ENVELOPE_DASHBOARD_URL`, then persistent `dashboard.base_url`, then localhost.
Use Tailscale Serve for tailnet-only handoff URLs. Tailscale Funnel publishes
the dashboard on the public internet; do not use Funnel for mailbox dashboards
unless that exposure is intentional.

Loopback is unauthenticated (single-user local trust). Before exposing the
dashboard beyond loopback, set `dashboard.auth_token` (Bearer, for agents) or
`dashboard.tailscale_allow` (tailnet identities, for humans). With either set,
every `/api` route requires the credential, and `serve --bind <non-loopback>`
refuses to start without one.

## Account Resolution

- If `--account` is omitted, the first-added account is used as default.
- Specify by numeric ID or email address: `--account 2` or `--account work@example.com`
- Use `accounts list --json` to see all configured accounts with their IDs.

## JSON Output

Every command supports the global `--json` flag for structured, machine-readable output:

```bash
# Pipe to jq for field extraction
envelope-email inbox --json | jq '.[0].subject'

# Check for unread count
envelope-email search "UNSEEN" --json | jq 'length'
```

**Always use `--json` when calling from agents or scripts.** The default human-readable table format is for interactive terminal use only.

## Tips

- **IMAP search syntax** — Envelope passes search queries directly to the IMAP server. Common operators: `FROM`, `TO`, `SUBJECT`, `BODY`, `SINCE`, `BEFORE`, `UNSEEN`, `SEEN`, `FLAGGED`. Combine freely.
- **App Passwords** — Gmail, Outlook, and other providers with 2FA require app-specific passwords, not your main login.
- **UIDs are folder-scoped** — A message UID is only valid within its folder. Always use the same `--folder` when referencing a UID from a previous listing.
- **Data storage** — Account metadata and drafts are stored in SQLite at `~/.config/envelope-email/`. Passwords go to the OS keychain.
