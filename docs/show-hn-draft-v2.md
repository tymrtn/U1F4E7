# Show HN Draft — Envelope Email (v2)

## Title (80 chars max)

```
Show HN: Envelope – run multiple AI agents on one shared mailbox, attributed
```

(78 chars)

## Post Body (URL field)

```
https://github.com/tymrtn/U1F4E7
```

## Comment (post this as the first comment)

I built Envelope because I wanted to run more than one AI agent against the same mailbox — a main assistant plus a specialized triage bot, say — without giving them the same credentials or losing track of which one did what.

**The core idea: agent identities, not one shared token.**

```bash
envelope agent create skippy
envelope agent create triage-bot
```

Each agent gets its own bearer token (printed once, hashed at rest) and its own policy:

```bash
envelope agent policy set skippy \
  --allow-accounts you@example.com \
  --allow-actions inbox.read,draft.create \
  --send-mode-ceiling draft-only

envelope agent policy set triage-bot \
  --allow-accounts you@example.com \
  --allow-actions inbox.read,message.flag,message.move \
  --send-mode-ceiling draft-only
```

Send-mode ceilings run `draft-only` → `confirm-send` → `allowlisted-send` → `autonomous-send`. Under `draft-only`, an agent can never push mail out directly — `draft create` plus human approval is the only path. Every action either agent takes is attributed and auditable:

```bash
envelope actions tail --agent skippy
envelope actions tail --agent triage-bot
```

Revoke one without touching the other:

```bash
envelope agent revoke triage-bot
```

Free tier covers 2 agent identities per account; beyond that, `envelope license activate env-lic-<key>`.

**Under the hood, it's still just a mailbox client.** Add an account and Envelope auto-discovers your IMAP/SMTP servers via DNS (SRV records → MX lookup → common host patterns) — no config file:

```bash
envelope accounts add --email you@gmail.com --password <app-password>
```

Every command supports `--json`:

```bash
envelope inbox --json | jq '.[0].subject'
envelope search "FROM boss@co.com SINCE 01-Mar-2026" --json
```

It also runs as an MCP server (22 tools — inbox, search, send, reply, draft management, rules, snooze, and the agent/policy surface above) that Claude Code, Cursor, or Zed can attach to directly:

```bash
envelope mcp --config
```

**Why not mutt/himalaya/aerc?** All three assume one human at one keyboard. None of them have a concept of "more than one caller with different permissions on the same account" — that's the gap Envelope is built around. mutt/neomutt is a TUI you script by screen-scraping. himalaya is closer in spirit (great project) but is single-caller and config-file-first. aerc is interactive-first. None model multi-agent attribution because none were built for it.

**Technical details:**

- Rust, 4 crates: `cli`, `email`, `store`, `dashboard`
- Credentials: AES-256-GCM encrypted file by default (`~/.config/envelope-email/credentials.json`, mode 0600); optional OS keychain backend (macOS Keychain, Linux Secret Service via `--credential-store keychain`)
- State in local SQLite (`envelope paths` shows the exact location for your platform)
- Sends queue into an outbox with a cooldown (default 120s) and go out via a scheduled-send sweep after a Governor attribution gate; immediate transmission requires explicit `--send-now --confirm-send-now`
- Localhost dashboard via Axum (`envelope serve`, Agent Cockpit for reviewing/approving drafts)
- Install: `brew install tymrtn/u1f4e7/u1f4e7` (macOS) or `cargo install --git https://github.com/tymrtn/U1F4E7 --bin envelope` (from source)

**What it's not:** Envelope doesn't do content scoring or spam classification — that's a separate layer it hooks into (Governor). Envelope's job is identity, policy, and attribution on top of a plain IMAP/SMTP client.

License is FSL-1.1-ALv2 — each release converts to Apache 2.0 two years after it ships.

I'd love feedback on the per-agent policy model and whether the send-mode-ceiling framing (draft-only → autonomous-send) makes sense to people running their own agent fleets. Happy to answer questions.
