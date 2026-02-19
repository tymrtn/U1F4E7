# Envelope Email MVP

Transactional Email API like Mailgun.

## Local Dev

```bash
cd ~/Dropbox/Code/envelope-email
source venv/bin/activate
pip install -r requirements.txt
cp .env.example .env  # Add your RESEND_API_KEY
uvicorn app.main:app --reload
```

Visit http://localhost:8000

API Docs: http://localhost:8000/docs

## Deploy Railway

1. `git add . && git commit -m "MVP" && git push`
2. Connect repo to Railway.app new project.
3. Add RESEND_API_KEY env var.

## Features

- POST /send: Send email via Resend
- GET /: Dashboard
- POST /webhooks/resend: Events (todo: log)
- SMTP/IMAP backend planned (aiosmtplib)

Aposema licensed.
