#!/bin/bash
# Kill any existing instance on port 8000
/usr/sbin/lsof -ti:8000 | xargs kill -9 2>/dev/null
sleep 1

cd ~/Dropbox/Code/envelope-email/U1F4E7
ENVELOPE_DB_PATH="$HOME/Dropbox/Code/envelope-email/envelope.db" \
exec .venv/bin/uvicorn app.main:app --host 127.0.0.1 --port 8000
