# Envelope Email

Turn any IMAP/SMTP email account into a programmable API. Agent-native primitives: drafts, approval gates, threading, audit logs.

## Quickstart

```bash
git clone https://github.com/tymrtn/U1F4E7.git && cd U1F4E7
python3 -m venv venv && source venv/bin/activate
pip install -r requirements.txt
cp .env.example .env
```

Edit `.env` and set `ENVELOPE_SECRET_KEY` to any passphrase (used to encrypt stored credentials):

```
ENVELOPE_SECRET_KEY=pick-any-passphrase-here
```

Start the server:

```bash
uvicorn app.main:app --reload
```

Open http://localhost:8000 (dashboard) or http://localhost:8000/review (agent review queue).

## Adding an Email Account

1. Click **+ Add** in the Accounts section
2. Enter your email address in the Username field
3. Click **Discover** — probes DNS (SRV, autoconfig, MX) to find your provider's SMTP/IMAP servers automatically
4. Enter your password (app password if using Gmail/Outlook with 2FA)
5. Click **Save Account**, then **Verify** to test the SMTP connection

Works with Gmail, Outlook, Fastmail, Migadu, and most providers. If discovery fails, enter SMTP/IMAP host and port manually.

## Sending Email

Use the Compose form in the dashboard, or hit the API directly:

```bash
# Synchronous (waits for SMTP delivery)
curl -X POST http://localhost:8000/send \
  -H "Content-Type: application/json" \
  -d '{
    "account_id": "<account-id>",
    "to": "recipient@example.com",
    "subject": "Hello",
    "text": "Sent via Envelope"
  }'

# Async (queues for background delivery with retry)
curl -X POST http://localhost:8000/send \
  -H "Content-Type: application/json" \
  -d '{
    "account_id": "<account-id>",
    "to": "recipient@example.com",
    "subject": "Hello",
    "text": "Sent via Envelope",
    "wait": false
  }'
```

## Drafts & Approval

Drafts are the core primitive for human-in-the-loop and agent workflows.

```bash
# Create a draft
curl -X POST http://localhost:8000/accounts/{id}/drafts \
  -H "Content-Type: application/json" \
  -d '{"to": "recipient@example.com", "subject": "Hello", "text": "Draft body"}'

# Approve and send immediately
curl -X POST http://localhost:8000/accounts/{id}/drafts/{draft_id}/approve

# Schedule for later
curl -X PATCH http://localhost:8000/accounts/{id}/drafts/{draft_id} \
  -H "Content-Type: application/json" \
  -d '{"send_after": "2026-03-01T09:00:00Z"}'

# Snooze (hide from queue until a future time)
curl -X PATCH http://localhost:8000/accounts/{id}/drafts/{draft_id} \
  -H "Content-Type: application/json" \
  -d '{"snoozed_until": "2026-02-25T08:00:00Z"}'

# Reject with feedback (agent regenerates on next poll)
curl -X POST http://localhost:8000/accounts/{id}/drafts/{draft_id}/reject \
  -H "Content-Type: application/json" \
  -d '{"feedback": "Too formal, use a warmer tone"}'
```

## Review Queue

Open http://localhost:8000/review to review agent-created drafts without re-reading every email.

The queue is decision-first: each card leads with the agent's reasoning, not the draft text. Layout adapts based on where confidence falls relative to your account thresholds:

- **Above auto-send threshold** — collapsed, batch-selectable, Send is the primary action
- **Between thresholds** — expanded with variable highlights, full four-intent CTA row: Send / Send later / Snooze / Feedback
- **Escalations** — no approve button; escalation note leads; View Thread fetches the original message inline

Thresholds are visible and adjustable inline — changes PATCH the account immediately.

## Inbox Agent

Enable to classify incoming mail and create drafts for review automatically:

```env
AGENT_ENABLED=true
AGENT_ACCOUNT_ID=<account-id>
AGENT_POLL_INTERVAL=120
```

The agent outputs structured signals alongside each draft (`kb_match`, `sensitive_categories`, `thread_context`) so the review queue can show scannable evidence rather than a score tier.

Trigger a manual poll:

```bash
curl -X POST http://localhost:8000/agent/poll
```

## API Reference

### Accounts

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/accounts` | List all accounts |
| POST | `/accounts` | Add an account |
| GET | `/accounts/{id}` | Get account details (includes thresholds) |
| PATCH | `/accounts/{id}` | Update display_name, auto_send_threshold, review_threshold |
| DELETE | `/accounts/{id}` | Remove an account |
| POST | `/accounts/{id}/verify` | Test SMTP connection |
| GET | `/accounts/discover?email=` | Auto-discover mail server settings |
| GET | `/accounts/discover/stream?email=` | SSE progressive discovery stream |

### Drafts

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/accounts/{id}/drafts` | Create draft |
| GET | `/accounts/{id}/drafts` | List drafts (filter by status, created_by, hide_snoozed) |
| GET | `/accounts/{id}/drafts/{draft_id}` | Get draft |
| PUT | `/accounts/{id}/drafts/{draft_id}` | Update draft content |
| PATCH | `/accounts/{id}/drafts/{draft_id}` | Set send_after or snoozed_until |
| POST | `/accounts/{id}/drafts/{draft_id}/approve` | Send now (records approved_by=review-queue) |
| POST | `/accounts/{id}/drafts/{draft_id}/send` | Send now (generic, accepts approved_by param) |
| POST | `/accounts/{id}/drafts/{draft_id}/reject` | Reject with optional feedback |
| DELETE | `/accounts/{id}/drafts/{draft_id}` | Discard draft |

### Inbox & Threads

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/accounts/{id}/inbox` | List inbox messages |
| GET | `/accounts/{id}/inbox/{uid}` | Fetch full message |
| GET | `/accounts/{id}/threads/{message_id}` | Get thread by message-id |
| GET | `/accounts/{id}/folders` | List IMAP folders |
| GET | `/accounts/{id}/context?q=` | Semantic search over indexed mail |

### Messages & Stats

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/send` | Send email (wait: false for async) |
| GET | `/messages` | List sent messages |
| GET | `/messages/{id}` | Get message details |
| GET | `/stats` | Send stats |

### Agent

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/agent/status` | Agent status and poll counts |
| GET | `/agent/actions` | Agent action log |
| POST | `/agent/poll` | Trigger manual poll |

## Project Structure

```
app/
  main.py              # FastAPI routes
  db.py                # SQLite setup (WAL mode, migrations)
  messages.py          # Message tracking
  drafts.py            # Draft CRUD with approval workflow
  discovery.py         # Mail server auto-discovery
  credentials/
    store.py           # Account CRUD with encrypted passwords
    crypto.py          # Fernet encryption
  transport/
    smtp.py            # SMTP send + MIME construction
    pool.py            # Connection pool (per-account semaphore, NOOP validation)
    worker.py          # Background send worker (exponential backoff retry)
    imap.py            # IMAP read, search, thread reconstruction
  agent/
    inbox_agent.py     # Email triage agent (poll, classify, draft, escalate)
    prompts.py         # LLM system prompts and response schema
    llm.py             # OpenRouter API client
    knowledge.py       # Domain knowledge base
    embeddings.py      # Semantic search over indexed mail
templates/
  index.html           # Dashboard (Jinja2)
  review.html          # Review queue shell
static/
  dashboard.js         # Dashboard client logic
  review.js            # Review queue React app
tests/
  test_app.py          # API endpoint tests
  test_drafts.py       # Draft workflow tests
  test_pool.py         # Connection pool tests
  test_worker.py       # Background worker tests
  test_discovery.py    # Auto-discovery tests
  test_agent.py        # Agent classification tests
```

## Running Tests

```bash
ENVELOPE_SECRET_KEY=test pytest tests/ -x -q
```

## License

FSL-1.1-ALv2. See [LICENSE](LICENSE).

See [VISION.md](VISION.md) for product direction and [ARCHITECTURE.md](ARCHITECTURE.md) for system design.
