# Envelope Email

Turn any IMAP/SMTP email account into a programmable API.

## Quickstart

```bash
git clone <repo-url> && cd envelope-email
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

Open http://localhost:8000

## Adding an Email Account

1. Click **+ Add** in the Accounts section
2. Enter your email address in the Username field
3. Click **Discover** -- this probes DNS (SRV, autoconfig, MX) to find your provider's SMTP/IMAP servers automatically
4. Enter your password (app password if using Gmail/Outlook with 2FA)
5. Click **Save Account**, then **Verify** to test the SMTP connection

The discover endpoint works with Gmail, Outlook, Fastmail, Migadu, and most providers. If discovery fails, enter the SMTP/IMAP host and port manually.

## Sending Email

Use the Compose form in the dashboard, or hit the API directly:

```bash
curl -X POST http://localhost:8000/send \
  -H "Content-Type: application/json" \
  -d '{
    "account_id": "<account-id-from-accounts-list>",
    "to": "recipient@example.com",
    "subject": "Hello",
    "text": "Sent via Envelope"
  }'
```

## API

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/accounts` | List all accounts |
| POST | `/accounts` | Add an account |
| DELETE | `/accounts/{id}` | Remove an account |
| POST | `/accounts/{id}/verify` | Test SMTP connection |
| GET | `/accounts/discover?email=you@domain.com` | Auto-discover mail server settings |
| POST | `/send` | Send an email |
| GET | `/messages` | List sent messages |
| GET | `/messages/{id}` | Get message details |
| GET | `/stats` | Send stats (total, sent, failed, success rate) |
| GET | `/docs` | Interactive API docs (Swagger) |

## Project Structure

```
app/
  main.py              # FastAPI routes
  db.py                # SQLite setup (WAL mode)
  messages.py          # Message tracking
  discovery.py         # Mail server auto-discovery
  credentials/
    store.py           # Account CRUD with encrypted passwords
    crypto.py          # Fernet encryption
  transport/
    smtp.py            # SMTP send via aiosmtplib
templates/
  index.html           # Dashboard shell (Jinja2)
static/
  dashboard.js         # All client-side fetch/render logic
tests/
  test_app.py          # pytest-asyncio test suite
```

## Running Tests

```bash
ENVELOPE_SECRET_KEY=test pytest tests/ -x -q
```

## What's Next

- IMAP read transport (inbox polling, reply threading)
- Agent primitives (draft preview, approval gates, audit trails)
- Domain provisioning API (auto-configure DNS for new domains)

See [VISION.md](VISION.md) for product direction and [ARCHITECTURE.md](ARCHITECTURE.md) for system design.
