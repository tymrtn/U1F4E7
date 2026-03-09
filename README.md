<p align="center">
  <h1 align="center">📧 Envelope</h1>
  <p align="center"><strong>The open-source email API for AI agents.</strong></p>
  <p align="center">BYO mailbox. MCP-native. Zero sending fees.</p>
</p>

<p align="center">
  <a href="#quickstart">Quickstart</a> •
  <a href="#why-envelope">Why Envelope</a> •
  <a href="#features">Features</a> •
  <a href="#mcp-integration">MCP Integration</a> •
  <a href="#api-reference">API</a> •
  <a href="#comparison">Comparison</a> •
  <a href="LICENSE">License</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/python-3.11+-blue.svg" alt="Python 3.11+">
  <img src="https://img.shields.io/badge/license-FSL--1.1--ALv2-green.svg" alt="License: FSL-1.1-ALv2">
  <img src="https://img.shields.io/badge/MCP-native-purple.svg" alt="MCP Native">
</p>

---

Turn any IMAP/SMTP mailbox into a programmable email API. Connect Gmail, Outlook, Fastmail, Migadu — anything with IMAP/SMTP — and get a REST API + MCP server that AI agents can use natively.

No new domain. No DNS records. No per-message fees. Your agent sends email **as you**, from **your** mailbox, with **your** approval.

```bash
# Your agent can now send email
curl -X POST http://localhost:8000/send \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer YOUR_API_KEY" \
  -d '{"account_id": "...", "to": "hello@example.com", "subject": "Hello", "text": "Sent by my agent"}'
```

## Why Envelope

Email is the last integration AI agents can't own. Every other channel — Slack, Discord, SMS — has clean programmatic access. Email still requires either:

1. **A paid sending service** (Resend, Mailgun, SendGrid) that forces a new domain, DNS configuration, and per-message billing for what amounts to an SMTP relay
2. **Raw IMAP/SMTP library code** that every team rewrites from scratch, with no standard patterns for threading, drafts, or audit trails

Neither path gives agents what they actually need: **draft previews before sending, approval gates for human oversight, threading that preserves conversation context, and accountability logs for every action taken.**

Envelope solves this. Two layers:

- **Layer 1 — BYO Mailbox API:** Connect any IMAP/SMTP account. Send, read, search, and track email through a REST API. Zero infrastructure cost — you already own the mailbox.
- **Layer 2 — Agent Primitives:** Draft previews, approval gates, reply threading, signature management, domain policies, action audit logs, and an MCP server for native AI integration.

## Quickstart

```bash
git clone https://github.com/tymrtn/U1F4E7.git && cd U1F4E7
python3 -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
cp .env.example .env
```

Set `ENVELOPE_SECRET_KEY` in `.env` (any passphrase — used to encrypt stored credentials):

```bash
# Generate a key
python3 -c "from cryptography.fernet import Fernet; print(Fernet.generate_key().decode())"
```

Start the server:

```bash
uvicorn app.main:app --reload
```

Open **http://localhost:8000** → add your first mailbox → start sending.

## Features

### Core API
- **Send email** with CC, BCC, Reply-To, attachments
- **Read inbox** via IMAP — list, search, fetch messages
- **Thread reconstruction** — follow conversation chains
- **Folder management** — list and navigate mailbox folders
- **Auto-discovery** — paste an email address, Envelope finds the IMAP/SMTP settings

### Agent Primitives
- **Draft previews** — agent composes, human reviews before send
- **Approval gates** — human-in-the-loop checkpoint before any outbound message
- **Domain policies** — per-domain rules that govern agent behavior
- **Action audit log** — full provenance trail of every agent action
- **Signature management** — per-account, per-context signatures
- **Open tracking** — pixel-based read receipts
- **Scheduled sends** — `send_after` for time-delayed delivery
- **Rate limiting** — per-account send throttling

### MCP Integration
- **Native MCP server** — AI agents connect via Model Context Protocol
- **Policy-aware** — agents call `start_here()` to learn account rules before acting
- **Action logging** — every MCP tool call is logged for audit

### Dashboard
- **Web UI** at `/` — manage accounts, view messages, monitor status
- **Review queue** at `/review` — approve or reject agent-drafted emails
- **Agent status** at `/agent/status` — monitor agent activity

## MCP Integration

Envelope ships with a built-in [MCP](https://modelcontextprotocol.io/) server. Add it to your Claude Code, Cursor, or any MCP-compatible client:

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

Your agent can then:

```
→ start_here(account_id="...")     # Learn account policies
→ create_draft(to="...", ...)      # Compose a draft
→ list_drafts(account_id="...")    # Review pending drafts  
→ log_action(action="reviewed_inbox", ...)  # Audit trail
```

## API Reference

### Accounts
| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/accounts` | Add a mailbox (IMAP/SMTP credentials) |
| `GET` | `/accounts` | List all accounts |
| `GET` | `/accounts/{id}` | Get account details |
| `PATCH` | `/accounts/{id}` | Update account settings |
| `DELETE` | `/accounts/{id}` | Remove account |
| `POST` | `/accounts/{id}/verify` | Test IMAP/SMTP connection |
| `GET` | `/accounts/discover?email=...` | Auto-discover mail settings |

### Send & Read
| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/send` | Send an email |
| `GET` | `/accounts/{id}/inbox` | List inbox messages |
| `GET` | `/accounts/{id}/inbox/{uid}` | Get full message |
| `GET` | `/accounts/{id}/threads/{msg_id}` | Get conversation thread |
| `GET` | `/accounts/{id}/folders` | List IMAP folders |

### Drafts & Approval
| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/accounts/{id}/drafts` | Create a draft |
| `GET` | `/accounts/{id}/drafts` | List pending drafts |
| `POST` | `/accounts/{id}/drafts/{id}/approve` | Approve and send |
| `POST` | `/accounts/{id}/drafts/{id}/reject` | Reject with reason |
| `DELETE` | `/accounts/{id}/drafts/{id}` | Discard draft |

### Agent
| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/agent/status` | Agent activity status |
| `GET` | `/agent/actions` | Audit log of all actions |
| `POST` | `/agent/poll` | Poll for new messages |

### Monitoring
| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/health` | Health check |
| `GET` | `/stats` | Send/receive statistics |
| `GET` | `/messages` | Global message log |

## Comparison

| Feature | Envelope | Resend | Mailgun | SendGrid |
|---------|----------|--------|---------|----------|
| BYO mailbox (IMAP/SMTP) | ✅ | ❌ | ❌ | ❌ |
| MCP server for AI agents | ✅ | ❌ | ❌ | ❌ |
| Draft preview + approval gates | ✅ | ❌ | ❌ | ❌ |
| Agent action audit log | ✅ | ❌ | ❌ | ❌ |
| Domain policy engine | ✅ | ❌ | ❌ | ❌ |
| Read inbox (IMAP) | ✅ | ❌ | ✅ | ✅ |
| Open tracking | ✅ | ✅ | ✅ | ✅ |
| Per-message pricing | **$0** | $0.001+ | $0.001+ | $0.001+ |
| New domain required | **No** | Yes | Yes | Yes |
| DNS configuration | **No** | Yes | Yes | Yes |
| Self-hosted | ✅ | ❌ | ❌ | ❌ |
| Open source | ✅ | ❌ | ❌ | ❌ |
| React Email templates | ❌ | ✅ | ❌ | ✅ |
| Webhooks | ✅ | ✅ | ✅ | ✅ |

## Stack

- **Python 3.11+** / **FastAPI**
- **aiosmtplib** + **aioimaplib** for async mail transport
- **SQLite** for persistence (swap to Postgres for production)
- **MCP SDK** for AI agent integration
- **Fernet** for credential encryption at rest

## Architecture

```
┌──────────────┐     ┌──────────────────────┐     ┌──────────────┐
│  AI Agent    │────▶│   Envelope API       │────▶│  Your SMTP   │
│  (MCP/REST)  │     │  (FastAPI + SQLite)  │     │  (Gmail,     │
│              │◀────│                      │◀────│   Migadu,    │
│              │     │  Drafts → Approval   │     │   Fastmail)  │
│              │     │  Policies → Audit    │     │              │
└──────────────┘     └──────────────────────┘     └──────────────┘
```

## Roadmap

- [ ] Python SDK (`pip install envelope-email`)
- [ ] Node.js SDK
- [ ] Webhook delivery events
- [ ] Multi-tenant mode
- [ ] Postgres adapter
- [ ] React Email template support
- [ ] OAuth2 for Gmail/Outlook (no app passwords)
- [ ] CLI tool (`envelope send ...`)
- [ ] Docker compose one-liner

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
  <em>Because your agent shouldn't need a $50/month Resend plan to send an email.</em>
</p>
