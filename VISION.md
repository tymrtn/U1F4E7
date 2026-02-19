# Envelope Email - Product Vision

## What It Is

Envelope turns any IMAP/SMTP email account into a programmable API, with agent-native features built in.

Plug in your existing mailbox credentials (Gmail, Outlook, Migadu, Fastmail, anything with IMAP/SMTP). Get a REST API to send, read, and track email. No new domain registration. No DNS configuration. No sending service bill.

## Who It's For

Agentic engineering teams building AI systems that need to send and receive email.

These teams know that Mailgun, SendGrid, and Resend are commodity SMTP wrappers charging per-message fees for infrastructure that costs nearly nothing to operate. They don't need another sending service. They need their existing mailbox exposed as an API their agents can use.

## The Problem

Email is the last integration agents can't own. Every other channel (Slack, Discord, SMS) has clean programmatic access. Email still requires either:

1. A paid sending service (Mailgun, Resend) that forces a new domain, DNS records, and per-message billing for what amounts to an SMTP relay
2. Raw IMAP/SMTP library code that every team rewrites from scratch, with no standard patterns for threading, drafts, or audit trails

Neither path gives agents what they actually need: draft previews before sending, reply-to threading that preserves conversation context, approval gates for human oversight, signature handling, and accountability logs.

## The Solution

Envelope sits between your existing mailbox and your agents. Two layers:

**Layer 1 - BYO Mailbox API**: Connect any IMAP/SMTP account. Send, read, search, and track email through a REST API. This is the commodity layer done right -- zero infrastructure cost because you already own the mailbox.

**Layer 2 - Agent Primitives**: The features that make email usable by autonomous systems:
- **Draft previews**: Agent composes, human reviews before send
- **Reply-to threading**: Maintain conversation context across agent interactions
- **Approval gates**: Human-in-the-loop checkpoints before any outbound message
- **Signature management**: Per-account, per-context signature handling
- **Audit trails**: Full provenance log of every action taken on every message

## Why Not Just Use Resend/Mailgun?

Resend and Mailgun solve a different problem: bulk transactional email from a dedicated sending domain. They're the right tool for SaaS notification pipelines.

Envelope solves the agent problem: programmatic access to real mailboxes that already exist, with primitives designed for autonomous workflows. Your agent sends email *as you*, from *your* account, with *your* approval.

## MVP Scope

- IMAP/SMTP send and read via REST API
- Gmail, Outlook, Migadu, Fastmail as initial targets
- Credential management (encrypted at rest)
- Basic send tracking
- SQLite persistence

## Moonshot

Envelope becomes the default email layer for every agent framework. The way `requests` is the HTTP library and `boto3` is the AWS library, Envelope is how agents do email.
