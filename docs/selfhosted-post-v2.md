# r/selfhosted Post Draft — Envelope Email (v2)

## Title

```
Envelope – Rust CLI mailbox client where multiple AI agents share one inbox with per-agent attribution. No cloud, IMAP/SMTP only.
```

## Body

I've been self-hosting email for years and got tired of every email tool wanting me to sign up for something, sync to their cloud, or install an Electron app that uses 800MB of RAM to show me a list of subjects.

So I built **Envelope** — a Rust CLI email client where your mailbox IS the backend. IMAP for reading, SMTP for sending. No intermediary service, no cloud sync, no telemetry, no account creation.

### The reason I built it: I run more than one agent against my inbox

I run an assistant agent and a separate triage bot against the same mailbox. They shouldn't have the same permissions, and I want to know which one did what. Envelope gives each agent its own identity:

```bash
envelope agent create skippy
envelope agent create triage-bot
# Each prints a bearer token once — it's hashed at rest, never shown again.
```

...and its own policy, scoped to specific accounts, actions, and a send-mode ceiling:

```bash
envelope agent policy set skippy \
  --allow-accounts you@yourdomain.com \
  --allow-actions inbox.read,draft.create \
  --send-mode-ceiling draft-only

envelope agent policy set triage-bot \
  --allow-accounts you@yourdomain.com \
  --allow-actions inbox.read,message.flag,message.move \
  --send-mode-ceiling draft-only
```

Ceilings run `draft-only` → `confirm-send` → `allowlisted-send` → `autonomous-send`. Under `draft-only`, an agent can compose but never transmit — a human has to approve and run `envelope draft send <id>` (or use the dashboard) before anything leaves. Every action either agent takes is attributed and auditable per agent:

```bash
envelope actions tail --agent skippy
envelope actions tail --agent triage-bot
```

Revoke one agent's access without touching the other:

```bash
envelope agent revoke triage-bot
```

Free tier covers 2 agent identities per mailbox account. Beyond that it's `envelope license activate env-lic-<key>` (see licensing in the repo README).

### How the base client works

```bash
# Install
brew install tymrtn/u1f4e7/u1f4e7

# Add your mailbox (auto-discovers IMAP/SMTP via DNS)
envelope accounts add --email you@yourdomain.com --password <password>

# Done. Read your mail.
envelope inbox
envelope read 42
envelope search "FROM someone@example.com" --json
```

That `accounts add` step does DNS auto-discovery (SRV records → MX → common patterns) to find your IMAP and SMTP hosts. If you're running Dovecot/Postfix or Mailcow, it should find them. If auto-discovery fails (custom ports, unusual setup), pass `--imap-host` / `--smtp-host` explicitly.

### What stays local

- **Credentials** → AES-256-GCM encrypted file by default (`~/.config/envelope-email/credentials.json`, mode 0600). Optional OS keychain backend on macOS (Keychain) or Linux desktop (Secret Service via GNOME Keyring/KWallet) with `--credential-store keychain`.
- **Account metadata, drafts, agent policies, action log** → local SQLite (`envelope paths` shows exact locations).
- **Email content** → stays on your IMAP server. Envelope doesn't cache or duplicate message bodies.
- **No phone-home, no analytics, no update checks.**

### Send safety

Outbound mail doesn't go straight out. A send queues into an outbox with a cooldown (default 60s) and transmits later via a scheduled-send sweep, gated by attribution scoring. Immediate transmission requires explicit `--send-now --confirm-send-now`. Agent contexts (MCP) default to `draft-only` regardless of what the account's own CLI default is.

### Features

- Multi-agent identities with per-agent policy and audit trail (above)
- Multiple mailbox accounts with `--account` switching
- Full message management: move, copy, delete, flag
- Attachment listing and download
- IMAP search passthrough (standard IMAP search syntax)
- Localhost web dashboard (`envelope serve`, includes an Agent Cockpit for reviewing/approving drafts)
- Draft management: create/list/send/discard
- Rules engine (`envelope rule create`) and snooze (`envelope snooze set`)
- MCP server with 22 tools for Claude Code / Cursor / Zed (`envelope mcp --config`)
- Single static binary (~13MB on aarch64-apple-darwin)

### What it's NOT

Envelope is not a mail server. It's not a webmail interface. It doesn't replace Mailcow or Mail-in-a-Box. It's a **client** — it talks to whatever IMAP/SMTP server you already run.

**Repo:** [https://github.com/tymrtn/U1F4E7](https://github.com/tymrtn/U1F4E7)
**Install (macOS):** `brew install tymrtn/u1f4e7/u1f4e7`
**Install (Linux):** `curl -fsSL https://raw.githubusercontent.com/tymrtn/U1F4E7/main/dist/install.sh | bash` (downloads a release tarball, verifies the sha256 checksum, no sudo) — or build from source, see [docs/install-linux.md](https://github.com/tymrtn/U1F4E7/blob/main/docs/install-linux.md)
**License:** FSL-1.1-ALv2 (each release converts to Apache 2.0 two years after it ships)

Would love to hear from anyone running Dovecot/Postfix, Stalwart, Mailcow, or similar — curious if auto-discovery works cleanly with your setup, and whether the multi-agent policy model maps to how you're running agents against your own mail.
