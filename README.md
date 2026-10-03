<p align="center">
  <h1 align="center">📧 Envelope</h1>
  <p align="center"><code>U+1F4E7</code> — if you know, you know.</p>
  <p align="center"><strong>One inbox for all your email accounts, shared with your agents.</strong></p>
</p>

> **Why U1F4E7?** It's the Unicode codepoint for 📧. Humans see a repo name. Agents see an envelope.

<p align="center">
  <a href="#quick-start">Setup</a> •
  <a href="#travel">Travel</a> •
  <a href="#cli-reference">CLI</a> •
  <a href="#rules-engine">Rules</a> •
  <a href="#mcp-server">MCP</a> •
  <a href="#use-envelope-from-your-agent">Agent plugins</a> •
  <a href="#why-not-himalaya--cloudflare--resend">vs. Alternatives</a> •
  <a href="#dashboard">Dashboard</a> •
  <a href="#commercial-licensing">Commercial licensing</a> •
  <a href="LICENSE">License</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/rust-stable-blue.svg" alt="Rust">
  <img src="https://img.shields.io/badge/version-1.3.14-green.svg" alt="v1.3.14">
  <img src="https://img.shields.io/badge/license-FSL--1.1--ALv2-green.svg" alt="License: FSL-1.1-ALv2">
</p>

---

Envelope connects to the email accounts you already have (Gmail, Fastmail, iCloud, your own domain, any IMAP server) and puts them in one place. Open the Unified Inbox in your browser to read every account together, search them all at once, and archive or snooze across accounts in one pass. Your agents work the same mailboxes from the CLI, with JSON output, or over MCP, and anything they draft waits for your approval.

You don't change DNS, set up a new domain, or pay per-message fees. Add each address with its app password and go.

```bash
envelope accounts add --email you@gmail.com
envelope accounts add --email you@work.example
envelope serve                                     # every account in one inbox at http://localhost:3141
envelope inbox --account you@work.example --json   # the same mail, for your agent
```

## Install

```bash
# macOS or Linux: downloads the release binary for your OS and CPU, checks it
# against the release's SHA-256 file, and installs it to ~/.local/bin (no sudo).
curl -fsSL https://u1f4e7.com/install.sh | bash

# Homebrew (macOS). The formula builds from source, so Homebrew installs Rust
# as a build dependency and the first install takes a few minutes.
brew install tymrtn/u1f4e7/envelope
# `tymrtn/u1f4e7/u1f4e7` is a live compat alias for the same formula.
```

To build it yourself instead:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh && source "$HOME/.cargo/env"
git clone https://github.com/tymrtn/U1F4E7
cd U1F4E7
cargo build --release
# Add `--features governor` to build in the Governor send gate (see below).
cp target/release/envelope ~/.local/bin/envelope
```

The release workflow signs and notarizes the macOS binaries once its Apple
credentials are configured ([docs/release-signing.md](docs/release-signing.md));
until then macOS release binaries are unsigned. If macOS refuses to run an
unsigned tarball you downloaded in a browser, clear the quarantine flag:
`xattr -d com.apple.quarantine ~/.local/bin/envelope`.

## Quick start

Envelope signs in to IMAP and SMTP with a password or an app password. It has
no OAuth sign-in yet, and that decides which mailboxes work today:

- **Fastmail and iCloud Mail:** create an app password in the provider's
  security settings.
- **Gmail:** create an [app password](https://support.google.com/accounts/answer/185833).
  Google only offers them once 2-Step Verification is on, and some Workspace
  and Advanced Protection accounts cannot create one. If Google won't let you
  create an app password for your account, Gmail can't connect to Envelope yet
  (OAuth sign-in isn't supported).
- **Migadu, self-hosted Dovecot, and most other IMAP hosts:** your normal
  mailbox password.
- **Outlook.com, Hotmail, Live, and Microsoft 365:** not supported yet.
  Microsoft turned off password sign-in for IMAP on
  [Outlook.com accounts](https://support.microsoft.com/en-us/office/modern-authentication-methods-now-needed-to-continue-syncing-outlook-email-in-non-microsoft-email-apps-c5d65390-9676-4763-b41f-d7986499a90d)
  and [Exchange Online](https://learn.microsoft.com/en-us/exchange/clients-and-mobile-in-exchange-online/deprecation-of-basic-authentication-exchange-online),
  so an app password does not help.

Envelope encrypts saved credentials with a passphrase. In a terminal it asks
you for one. Scripts, agents, and the piped command below have no terminal to
ask from, so give Envelope a passphrase file first:

```bash
# Create a random passphrase readable only by you, and point Envelope at it.
# Add the export line to your shell profile, and back the file up: without it
# the saved credentials cannot be decrypted.
(umask 077 && openssl rand -base64 32 > "$HOME/.envelope-passphrase")
export ENVELOPE_MASTER_PASSPHRASE_FILE="$HOME/.envelope-passphrase"

# Add an account. Envelope finds the IMAP/SMTP servers from the email domain
# and logs in before saving; a rejected login fails here and prints what to fix.
# (--skip-login-check saves without logging in, for offline setup.)
printf '%s\n' "$APP_PASSWORD" | envelope accounts add --email you@fastmail.com --password-stdin

# Verify setup end-to-end (paths → account → IMAP auth → inbox peek)
envelope quickstart

# See folders with unread counts
envelope folders

# Read the inbox
envelope inbox --limit 20

# Read a message (does not mark it as read)
envelope read 42

# Send with attachment
envelope send --to someone@example.com --subject "Report" --body "Attached." --attach report.pdf

# Reply in-thread as a reviewable draft
envelope draft reply 42 --body "Thanks."

# Snooze until Monday
envelope snooze set 42 --until monday --reason waiting-reply

# Watch for new mail in real time (IMAP IDLE push)
envelope watch --json

# Retrieve a verification-code JSON result for unattended automation (bounded and fail-closed)
envelope --json code --account you@example.com --from otp@issuer.example --wait 60

# Schedule a send for business hours
envelope send --to cto@example.com --subject "Report" --body "..." --at "monday 9am"

# Import contacts from your inbox, then create a contact-based rule
envelope contacts import --from-inbox
envelope rule create --name "VIP" --match-contact-tag vip --action flag=\\Flagged

# Agent scores a message, creates a rule, Envelope enforces it forever
envelope tag set 42 --score urgent=0.1 --tag newsletter
envelope rule create --name "Junk newsletters" --match-tag newsletter --match-score-below interesting=0.3 --action move=Junk
envelope rule run --confirm

# Unsubscribe from a mailing list (dry-run by default)
envelope unsubscribe 99

# Open the local dashboard at http://localhost:3141 (loopback, no auth — single-user local trust)
envelope serve

# Expose it to agents/devices on your tailnet — REQUIRES auth first.
# Option A (humans): allowlist tailnet identities; `tailscale serve` proves them.
envelope config set dashboard.tailscale_allow "you@your-tailnet.ts.net,skippy@your-tailnet.ts.net"
# Option B (agents/scripts): a bearer token (stored 0600, never echoed).
envelope config set dashboard.auth_token "$(openssl rand -hex 32)"

tailscale serve --bg 3141

# See where Envelope is storing local state
envelope paths
```

The dashboard opens as a three-pane mail shell. Unified Inbox is the default
read-only surface and loads from the local message index; explicit refreshes
or account/folder selections are what touch live IMAP mailboxes.

Actual sends are outbox-first. By default, an allowed send queues with a safety
cooldown and the scheduled-send sweep performs the SMTP transmission later.
Every agent send must declare factual attributes, and Envelope checks them
against what it can observe before anything is queued or sent.

Governor scoring is a build-time Cargo feature, off by default. Homebrew and the
release archives are built without it, so their sends are not scored by
Governor, and send results and audit events record the gate as
`mode: "off"`. In those builds the draft-only ceiling still binds MCP sessions
that carry an agent token, but an agent with shell access can send through
`envelope send` (which defaults to `autonomous-send`), so give agents MCP with
an agent token rather than a shell. Builds made with
`cargo build --release --bin envelope --features governor` (or
`cargo install --path crates/cli --features governor`) derive sanitized
contextual attributes immediately before SMTP and ask Governor to score them
with `governor score --catalog envelope`; only an opaque Governor `allow` sends.
Those builds expect the Governor binary at the path pinned in
`SMTP_GOVERNOR_BIN` (`crates/email/src/outbound.rs`), and every send fails
closed when it is missing. Message bodies, full recipient
addresses, attachment bytes, and secrets never enter the Governor request.

Dashboard URL metadata in `--json` output is discovered fresh for each UI
construction or request, from an active local `tailscale serve` HTTPS route whose
root proxy targets `http://127.0.0.1:3141`. It otherwise falls back to
`http://localhost:3141`.
Every agent-facing `ui` object reports that decision as
`dashboard_origin_source` (`tailscale_serve` or `localhost_fallback`) and adds
a non-secret `dashboard_origin_warning` only when discovery needs attention.
MCP results omit these `ui` objects and skip Tailscale discovery unless the
call passes `include_ui_links: true`, so the host's tailnet name stays out of
model context by default.
`dashboard_path` is always portable and remains the canonical handle for agents;
`dashboard.base_url` and the legacy dashboard-base environment variables are
retained only for compatibility and do not affect emitted agent UI links.
Tailscale Serve keeps the dashboard tailnet-only; Tailscale Funnel publishes it
on the public internet, so do not use Funnel for a mailbox dashboard unless that
exposure is intentional.

### Authentication (required before any non-loopback exposure)

On `127.0.0.1` the dashboard is an unauthenticated single-user local surface —
the same trust boundary as your shell. The REST API reads and deletes mail,
manages accounts, and queues sends, so the moment it is reachable by another
device (a non-loopback `--bind`, or a `tailscale serve` front-end) it **must**
require a credential. Configure one of:

- **Tailscale identity allowlist** (`dashboard.tailscale_allow`) — a request
  whose `Tailscale-User-Login` header (injected by `tailscale serve`) is in the
  allowlist is authorized. Humans just open the `.ts.net` URL; no token to type.
  Only safe behind `tailscale serve`, which sets and strips that header.
- **Bearer token** (`dashboard.auth_token` or `ENVELOPE_DASHBOARD_TOKEN`) —
  sent as `Authorization: Bearer <token>` (or `X-Envelope-Token`), compared in
  constant time. The path for Hermes/OpenClaw agents and scripts.

Once either is configured, every `/api` route returns `401` without a valid
credential — including when `tailscale serve` fronts loopback. `envelope serve
--bind <non-loopback>` **refuses to start** unless an auth method is configured.
`/api/health` stays reachable for liveness probes but discloses local
filesystem paths only to authorized callers. The CORS allowlist is a
browser-only defense, not the access control.

## Why not Himalaya / Cloudflare / Resend?

### vs. Himalaya

Himalaya is a great CLI email client. Envelope is a CLI email client built for agents.

| | Envelope | Himalaya |
|---|:---:|:---:|
| Compose / Reply / Forward | ✅ | ✅ |
| Inbox / Search / Folders | ✅ | ✅ |
| Move / Copy / Delete / Flag | ✅ | ✅ |
| Attachments (send + download) | ✅ | ✅ |
| JSON output | ✅ | ✅ |
| Multiple accounts | ✅ | ✅ |
| Auto-discovery (email + password, done) | ✅ | ❌ Manual config |
| Snooze + unsnooze | ✅ | ❌ |
| Threading (11-language subject normalization) | ✅ | ❌ |
| Rules engine (agent-trained junk filters) | ✅ | ❌ |
| Message scoring + tagging | ✅ | ❌ |
| Unsubscribe (RFC 8058 one-click) | ✅ | ❌ |
| Sieve export | ✅ | ❌ |
| IMAP IDLE push (`envelope watch`) | ✅ | ❌ |
| Verification code extraction | ✅ | ❌ |
| MCP server (Claude Code, Cursor, Zed) | ✅ | ❌ |
| Scheduled send (`--at`) | ✅ | ❌ |
| Contacts with rules integration | ✅ | ❌ |
| Webhook rule actions | ✅ | ❌ |
| Localhost dashboard (web UI) | ✅ | ❌ |

### vs. Cloudflare Email Service

Cloudflare's [Email Service](https://blog.cloudflare.com/email-for-agents/) (public beta, April 2026) is email infrastructure for the Cloudflare platform. Envelope is email mastery for your existing mailbox.

| | Envelope | Cloudflare Email |
|---|:---:|:---:|
| BYO mailbox (your existing email) | ✅ | ❌ Cloudflare routing |
| DNS setup required | **None** | Cloudflare DNS |
| Read inbox (full IMAP) | ✅ | ❌ Inbound routing only |
| Self-hosted | ✅ | ❌ Workers platform |
| Per-message cost | **$0** | Paid Workers plan |
| Agent-native | ✅ CLI + JSON | ✅ Workers SDK |
| Rules engine | ✅ Local + Sieve | Workers AI |
| Works offline | ✅ | ❌ Cloud-only |
| Any provider | ✅ Gmail, Fastmail, iCloud, Migadu, any password-based IMAP host | ❌ Cloudflare only |
| Open source | ✅ FSL-1.1-ALv2 | Reference app only |

### vs. Resend / Mailgun / SendGrid

| | Envelope | Resend | Mailgun | SendGrid |
|---|:---:|:---:|:---:|:---:|
| BYO mailbox | ✅ Your existing email | ❌ New domain | ❌ New domain | ❌ New domain |
| DNS setup | **None** | SPF/DKIM/DMARC | SPF/DKIM/DMARC | SPF/DKIM/DMARC |
| Per-message cost | **$0** | $0.001+ | $0.001+ | $0.001+ |
| Read inbox | ✅ Full IMAP | ❌ Send only | ⚠️ Limited | ⚠️ Limited |
| Self-hosted | ✅ | ❌ | ❌ | ❌ |
| Open source | ✅ | ❌ | ❌ | ❌ |

## Provider support

Envelope auto-discovers IMAP/SMTP from your email domain via DNS. Tested with:

| Provider | Auth | Notes |
|---|---|---|
| **Gmail** | App password | `[Gmail]/` folder prefix handled automatically |
| **Outlook.com / Microsoft 365** | Not supported yet | Microsoft requires OAuth sign-in for IMAP; Envelope has no OAuth yet |
| **Migadu** | Password | Standard folders |
| **Fastmail** | App password | Standard folders |
| **iCloud Mail** | App-specific password | Standard folders |
| **Self-hosted Dovecot** | Password | `INBOX.` dot-separator detected |
| **Generic IMAP** | Password | Anything RFC 3501 |

## MCP server

`envelope mcp` starts a Model Context Protocol server over stdio — drop-in email for Claude Code, Cursor, Zed, or any MCP runtime.

Framing follows the MCP stdio spec: one newline-delimited JSON-RPC message per line on stdout, nothing else on that stream (logs go to stderr). On stdin the server accepts newline-delimited JSON and, for callers written against versions before 1.1.0, the legacy `Content-Length:` framing — detected per message.

```bash
# Print a ready-to-paste config snippet
envelope mcp --config

# Output (paste into your MCP config):
# {
#   "mcpServers": {
#     "envelope": {
#       "command": "/path/to/envelope",
#       "args": ["mcp"]
#     }
#   }
# }
```

Tools include `inbox`, `read`, `search`, `send`, `reply`, `create_reply_draft`, `create_forward_draft`, `modify_draft`, `get_draft`, `send_draft`, `move_message`, `flag`, `folders`, `tag`, `contacts`, `accounts`, `bulk`, `thread`, `rules_preview`, `rules_run`, `watch_status`, `snooze`, plus `threat_show` and `governor_catalog`. Envelope is the only MCP email server that works against any IMAP provider.

For a single, distribution-ready operating guide to hand a fresh agent, see [the Envelope agent skill](docs/agents/envelope-skill.md).

For a walkthrough of running multiple agents from one shared inbox with scoped policies, see [Agents at a glance](docs/agent-fleet-shared-inbox.md).

## Use Envelope from your agent

Envelope ships as a plugin for Claude Code, Codex and Cursor, and as skills on
skills.sh. Each install adds two skills: `envelope` for working mail, and
`envelope-setup` for first-run setup. It also adds the local MCP server
(`envelope mcp`).

**Prerequisite:** the `envelope` binary on `PATH`. The `envelope-setup` skill
walks through this if it's missing.

```bash
curl -fsSL https://u1f4e7.com/install.sh | bash
# or
brew install tymrtn/u1f4e7/envelope
```

`tymrtn/u1f4e7/u1f4e7` is a live Homebrew compat alias for the same formula.

Then add your accounts and create a token for the agent. Run both in your own
terminal, so your password and token never pass through the agent:

```bash
envelope accounts add --email you@example.com
envelope agent create claude-code
```

Agent names are unique, and the free tier allows two active agents
(`envelope agent list` shows them). Use one name per tool, or reuse one
agent's token in several tools.

**Claude Code** (it asks for the token while installing):

```
/plugin marketplace add tymrtn/U1F4E7
/plugin install envelope@envelope
```

**Codex** (it forwards `ENVELOPE_AGENT_TOKEN` from your shell environment):

```bash
codex plugin marketplace add tymrtn/U1F4E7
codex plugin add envelope@envelope
```

Then, in an editor, add the token to the shell profile that starts Codex (for
example `~/.zshrc`). Typing it at the prompt would leave it in shell history.

```sh
export ENVELOPE_AGENT_TOKEN=<token>
```

**Cursor Marketplace plugin:** install Envelope from the marketplace, then set
`ENVELOPE_AGENT_TOKEN` under Plugins → Configure. Listing copy is
in [MARKETPLACE.md](MARKETPLACE.md).

**Skills only** (any agent that skills.sh supports):

```bash
npx skills add https://github.com/tymrtn/U1F4E7/tree/main/skills
```

| Path | Role |
|---|---|
| `skills/envelope/`, `skills/envelope-setup/` | Skills shared by every tool |
| `.claude-plugin/` | Claude Code manifest and marketplace |
| `.codex-plugin/`, `.agents/plugins/marketplace.json` | Codex manifest, MCP config and marketplace |
| `.cursor-plugin/plugin.json`, `mcp.json` | Cursor manifest and MCP config |

Plugin versions follow Envelope releases; `ci/check-agent-plugins.sh` enforces it.

## Rules engine

The agent is the intelligence. Envelope is the execution.

```bash
# 1. Agent reads inbox and scores each message
envelope inbox --json | jq -r '.[].uid' | while read uid; do
  envelope tag set "$uid" --score urgent=0.1 --score interesting=0.2 --tag newsletter
done

# 2. Agent creates rules from observed patterns
envelope rule create --name "Junk newsletters" \
  --match-tag newsletter --match-score-below interesting=0.3 \
  --action move=Junk

# 3. Preview, then explicitly confirm rule execution
# Preview is read-only; --confirm is required for mailbox mutation.
envelope rule preview
envelope rule run --confirm

# 4. Export to Sieve for server-side filtering
envelope rule export
```

The LLM teaches Envelope what to look for. Envelope applies those patterns deterministically. The LLM only re-engages when something new appears.

## rShield

rShield scores every message for phishing and malware on your machine: `clean`, `suspicious`, `dangerous`, or `unavailable` when an analyzer that had to answer could not. Six local analyzers are on by default and send nothing anywhere.

```bash
envelope threat scan              # score the newest INBOX messages
envelope threat explain 4812      # the signals and arithmetic behind a verdict
envelope threat report 4812       # draft a report to APWG (never sends)
```

Two optional analyzers are off until you enable them:

| Analyzer | Enable | What leaves the machine |
|---|---|---|
| ClamAV | `brew install clamav`, start clamd, then `envelope config set threat.clamd.address unix:/path/to/clamd.sock` | Attachment bytes, to the clamd you configured |
| Spamhaus DBL | `envelope config set threat.reputation.provider spamhaus-dbl` (optionally `threat.reputation.dqs_key`) | Sender and link domain names, as DNS queries to Spamhaus |

`threat report` also looks up abuse contacts over RDAP for the sending domain and, when there is one, the imitated domain, sending those domain names to their registries. Every outside lookup is logged as a `lookup_performed` event. Spamhaus restricts free and commercial use of its blocklists; read their [usage terms](https://www.spamhaus.org/blocklists/dnsbl-fair-use-policy/) first. Setup details, weights and the clamd walkthrough are in [docs/rshield.md](docs/rshield.md).

## Dashboard

`envelope serve` starts a localhost web UI at [http://localhost:3141](http://localhost:3141).

- Left mailbox sidebar with Unified Inbox, Today/Needs Attention, Snoozed,
  Sent, Drafts, All Mail, and account mailboxes
- Middle message list with a permanent right-side reader
- Review page for agent drafts waiting on your approval, and a Logs page with
  each agent's history
- Reply / Reply-all with automatic header threading
- Compose with text/html toggle and file attachments
- ★ Snoozed virtual folder with overdue highlighting
- Search across every account with Gmail-style operators (`from:`, `subject:`,
  `is:unread`, `before:`)

## Travel

Envelope Travel is a self-hosted itinerary and family-coordination workspace at
[`/travel`](http://localhost:3141/travel). It uses the Gmail or Google Workspace
address you already own—no forwarding address or new travel account.

1. Turn on Google 2-Step Verification and create an app-specific password.
2. Run `envelope serve`, open **Travel**, and enter the address plus that app
   password. Envelope verifies Gmail before saving the encrypted credential.
   A fresh macOS install uses Keychain automatically; an existing encrypted
   file store remains in place. On a headless Linux host, set
   `ENVELOPE_MASTER_KEY` or `ENVELOPE_MASTER_PASSPHRASE_FILE` before starting
   the service.
3. Leave confirmations in the configured Gmail folder (INBOX by default), pick
   `[Gmail]/All Mail` in Travel settings to include archived confirmations, or
   paste a confirmation into the receipt desk. The mailbox scan is read-only:
   IMAP `EXAMINE` plus `BODY.PEEK[]` never marks mail as read.

The workspace groups flights, lodging, rail, cars, and activities into trips;
keeps the source receipt and parser provenance; quarantines uncertain details
for approval; detects emailed changes and cancellations; schedules reminders;
and provides shared tasks. Family links are revocable and structurally omit
confirmation codes, receipt text, private notes, and parser metadata. Each
share also gets a separate read-only `.ics` capability, so Apple Calendar,
Google Calendar, or another subscriber never receives task-edit authority.

For a computer-only install, keep `envelope serve` running while you need sync,
alerts, family pages, or calendar refreshes; a sleeping laptop cannot poll
Gmail. For access from family devices, the recommended remote shape is the
Tailscale setup in Quick start. For an always-on Linux host, install the
systemd user service from [docs/install-linux.md](docs/install-linux.md), put
`ENVELOPE_HOME` on persistent encrypted storage, require dashboard auth, and
terminate TLS with Tailscale Serve or a trusted reverse proxy. Do not expose a
mailbox dashboard anonymously. Gmail onboarding is deliberately disabled when
Envelope binds directly to a non-loopback address: enter the app password only
through localhost, or through an HTTPS/Tailscale proxy whose Envelope listener
remains on loopback. A custom proxy must overwrite `X-Forwarded-Proto` with the
actual client-facing scheme; plaintext proxy requests and conflicting HTTP
browser origins are refused for Gmail credential setup.

Travel changes and delays currently come from messages received in Gmail.
Live gate, aircraft, baggage-belt, and operational delay telemetry requires a
separate flight-status provider and is not inferred when the airline has not
emailed an update. Scheduled alerts appear in the Travel workspace; this build
does not yet deliver push notifications when the browser is closed.

## Commercial licensing

Personal use is free. Commercial rollout is a flat annual license sized by the number of mailbox users on one primary domain:

| Plan | Price | Scope | Support |
|---|---:|---|---|
| Personal | Free | Individual, non-commercial use on mailboxes you personally own and operate | Community support via GitHub issues |
| Team | $240/year | Commercial use for one organization, up to 10 mailbox users on one primary domain | Email support, best-effort next-business-day response |
| Growth | $960/year | Commercial use for one organization, up to 25 mailbox users on one primary domain | Priority email support, next-business-day response target |
| Enterprise | Contact us | 26+ users, multi-domain deployments, embedded/OEM, reseller terms, or custom security review | Custom support agreement |

"Commercial use" means any use of Envelope by or for a company, team, agency, or paid project, including internal operations, customer workflows, or embedding Envelope in a product. "Mailbox users" means distinct human or service mailbox identities on the licensed domain that Envelope is configured to read, send, or automate. One person with multiple aliases on the same mailbox counts as one user. "Domain" means one primary email domain; subdomains of that primary domain are included, and additional unrelated domains require Enterprise.

Licenses are annual, non-transferable across organizations, and renew on the anniversary of issue. Request an annual license at [ty@tmrtn.com](mailto:ty@tmrtn.com?subject=Envelope%20licensing).

## Evidence bundles

`envelope evidence` collects a query-scoped, local evidence bundle while leaving the source mailbox read-only. Collection opens the requested folder with IMAP `EXAMINE` and fetches canonical raw RFC822 originals with `BODY.PEEK[]`.

```bash
envelope evidence collect \
  --account user@example.com \
  --folder '[Gmail]/All Mail' \
  --query 'FROM "sender@example.com" SUBJECT "contract"' \
  --include-thread \
  --out ./evidence-bundle

envelope evidence verify --from ./evidence-bundle
envelope evidence verify --from ./evidence-bundle --strict
```

Structured filters can replace or combine with `--query`: `--from-address`, `--to-address`, `--subject`, `--since`, `--before`, `--body`, and repeatable `--keyword`. At least one filter is required unless the raw query is explicitly `ALL`.

Thread expansion is header-driven and bounded. By default, `--include-thread` fetches at most 500 messages while following Message-ID, In-Reply-To, and References links; adjust with `--max-thread-messages`. If the cap is reached, the bundle records a warning.

Evidence bundles intentionally expose message metadata, account identity, IMAP host/username, and local source paths for provenance. Treat bundles as sensitive even though they do not serialize passwords, OAuth tokens, or credential file paths.

Bundle layout:

```text
evidence-bundle/
  manifest.json
  index.csv
  README.md
  SHA256SUMS
  bundle.sha256
  messages/<encoded-folder>/<uidvalidity>-<uid>.eml
```

Thread inclusion is driven only by `Message-ID`, `In-Reply-To`, and `References`; subject matching is never used as a fallback. Hash files provide local tamper evidence, not an external signature. Non-goals for the MVP: ZIP packaging, detached signatures, PST/mbox conversion, multi-account collection, mailbox mutation, and restore/import into a mailbox.

## CLI reference

| Command | Description |
|---|---|
| `envelope accounts add/list/remove` | Manage accounts (auto-discovers hosts) |
| `envelope folders` | List folders with unread/total counts |
| `envelope inbox [--folder] [--limit]` | List messages |
| `envelope read <uid>` | Read a message (BODY.PEEK — no auto-mark-read) |
| `envelope search "<query>"` | IMAP search |
| `envelope send --to --subject --body [--attach]` | Queue/send email through the outbox (and the Governor gate in `governor` builds) |
| `envelope move/copy/delete <uid>` | Message management |
| `envelope flag add/remove <uid> <flag>` | IMAP flags |
| `envelope attachment list/download <uid>` | Attachments |
| `envelope draft create/list/send/discard` | Drafts (IMAP-backed) |
| `envelope snooze set/list/cancel` | Snooze with flexible time parsing |
| `envelope unsnooze [--once]` | Return due snoozed messages |
| `envelope thread show/list/build` | Conversation threads |
| `envelope tag set/show/list` | Score and tag messages |
| `envelope rule create/list/test/run/export` | Mail rules (webhook actions supported) |
| `envelope backup export/verify/restore` | Stage a mailbox to a local RFC822 archive (offline verify, append-only restore) |
| `envelope evidence collect/verify` | Query-scoped RFC822 evidence bundle with offline verification |
| `envelope unsubscribe <uid> [--confirm]` | List-Unsubscribe (dry-run default) |
| `envelope watch [--webhook] [--json]` | IMAP IDLE push — real-time new mail events |
| `envelope code [--from] [--wait 120]` | Extract verification/OTP codes; `--json` automation requires `--account` plus an exact mailbox/full-domain `--from` binding and stabilizes before returning |
| `envelope paths` | Show resolved database/credential paths and HOME drift warnings |
| `envelope contract [--surface <name>]` | Export the versioned agent JSON/MCP contract |
| `envelope mcp [--config]` | MCP server (stdio) for Claude Code, Cursor, Zed |
| `envelope send --at "monday 9am"` | Scheduled send with flexible datetime |
| `envelope scheduled list/hold/cancel` | Manage scheduled messages (`hold` unqueues and keeps the draft; `cancel` discards it) |
| `envelope contacts add/list/show/tag/import` | Contact store with rules integration |
| `envelope serve` | Localhost dashboard |

Every command supports `--json` for agent consumption.

## Architecture

```
┌──────────────┐     ┌────────────────────────────┐     ┌──────────────┐
│  AI Agent    │────▶│        Envelope (Rust)      │────▶│  Your SMTP   │
│              │     │                              │     │  (Gmail,     │
│  CLI / JSON  │     │  crates/cli       binary     │     │   Migadu,    │
│              │◀────│  crates/email     IMAP/SMTP  │◀────│   Fastmail)  │
│              │     │  crates/store     SQLite      │     │              │
│              │     │  crates/dashboard web UI      │     │  IMAP/SMTP   │
└──────────────┘     └────────────────────────────┘     └──────────────┘
```

## Development

```bash
cargo build                # Build all crates
cargo build --release      # Optimized release binary
cargo test --all --features governor   # Also test with the Governor gate built in
cargo test                 # 194 tests, 0 failures
cargo clippy               # Lint
./ci/check-orphans.sh      # Verify every .rs file is reachable via mod
```

See [CHANGELOG.md](CHANGELOG.md) for per-release notes.

## License

[FSL-1.1-ALv2](LICENSE) — source-available, no competing services.
Separate annual commercial licenses are available for organizational rollout,
support, Enterprise/OEM terms, and uses outside the public license. The public
license becomes Apache 2.0 two years after each release.

Copyright © 2026 Tyler Martin.

---

<p align="center">
  <strong>Built by <a href="https://github.com/tymrtn">Tyler Martin</a></strong><br>
  <em>Your agent shouldn't need a $50/month Resend plan to send an email.</em>
</p>
