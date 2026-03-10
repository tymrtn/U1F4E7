<p align="center">
  <h1 align="center">📧 Envelope</h1>
  <p align="center"><code>U+1F4E7</code> — if you know, you know.</p>
  <p align="center"><strong>Email for agents. Add your credentials, and go.</strong></p>
  <p align="center">BYO mailbox. Human-in-the-loop approval. Production-scale.</p>
</p>

> **Why U1F4E7?** It's the Unicode codepoint for 📧. Humans see a repo name. Agents see an envelope. The ones who get it are the ones this was built for.

<p align="center">
  <a href="#30-second-setup">30-Second Setup</a> •
  <a href="#approval-flows">Approval Flows</a> •
  <a href="#why-not-himalaya--resend--mailgun">vs. Alternatives</a> •
  <a href="#openclaw-integration">OpenClaw</a> •
  <a href="#production-scale">Scale</a> •
  <a href="#api-reference">API</a> •
  <a href="LICENSE">License</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/python-3.11+-blue.svg" alt="Python 3.11+">
  <img src="https://img.shields.io/badge/license-FSL--1.1--ALv2-green.svg" alt="License: FSL-1.1-ALv2">
  <img src="https://img.shields.io/badge/MCP-native-purple.svg" alt="MCP Native">
  <img src="https://img.shields.io/badge/OpenClaw-ready-orange.svg" alt="OpenClaw Ready">
</p>

---

Your agent needs to send email. You shouldn't need to configure DNS records, set up a new domain, or pay per-message fees to make that happen.

**Envelope: add your email address and SMTP password. That's it. Your agent sends email as you, from your mailbox, with your approval.**

```bash
# Add a mailbox
curl -X POST http://localhost:8000/accounts \
  -H "Content-Type: application/json" \
  -d '{"email": "you@gmail.com", "smtp_password": "your-app-password"}'

# Your agent drafts an email (doesn't send — waits for approval)
curl -X POST http://localhost:8000/accounts/{id}/drafts \
  -d '{"to": "client@example.com", "subject": "Proposal", "text": "..."}'

# You review and approve
curl -X POST http://localhost:8000/accounts/{id}/drafts/{draft_id}/approve
```

## 30-Second Setup

```bash
git clone https://github.com/tymrtn/U1F4E7.git && cd U1F4E7
python3 -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
cp .env.example .env
uvicorn app.main:app --reload
```

Open **http://localhost:8000** → add your email address + SMTP password → done. Your agent can send email.

For Gmail: [create an app password](https://myaccount.google.com/apppasswords). For Fastmail, Migadu, any IMAP/SMTP provider: use your existing credentials. Envelope auto-discovers IMAP/SMTP settings from your email address.

## Approval Flows

This is what separates Envelope from a raw SMTP library or a CLI tool like Himalaya.

**Your agent should never send email without your say-so.** Envelope enforces this architecturally:

```
Agent drafts email → Draft enters review queue → Human approves/rejects → Email sends (or doesn't)
```

### How it works

1. **Agent creates a draft** via API or MCP — the email is composed but NOT sent
2. **Draft enters the review queue** at `/review` — a web UI where you see exactly what the agent wants to send
3. **You approve or reject** — approve sends immediately; reject returns feedback to the agent
4. **Full audit trail** — every draft, every approval, every rejection is logged with timestamps

### Why this matters

AI agents are powerful but imperfect. A hallucinated email to a client, a wrong recipient, a tone-deaf reply — these are career-ending mistakes, not debug-and-retry situations. The approval gate is not a nice-to-have. It's the entire point.

You can also configure **domain policies** — rules like "always require approval for external domains" or "auto-approve internal replies" — so the gate is smart, not annoying.

### Approval modes

| Mode | Behavior | Use case |
|------|----------|----------|
| **Always approve** | Every email requires human approval | High-stakes: client comms, legal, finance |
| **Policy-based** | Rules per domain/recipient | Mixed: auto-approve internal, gate external |
| **Auto-send** | Agent sends freely, audit log only | Low-stakes: notifications, alerts, internal |

## OpenClaw Integration

Envelope ships with a `SKILL.md` in the repo root. OpenClaw agents that clone or install Envelope get the full API reference, confidence scoring guidelines, and blind routing protocol automatically.

### As an OpenClaw Skill

Your agent uses the REST API directly — no MCP required:

```
→ POST /accounts/{id}/drafts          # Compose email (blind routed by confidence)
→ GET /accounts/{id}/inbox            # Read inbox
→ GET /accounts/{id}/drafts           # Review pending drafts
→ POST /accounts/{id}/drafts/{id}/approve  # Approve and send
→ POST /accounts/{id}/drafts/{id}/reject   # Reject with feedback
→ GET /accounts/{id}/search?q=...     # Search messages
```

Blind routing means your agent always composes — never sends directly. The system evaluates confidence against hidden thresholds and routes automatically:

```
confidence ≥ 0.85  →  auto-sent
0.50 – 0.84        →  pending_review (human approves)
< 0.50             →  blocked
```

See [`SKILL.md`](SKILL.md) for the full API reference and confidence scoring guidelines.

### With Claude Desktop, Cursor, Windsurf

Add Envelope as an MCP server in your client config:

```json
{
  "mcpServers": {
    "envelope": {
      "command": "python",
      "args": ["-m", "app.mcp"],
      "cwd": "/path/to/U1F4E7",
      "env": {
        "ENVELOPE_SECRET_KEY": "your-key",
        "ENVELOPE_API_URL": "http://localhost:8000"
      }
    }
  }
}
```

Envelope works with any MCP-compatible client — it's not locked to any framework.

### With raw REST

No MCP? No skill? Every feature is available via the REST API. Use it from Python, Node, curl, or any HTTP client.

## Why Not Himalaya / Resend / Mailgun?

### vs. Himalaya

Himalaya is a great CLI email client for humans. Envelope is an email API for agents. The differences:

| | Envelope | Himalaya |
|---|---------|----------|
| **Approval gates** | ✅ Agent drafts, human approves | ❌ Sends immediately |
| **Audit trail** | ✅ Every action logged | ❌ No audit |
| **REST API** | ✅ Full CRUD | ❌ CLI only |
| **MCP server** | ✅ Native | ❌ None |
| **Domain policies** | ✅ Per-domain rules | ❌ None |
| **Multi-tenant** | ✅ Multiple accounts, one API | ⚠️ Config per account |
| **Review queue UI** | ✅ Web dashboard | ❌ None |

If your agent just needs to fire-and-forget a notification, Himalaya works. If your agent is emailing clients, partners, or anyone who matters — you need the approval layer.

### vs. Resend / Mailgun / SendGrid

| | Envelope | Resend | Mailgun | SendGrid |
|---|---------|--------|---------|----------|
| **BYO mailbox** | ✅ Your existing email | ❌ New domain required | ❌ New domain required | ❌ New domain required |
| **DNS setup** | **None** | SPF, DKIM, DMARC | SPF, DKIM, DMARC | SPF, DKIM, DMARC |
| **Per-message cost** | **$0** | $0.001+ | $0.001+ | $0.001+ |
| **Read inbox** | ✅ Full IMAP | ❌ Send only | ⚠️ Limited | ⚠️ Limited |
| **Approval gates** | ✅ | ❌ | ❌ | ❌ |
| **MCP server** | ✅ | ❌ | ❌ | ❌ |
| **Self-hosted** | ✅ | ❌ | ❌ | ❌ |
| **Open source** | ✅ | ❌ | ❌ | ❌ |

Resend and friends are transactional email services — they're great for sending password resets from `noreply@yourdomain.com`. Envelope is a full email operating layer — read, write, draft, approve, track, audit — on top of mailboxes you already own.

## Production Scale

Envelope isn't a toy. It's built to replace Resend/Mailgun in production:

- **Async everywhere** — aiosmtplib + aioimaplib for non-blocking I/O
- **Connection pooling** — persistent IMAP connections, not connect-per-request
- **Rate limiting** — per-account send throttling to stay within provider limits
- **Queue-based sending** — decouple draft creation from delivery
- **SQLite → Postgres** — swap the persistence layer for production (Postgres adapter on roadmap)
- **Horizontal scaling** — stateless API, externalize the DB, run N instances
- **Credential encryption** — Fernet encryption at rest for all stored passwords
- **Webhook delivery events** — get notified on send, bounce, open (roadmap)

### For SaaS / Multi-Tenant

Building a product where users connect their own email? Envelope handles multi-account natively. Each account has its own credentials, policies, and audit trail. Add a tenant layer on top and you have a white-label email API.

## Features

### Core
- **Send email** with CC, BCC, Reply-To, attachments
- **Read inbox** via IMAP — list, search, fetch messages
- **Thread reconstruction** — follow conversation chains
- **Auto-discovery** — paste an email address, Envelope finds IMAP/SMTP settings
- **Open tracking** — pixel-based read receipts
- **Signatures** — per-account, per-context

### Agent Layer
- **Draft previews** — agent composes, human reviews
- **Approval gates** — configurable per domain, per recipient, per account
- **Domain policies** — rules that govern what agents can do
- **Action audit log** — full provenance trail
- **Scheduled sends** — `send_after` for time-delayed delivery
- **Rate limiting** — per-account throttling

### Dashboard
- **Web UI** at `/` — manage accounts, view messages
- **Review queue** at `/review` — approve or reject agent drafts
- **Agent status** at `/agent/status` — monitor activity

## API Reference

### Accounts
| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/accounts` | Add a mailbox (email + SMTP password) |
| `GET` | `/accounts` | List all accounts |
| `GET` | `/accounts/{id}` | Get account details |
| `PATCH` | `/accounts/{id}` | Update account settings |
| `DELETE` | `/accounts/{id}` | Remove account |
| `POST` | `/accounts/{id}/verify` | Test connection |
| `GET` | `/accounts/discover?email=...` | Auto-discover mail settings |

### Send & Read
| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/send` | Send an email (or create draft if approval required) |
| `GET` | `/accounts/{id}/inbox` | List inbox messages |
| `GET` | `/accounts/{id}/inbox/{uid}` | Get full message |
| `GET` | `/accounts/{id}/threads/{msg_id}` | Get conversation thread |
| `GET` | `/accounts/{id}/folders` | List IMAP folders |

### Drafts & Approval
| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/accounts/{id}/drafts` | Create a draft (agent composes) |
| `GET` | `/accounts/{id}/drafts` | List pending drafts |
| `POST` | `/accounts/{id}/drafts/{id}/approve` | Approve and send |
| `POST` | `/accounts/{id}/drafts/{id}/reject` | Reject with reason |
| `DELETE` | `/accounts/{id}/drafts/{id}` | Discard |

### Agent & Monitoring
| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/agent/status` | Agent activity status |
| `GET` | `/agent/actions` | Action audit log |
| `POST` | `/agent/poll` | Poll for new messages |
| `GET` | `/health` | Health check |
| `GET` | `/stats` | Send/receive statistics |

## Architecture

```
┌──────────────┐     ┌──────────────────────────┐     ┌──────────────┐
│  AI Agent    │────▶│      Envelope API         │────▶│  Your SMTP   │
│  (OpenClaw,  │     │                            │     │  (Gmail,     │
│   Claude,    │     │  Draft → Review → Approve  │     │   Fastmail,  │
│   Cursor)    │◀────│  Policies → Audit → Send   │◀────│   Migadu)    │
│              │     │                            │     │              │
│  MCP / REST  │     │  SQLite / Postgres         │     │  IMAP/SMTP   │
└──────────────┘     └──────────────────────────┘     └──────────────┘
```

## Roadmap

- [ ] `pip install envelope-email` (Python SDK)
- [ ] Node.js SDK
- [ ] Postgres adapter
- [ ] Webhook delivery events (send, bounce, open)
- [ ] OAuth2 for Gmail/Outlook (no app passwords needed)
- [ ] React Email template support
- [ ] Docker compose one-liner
- [ ] Multi-tenant admin API
- [ ] OpenClaw skill package (one-command install)

## Contributing

Envelope uses an agentic engineering protocol. See `agents/PROTOCOL.md` for contribution workflow.

```bash
# Run tests
pytest tests/

# Run with debug logging
uvicorn app.main:app --reload --log-level debug
```

## License

[FSL-1.1-ALv2](LICENSE) — Functional Source License. Free to use, modify, and self-host. Converts to Apache 2.0 after 2 years. Cannot be used to build a competing hosted email API service.

---

<p align="center">
  <strong>Built by <a href="https://github.com/tymrtn">Tyler Martin</a></strong><br>
  <em>Your agent shouldn't need a $50/month Resend plan to send an email.</em>
</p>
