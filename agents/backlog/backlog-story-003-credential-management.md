---
id: story-003
status: backlog
priority: high
estimated_points: 5
depends_on: []
blocks: [story-001, story-002]
---

# Story-003: Add credential management with encrypted storage [API]

**As a** developer connecting my mailbox to Envelope
**I want** to register my IMAP/SMTP credentials securely
**So that** Envelope can connect to my mail server without exposing my passwords

## Context

This is the prerequisite for all transport features. Before Envelope can send or read email, it needs to know how to connect to the user's mail server. Credentials must be encrypted at rest.

Per ARCHITECTURE.md: SQLite with Fernet symmetric encryption, key from environment variable.

## Acceptance Criteria

- [ ] `POST /accounts` registers a new mailbox with IMAP and SMTP credentials
- [ ] `GET /accounts` lists registered accounts (without exposing passwords)
- [ ] `GET /accounts/{id}` returns account details (host, port, username -- no password)
- [ ] `DELETE /accounts/{id}` removes an account and its credentials
- [ ] Credentials encrypted at rest using Fernet symmetric encryption
- [ ] Encryption key loaded from `ENVELOPE_SECRET_KEY` environment variable
- [ ] `POST /accounts/{id}/verify` tests the IMAP and SMTP connections and reports status
- [ ] SQLite database created automatically on first run
- [ ] Rejects registration if required fields are missing (host, port, username, password for both IMAP and SMTP)

## Technical Notes

- Use `cryptography.fernet` for encryption (add to requirements.txt)
- SQLite via `aiosqlite` for async access (add to requirements.txt)
- Schema: `accounts` table with `id`, `name`, `imap_host`, `imap_port`, `smtp_host`, `smtp_port`, `username`, `encrypted_password`, `display_name`, `default_signature`, `created_at`, `verified_at`
- Consider a `credentials/` module: `store.py` (CRUD), `crypto.py` (encrypt/decrypt)
- Generate Fernet key: `python3 -c "from cryptography.fernet import Fernet; print(Fernet.generate_key().decode())"`

## Regression Check

Run BEFORE starting (baseline) and AFTER completing (verify no breakage):

```bash
cd U1F4E7 && uvicorn app.main:app --host 0.0.0.0 --port 8000 &
sleep 2

# Existing endpoints still work
curl -s http://localhost:8000/ | head -5

# New endpoints respond
curl -s http://localhost:8000/accounts | python3 -m json.tool

# Register account
curl -s -X POST http://localhost:8000/accounts \
  -H "Content-Type: application/json" \
  -d '{"name":"test","imap_host":"imap.example.com","imap_port":993,"smtp_host":"smtp.example.com","smtp_port":587,"username":"user@example.com","password":"secret"}' | python3 -m json.tool

kill %1
```

## Affected Files

**New:**
- `app/credentials/__init__.py`
- `app/credentials/store.py`
- `app/credentials/crypto.py`
- `app/db.py` (SQLite setup and migrations)

**Modified:**
- `app/main.py` (add `/accounts` endpoints)
- `requirements.txt` (add `cryptography`, `aiosqlite`)
- `.env.example` (add `ENVELOPE_SECRET_KEY`)

**Reference:**
- `ARCHITECTURE.md` (component 2: Credential Store)
