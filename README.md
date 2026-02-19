# Envelope Email

Turn any IMAP/SMTP email account into a programmable API. Built for agents.

## What This Is

Envelope gives your AI agents a REST API to send, read, and manage email from your existing mailbox. No new domain, no DNS config, no sending service fees. Plug in your Gmail, Outlook, Migadu, or Fastmail credentials and go.

On top of the mailbox API, Envelope adds agent-native primitives: draft previews, approval gates, reply threading, signature management, and full audit trails.

See [VISION.md](VISION.md) for product direction and [ARCHITECTURE.md](ARCHITECTURE.md) for system design.

## Current State

Early prototype. FastAPI skeleton with a `/send` endpoint and basic dashboard.

What's next:
- IMAP/SMTP transport (send and read via BYO mailbox credentials)
- Credential management (encrypted at rest)
- Inbox read and reply threading
- Agent primitives (drafts, approvals, audit trails)

## Local Dev

```bash
cd ~/Dropbox/Code/envelope-email
source venv/bin/activate
pip install -r requirements.txt
cp .env.example .env  # Configure your credentials
uvicorn app.main:app --reload
```

App: http://localhost:8000

API docs: http://localhost:8000/docs

## Project Structure

```
envelope-email/
  app/
    main.py          # FastAPI application
  static/            # CSS/JS assets
  templates/         # Jinja2 HTML templates
  VISION.md          # Product vision
  ARCHITECTURE.md    # System architecture
  plan.md            # Development plan
```
