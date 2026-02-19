# Agentic Team Protocol

## Project

**Envelope Email (U1F4E7)** -- BYO mailbox API with agent-native primitives.

## Stack

- Python 3.11+
- FastAPI
- aiosmtplib (outbound SMTP)
- aioimaplib (inbound IMAP)
- SQLite (persistence)

## Commit Format

```
<type>(<scope>): <subject>

<body - optional>

Agent: <agent-name>
```

**Types**: feat, fix, refactor, docs, test, chore, perf

**Scopes**: api, transport, credentials, primitives, infra

## Quality Gates

- QA Tester sign-off required before merge to main
- All tests must pass
- No regressions in existing endpoints

## Agent Roster

### CPO Advisor
- Product strategy and story validation
- Prioritization decisions
- Scope guard (prevents feature creep)

### CTO Advisor
- Architecture review and tech debt tracking
- Build-vs-buy decisions
- Performance and security review

### QA Tester
- Post-implementation verification
- Regression testing
- Quality gate approval/rejection

## Workflow

1. Stories land in `backlog/` as numbered markdown files
2. Active work moves to `active/` with agent assignment
3. Handoffs between agents go in `handoffs/`
4. Daily standups captured in `standups/`

## Conventions

- One story per file
- Stories follow write-story skill format
- Agents identify themselves in commits and handoffs
- Blockers escalate through handoffs, not silently
