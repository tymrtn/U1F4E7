---
name: envelope
description: ARCHIVAL ONLY. Historical Python/FastAPI Envelope API notes for the old U1F4E7 system. Do not use this for current operations. Current Envelope usage is the Rust CLI at ~/.local/bin/envelope.
---

# Envelope (archival Python API)

This file documents the old Python/FastAPI Envelope API that lived in the historical `U1F4E7` app.

It is not the current Envelope system Tyler uses.

Current truth:
- Active tool: Rust CLI
- Binary: `~/.local/bin/envelope` (wraps `~/.local/libexec/envelope-rust`)
- Repo: `~/Dropbox/Code/envelope-email/u1f4e7-repo/` (github.com/tymrtn/U1F4E7)
- Storage: `~/Library/Application Support/envelope-email/`
- Hermes skill: the installed `envelope` skill under Hermes, which documents the Rust CLI

Do not follow the old API workflow in this directory for current email work.
Do not assume blind-routing REST endpoints here are live.
Do not treat this file as authoritative for current Envelope behavior.

If you need current usage, use commands like:
```bash
envelope accounts list --json
envelope inbox --account <email> --json
envelope read --account <email> <uid> --json
envelope search --account <email> "<query>" --json
```

Historical note:
- The parent `~/Dropbox/Code/envelope-email/` tree contains stale Python-era artifacts.
- The current operational Envelope path is the Rust CLI at `~/.local/bin/envelope`.
