# Envelope Email (U1F4E7)

Turn any IMAP/SMTP email account into a programmable API. Built for agents.

## What This Is

Envelope gives your AI agents a REST API to send, read, and manage email from your existing mailbox. No new domain, no DNS config, no sending service fees. Plug in your Gmail, Outlook, Migadu, or Fastmail credentials and go.

On top of the mailbox API, Envelope adds agent-native primitives: draft previews, approval gates, reply threading, signature management, and full audit trails.

See [VISION.md](VISION.md) for product direction and [ARCHITECTURE.md](ARCHITECTURE.md) for system design.

## Current State

FastAPI skeleton with a `/send` stub endpoint, dashboard, and test suite. Transport layer (SMTP/IMAP) is next.

## Local Dev

```bash
cd U1F4E7
source ../venv/bin/activate
pip install -r requirements.txt
cp .env.example .env
uvicorn app.main:app --reload
```

App: http://localhost:8000

API docs: http://localhost:8000/docs

## Tests

```bash
cd U1F4E7
python -m pytest tests/ -v
```

## Project Structure

```
U1F4E7/
  app/
    main.py              # FastAPI application
  static/                # CSS/JS assets
  templates/             # Jinja2 HTML templates
  tests/                 # Test suite (pytest + httpx)
  agents/                # Agentic team coordination
    backlog/             # Story queue
    active/              # In-flight work
    handoffs/            # Cross-agent communication
    standups/            # Daily standups
    PROTOCOL.md          # Team protocol
  CLAUDE.md              # Agent instructions
  VISION.md              # Product vision
  ARCHITECTURE.md        # System architecture
```
