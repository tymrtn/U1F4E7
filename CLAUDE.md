# Envelope Email (U1F4E7)

## Project

BYO mailbox API with agent-native primitives. Turn any IMAP/SMTP account into a programmable email API.

## License

FSL-1.1-ALv2. All source files must include:
```
# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)
```

Stack: Python, FastAPI, aiosmtplib, aioimaplib, SQLite.

## Multi-Agent Coordination

This project uses an agentic engineering team. See `agents/PROTOCOL.md` for the full protocol.

### Agent Roster
- **CPO Advisor** -- Product strategy, story validation, prioritization
- **CTO Advisor** -- Architecture, tech debt, build-vs-buy decisions
- **QA Tester** -- Post-implementation verification, quality gate

### Handoff Rules
- Use `agents/handoffs/` for cross-agent communication
- Never silently drop a blocker -- escalate through handoff
- QA sign-off required before any merge to main

## Bug Fixing Protocol

1. Write a failing test that reproduces the bug
2. Fix the code
3. Verify the test passes
4. Check for regressions

Never fix a bug without a test. The test is the proof.

## Agent Context Discipline

- Read the story file before starting work
- Read relevant source files before proposing changes
- Check `agents/active/` for in-flight work that might conflict
- Update the story file when done

## Commit Format

```
<type>(<scope>): <subject>

Agent: <agent-name>
Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>
```

**Types**: feat, fix, refactor, docs, test, chore, perf

**Scopes**: api, transport, credentials, primitives, infra

## Directory Layout

```
U1F4E7/
  app/main.py           # FastAPI application
  static/               # CSS/JS assets
  templates/            # Jinja2 HTML templates
  agents/               # Agentic team coordination
    backlog/            # Story queue
    active/             # In-flight work
    handoffs/           # Cross-agent communication
    standups/           # Daily standups
  VISION.md             # Product vision
  ARCHITECTURE.md       # System architecture
```
