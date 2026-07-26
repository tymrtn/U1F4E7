# AgentMail teardown

_Researched 2026-07-26. Sources are public: agentmail.to, docs.agentmail.to, their
Launch HN thread, and third-party comparison roundups. Everything attributed to
AgentMail below is from their own published material; everything attributed to
Envelope is verified against this repo at `be98752`._

---

## 1. What they are

**AgentMail (YC S25) — "Email inbox API for AI agents."** Gmail-for-agents:
one API call mints a real, persistent inbox on `agentmail.to` (or a verified
customer domain), and the agent sends, receives, threads, searches, and replies
from its own address. No OAuth, no human account behind the mailbox, no per-seat
pricing.

- **Funding:** $6M seed led by General Catalyst; YC, Paul Graham, Dharmesh Shah,
  Paul Copplestone, Karim Atiyeh, Taro Fukuyama participating.
- **Traction claimed:** 500+ business customers, "hundreds of thousands of agent
  accounts."
- **The ad that prompted this analysis:** an X promo pairing AgentMail with
  Hermes — _"One inbox unlocks the whole internet for Hermes. Signups, OTPs,
  OAuth."_ That's the sharpest articulation of their wedge: an inbox is the
  credential an agent needs to bootstrap itself onto the internet.

### Product surface (published)

| Capability | Notes |
|---|---|
| Programmatic inbox creation | `POST /inboxes` — `username` (random if omitted), `domain` (defaults `agentmail.to`, verified domains + subdomains), `display_name`, `client_id`, `metadata` |
| Domains | Customer domain verification, DKIM/SPF/DMARC provisioning |
| Threading / messages / attachments | Real inbox semantics, not just SMTP relay |
| Realtime inbound | Webhooks **and** websockets |
| Search | Full-text; semantic search advertised in the Launch HN post |
| Enrichment | Automatic labeling, attachment text extraction, structured data extraction |
| SDKs | Python, TypeScript |
| MCP | Native (hosted) MCP server |
| IMAP + SMTP | `imap.agentmail.to` (IDLE, RFC 2177), `smtp.agentmail.to` :465 SSL / :587 STARTTLS. **Username = the inbox address, password = the API key.** Limits: 50 recipients/message, 10 MB/message |
| Multi-tenant | "Pods" for platform builders; BYO-cloud at enterprise |

### Pricing (published)

| Plan | Price | Inboxes | Email/mo | Storage |
|---|---:|---:|---:|---:|
| Free | $0 | 3 | 3,000 (100/day) | 3 GB |
| Developer | $20/mo | 10 | 10,000 | 10 GB |
| Startup | $200/mo | 150 | 150,000 | 150 GB |
| Enterprise | Custom | Unlimited | Custom | Custom |

Effective rate: ~$2.00 per 1k emails (Developer), ~$1.33 per 1k (Startup).
Custom domains are paid-tier only.

---

## 2. Why they're winning the narrative

1. **One-call provisioning.** `create_inbox()` → a working address. Envelope's
   equivalent is "go generate an app password in your provider's security
   settings," which is a human, a browser, and 90 seconds — per mailbox.
2. **Category ownership.** They named the category ("email inboxes for AI
   agents") and are buying the keyword. Comparison roundups now use *them* as
   the reference implementation and sort everyone else relative to them.
3. **Zero prerequisites.** No mailbox, no domain, no DNS, no provider account.
   The floor for "hello world" is an API key.
4. **Developer distribution.** SDKs + hosted MCP + free tier + YC network. They
   sell to the people *building* agent products, who then carry them into
   production.
5. **Capital.** $6M buys abuse ops, IP reputation management, and a docs team.

## 3. Where they're structurally exposed

These are not potshots — each one is a load-bearing constraint of the hosted
model, and each maps to an Envelope advantage that is expensive for them to
neutralize.

1. **Shared-tenant sender reputation.** Every customer's agent sends from
   infrastructure shared with every other customer's agent. The top criticism in
   their own Launch HN thread was deliverability and abuse: SPF/DKIM/DMARC do not
   buy inbox placement, and a fraud-adjacent tenant degrades the pool. Envelope
   sends from *your* mailbox with *your* decade-old domain reputation. This gap
   widens as the category attracts spammers.
2. **A new address nobody trusts.** `agent-a3f9@agentmail.to` is a cold identity.
   For a *support*, *procurement*, *recruiting*, or *legal* workflow — where the
   counterparty needs to recognize the sender — an unrecognized address is a
   business problem, not a technical one.
3. **Per-message economics.** $1.33–$2.00 per 1k messages is cheap until an agent
   loops. Envelope's marginal cost per message is $0 because the mailbox is
   already paid for.
4. **The mail you already have is out of reach.** Their model starts a mailbox
   from zero. It cannot triage the ten years of correspondence already sitting in
   your Gmail — the actual job most people want an agent to do.
5. **Custody.** Message bodies, attachments, and threads live on their
   infrastructure. That's a hard "no" for legal, healthcare, finance, and
   anyone with a data-residency clause. BYO-cloud at enterprise is the
   acknowledgement, not the fix.
6. **An inbox is not a permission.** They ship provisioning; they do not ship
   authority. There is no published per-agent policy clamp, no send-mode
   ceiling, no attributed audit trail of which agent did what. Their answer to
   prompt injection on HN was "allowlists and permissions" — i.e. an
   acknowledged open problem. Envelope's send-policy ladder, per-agent policy,
   Governor gate, and `actions tail` are exactly that missing layer.
7. **They opened the door to us.** IMAP + SMTP with the API key as password
   means **an AgentMail inbox is an Envelope account today.** Their growth is
   addressable surface for us, not lost ground. See the interop wedge in the
   roadmap.

## 4. Honest scorecard

Where they genuinely beat us today — this is the gap list the roadmap is built
from:

| | AgentMail | Envelope (today) |
|---|---|---|
| Mint an inbox from code | ✅ one API call | ❌ BYO mailbox, human-provisioned app password |
| Per-agent email address | ✅ inbox per agent | ❌ agents share one mailbox identity |
| Hosted HTTP API | ✅ public, versioned, documented | ⚠️ dashboard REST exists (`crates/dashboard/src/lib.rs`) but is UI-facing, undocumented as a product API |
| SDKs | ✅ Python + TS | ❌ CLI/JSON + MCP only |
| Remote MCP | ✅ hosted | ❌ stdio only (`crates/cli/src/mcp.rs`) |
| Semantic search | ✅ | ❌ IMAP `SEARCH` only (`crates/cli/src/commands/search.rs`) |
| Attachment text extraction | ✅ | ❌ download bytes only |
| Structured extraction / auto-labeling | ✅ managed | ⚠️ agent-supplied via `tag`/`rule` (by design) |
| Managed deliverability (DKIM/SPF/DMARC provisioning) | ✅ | ⚠️ `envelope deliverability` audits DNS, provisions nothing |
| OAuth / modern auth | ✅ N/A (API key) | ❌ app passwords only — blocked where an org disables them |
| Docker / one-click deploy | ✅ hosted | ❌ no image |
| Free tier for developers | ✅ 3 inboxes, 3k msgs | ✅ free for personal use, 2 agent identities |

Where we beat them, and they cannot easily copy:

| | Envelope | AgentMail |
|---|---|---|
| Your existing mailbox + reputation | ✅ any IMAP | ❌ |
| Marginal cost per message | **$0** | $1.33–$2.00 / 1k |
| Runs offline / self-hosted / air-gapped | ✅ | ❌ |
| Message bodies never leave your infra | ✅ | ❌ |
| Per-agent identity, policy clamp, send-mode ceiling | ✅ | ❌ |
| Attributed audit trail per agent action | ✅ `actions tail --agent` | ❌ |
| Fail-closed send gate (Governor) | ✅ | ❌ |
| Human-in-the-loop draft approval queue | ✅ Agent Cockpit | ❌ |
| Deterministic rules engine + Sieve export | ✅ | ⚠️ managed labeling |
| Read-only forensic evidence bundles | ✅ `envelope evidence` | ❌ |
| Snooze, threading, unsubscribe, scheduled send | ✅ | ⚠️ partial |
| Governs *their* inboxes over IMAP | ✅ | — |

## 5. Strategic read

**Do not fight them on provisioning.** Hosted inbox supply is a capital and
abuse-operations business: IP pools, reputation, blocklist relationships, a
trust-and-safety team. They raised $6M for exactly that. We would lose, slowly
and expensively.

**Fight them on authority.** They answered "how does an agent get an inbox."
Nobody has answered "how do you let an agent touch the inbox that already runs
your business, and prove afterwards what it did." That question gets *more*
urgent as agent email volume grows — and it's the question our existing agent
identity, policy clamp, Governor gate, and audit trail already answer.

Two markets, one sentence each:

- **AgentMail:** _the agent is a new participant that needs an address._
- **Envelope:** _the agent is a delegate acting on an identity you already own._

The categories converge from both sides — they'll add connected accounts, our
users will want per-agent addresses. The roadmap closes our side of that
convergence without taking on their cost structure, and the interop wedge turns
their installed base into ours.

---

_See `roadmap-agent-mail-parity.md` for the build plan and
`gtm-competitive-campaign.md` for the campaign._
