# r/selfhosted Post Draft — Envelope Email (v2)

## Title

```
Envelope – BYO-mailbox CLI email client in Rust. No cloud, no accounts to create, your IMAP server is the only backend.
```

## Body

I've been self-hosting email for years and got tired of every email tool wanting me to sign up for something, sync to their cloud, or install an Electron app that uses 800MB of RAM to show me a list of subjects.

So I built **Envelope** — a Rust CLI email client where your mailbox IS the backend. IMAP for reading, SMTP for sending. That's the entire architecture. There's no intermediary service, no cloud sync, no telemetry, no account creation.

### How it works

```bash
# Install
brew install tymrtn/tap/envelope-email

# Add your mailbox (auto-discovers IMAP/SMTP via DNS)
envelope-email accounts add --email you@yourdomain.com

# Done. Read your mail.
envelope-email inbox
envelope-email read 42
envelope-email search "FROM someone@example.com" --json
```

That `accounts add` step does DNS auto-discovery (SRV records → MX → common patterns) to find your IMAP and SMTP hosts. If you're running Dovecot/Postfix or Mailcow, it'll find them. If auto-discovery fails (custom ports, unusual setup), you can pass `--imap-host` and `--smtp-host` explicitly.

### What stays local

- **Credentials** → OS keychain (macOS Keychain, Linux Secret Service). Not a dotfile.
- **Account metadata + drafts** → SQLite at `~/.config/envelope-email/`
- **Email content** → stays on your IMAP server. Envelope doesn't cache or duplicate it.
- **No phone-home, no analytics, no update checks.**

### Why I built it

I run an AI agent (OpenClaw) that needs to read and send email programmatically. Every command in Envelope supports `--json`, so it pipes cleanly into `jq`, scripts, or agent frameworks. The draft workflow (`draft create` → human reviews → `draft send`) was designed specifically for the pattern where an AI composes but a human approves.

But you don't need an AI agent to use it. If you want a fast, scriptable way to interact with your self-hosted mailbox from the terminal, that's the core use case.

### Features

- Multiple accounts with `--account` switching
- Full message management: move, copy, delete, flag
- Attachment listing and download
- IMAP search passthrough (all standard IMAP search operators)
- Localhost web dashboard (`envelope-email serve`)
- Draft management with create/list/send/discard
- Single static binary, ~5MB

### What it's NOT

Envelope is not a mail server. It's not a webmail interface. It doesn't replace Mailcow or Mail-in-a-Box. It's a **client** — it talks to whatever IMAP/SMTP server you already run.

**Repo:** https://github.com/tymrtn/envelope-email-rs
**Install:** `brew install tymrtn/tap/envelope-email` or `cargo install envelope-email`
**License:** FSL-1.1-ALv2 (converts to Apache 2.0 after 2 years)

Would love to hear from anyone running Dovecot/Postfix, Stalwart, Mailcow, or similar — curious if auto-discovery works cleanly with your setup.
