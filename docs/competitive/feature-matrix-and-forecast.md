# Full feature matrix + competitive forecast

_Envelope vs. AgentMail vs. Cloudflare Email Service. Researched 2026-07-26.
Envelope column verified against this repo at `c4749a7`. Competitor columns
come from published docs, blogs, and pricing pages only. Forecast sections are
labeled inference and should be re-scored quarterly._

Legend: ✅ shipped · ⚠️ partial / caveated · ❌ absent · 💰 paid tier only ·
🔜 on our roadmap (with phase) · 🏗️ you build it yourself on their primitives

---

## 1. What each product actually is

| | Envelope | AgentMail | Cloudflare Email Service |
|---|---|---|---|
| One-line | Agent runtime over mailboxes you already own | Hosted inboxes minted by API | Transactional send + inbound routing on the Workers platform |
| Shape | Rust binary + SQLite, self-hosted | SaaS API (AWS SES underneath) | Platform primitive, two halves: Email Routing (in) + Email Sending (out) |
| Mailbox lives | Your existing provider | Their infrastructure | **Nowhere** — routing forwards, it doesn't store |
| Identity model | Human owns it, agents get delegations | Agent owns its own address | Domain-level addresses routed to Workers or forwarded on |
| Business model | Flat annual license | Metered per message | Metered per message, priced to sell Workers compute |
| Status | v1.0.0 | GA | Sending in public beta, Routing GA since 2021 |

The most important row is **"mailbox lives"**. Cloudflare has no inbox. Their
"Agentic Inbox" is an open-source *reference app* you deploy yourself — Email
Routing for inbound, Workers AI to classify, R2 for attachments, D1 for state,
Agents SDK for logic. That's a kit, not a product, and it's the single biggest
gap in their offering.

---

## 2. Feature matrix

### Provisioning and identity

| Capability | Envelope | AgentMail | Cloudflare |
|---|:--:|:--:|:--:|
| Mint an address from code, no human | ❌ 🔜 P1 | ✅ | ⚠️ Routing rules via API; no mailbox behind it |
| Works with an existing mailbox | ✅ any IMAP | ❌ | ⚠️ forwards into one |
| Address on a domain you already own | ✅ | 💰 verified domain | ✅ |
| Free default sending domain | ❌ by design | ✅ `agentmail.to` | ❌ your domain |
| Default domain flagged disposable | n/a | ⚠️ `disposable: true` (UserCheck) | n/a |
| Unlimited addresses at $0 | 🔜 P1 (sub-addressing) | ❌ inbox-capped | ✅ catch-all routing, free |
| Per-agent identity with distinct auth | ✅ `envtok_`, revocable | ❌ API key per org | ❌ |
| Per-agent policy clamp | ✅ | ❌ | ❌ |
| OAuth / modern auth to a provider | ❌ 🔜 P2.6 | n/a | n/a |
| App-password onboarding friction | ⚠️ yes | ✅ none | ✅ none |

### Reading mail

| Capability | Envelope | AgentMail | Cloudflare |
|---|:--:|:--:|:--:|
| Persistent inbox storage | ✅ provider + local index | ✅ | ❌ 🏗️ R2/D1 |
| Read existing/historical mail | ✅ full IMAP | ❌ starts empty | ❌ |
| Threading | ✅ 11-language subject normalization | ✅ | 🏗️ |
| Folders, labels, unread counts | ✅ | ⚠️ labels | ❌ |
| Full-text search | ⚠️ IMAP `SEARCH` only, 🔜 P3 local FTS | ✅ | 🏗️ |
| Semantic search | ❌ 🔜 P3 (local, opt-in) | ✅ hosted | 🏗️ Vectorize |
| Attachment download | ✅ | ✅ | 🏗️ R2 |
| Attachment text extraction | ❌ 🔜 P3 | ✅ | 🏗️ Workers AI |
| Structured data extraction | ⚠️ agent-supplied via tags/rules | ✅ managed | 🏗️ Workers AI |
| Read without marking read | ✅ `BODY.PEEK` everywhere | ⚠️ unspecified | n/a |
| Offline operation | ✅ | ❌ | ❌ |

### Sending

| Capability | Envelope | AgentMail | Cloudflare |
|---|:--:|:--:|:--:|
| Send | ✅ your SMTP | ✅ | ✅ 💰 Workers Paid |
| Reply in-thread with correct headers | ✅ | ✅ | 🏗️ (HMAC reply routing provided) |
| Drafts | ✅ IMAP-backed | ✅ | ❌ |
| Scheduled send | ✅ `--at` | ❌ | 🏗️ Cron/Queues |
| Undo window / cooldown | ✅ | ❌ | ❌ |
| Outbox-first queueing | ✅ | ❌ | ❌ |
| Sender reputation | your domain, existing | shared tenant pool | Cloudflare-managed IPs |
| SPF/DKIM/DMARC setup | ⚠️ audit only (`deliverability`) | ✅ managed | ✅ automatic |
| Per-message cost | **$0** | ~$1.33–2.00 / 1k | $0.35 / 1k after 3k |
| Recipient cap per message | provider's | 50 | provider limits |

### Oops protection — the safety layer

| Capability | Envelope | AgentMail | Cloudflare |
|---|:--:|:--:|:--:|
| Draft-only default in agent contexts | ✅ | ❌ | ❌ |
| Send-mode ladder (draft → confirm → allowlist → autonomous) | ✅ | ❌ | ❌ |
| Recipient allowlist enforced pre-send | ✅ | ⚠️ allowlists doc'd, scope unclear | ❌ |
| Human approval queue | ✅ Agent Cockpit | ❌ | ❌ |
| Fail-closed send gate | ✅ Governor | ❌ | ❌ |
| Per-agent attributed audit trail | ✅ `actions tail --agent` | ❌ | ❌ |
| Revoke one agent, not the fleet | ✅ | ❌ | ❌ |
| Spend ceiling / runaway-loop stop | ✅ n/a (flat) | ❌ | ❌ |
| Prompt-injection provenance tagging | ❌ 🔜 P4 | ❌ | ⚠️ owns Area 1 email security — see forecast |
| Read-only forensic evidence bundles | ✅ | ❌ | ❌ |

**This block is the product.** Nine of eleven rows are ours alone, and the two
that aren't are a roadmap item and a capability Cloudflare owns but hasn't
pointed at agents yet.

### Automation and rules

| Capability | Envelope | AgentMail | Cloudflare |
|---|:--:|:--:|:--:|
| Deterministic rules engine | ✅ | ⚠️ auto-labeling | 🏗️ Workers code |
| Blast-radius preview before applying | ✅ | ❌ | ❌ |
| Sieve export (server-side filtering) | ✅ | ❌ | ❌ |
| Snooze / unsnooze | ✅ | ❌ | ❌ |
| Message scoring + tagging | ✅ | ⚠️ labels | 🏗️ |
| Contacts store wired to rules | ✅ | ❌ | ❌ |
| RFC 8058 one-click unsubscribe | ✅ | ❌ | ❌ |
| OTP / verification-code extraction | ✅ `code --wait` | ⚠️ via search | 🏗️ |
| Bulk operations with dry-run | ✅ 500-cap | ⚠️ API loops | 🏗️ |

### Integration surface

| Capability | Envelope | AgentMail | Cloudflare |
|---|:--:|:--:|:--:|
| CLI | ✅ every command `--json` | ⚠️ | ✅ Wrangler |
| REST API | ⚠️ dashboard-facing, 🔜 P2 public | ✅ | ✅ |
| TypeScript SDK | ❌ 🔜 P2.4 | ✅ | ✅ |
| Python SDK | ❌ 🔜 P2.4 | ✅ | ✅ |
| Go SDK | ❌ | ❌ | ✅ |
| MCP server | ✅ stdio, 22 tools | ✅ hosted | ✅ |
| Remote/HTTP MCP | ❌ 🔜 P2.3 | ✅ | ✅ |
| Webhooks | ✅ HMAC, retry, dead-letter | ✅ | 🏗️ Workers |
| WebSockets / streaming | ⚠️ SSE | ✅ | 🏗️ Durable Objects |
| IMAP IDLE push | ✅ | ✅ | ❌ |
| Native agent-framework hook | ⚠️ MCP | ⚠️ MCP | ✅ Agents SDK `onEmail` |
| Versioned machine-readable contract | ✅ `envelope contract` | ❌ | ❌ |

### Operations, custody, compliance

| Capability | Envelope | AgentMail | Cloudflare |
|---|:--:|:--:|:--:|
| Self-hostable | ✅ | ⚠️ BYO-cloud 💰 enterprise | ❌ |
| Air-gapped / offline | ✅ | ❌ | ❌ |
| Bodies never leave your infra | ✅ | ❌ | ❌ |
| Data residency control | ✅ your box | 💰 | ⚠️ regional |
| Vendor outage = no mail access | ❌ local cache survives | ✅ total | ✅ total |
| Address portability if you leave | ✅ your domain | ❌ address is theirs | ✅ your domain |
| Retention / legal hold | ⚠️ evidence bundles, 🔜 P4 | ❌ | 🏗️ |
| Human webmail UI | ✅ embedded dashboard | ⚠️ console | ❌ |
| Docker image | ❌ 🔜 P2.5 | n/a | n/a |
| Source available | ✅ FSL-1.1-ALv2 | ❌ | ⚠️ reference app only |

### Adjacent players, one line each

| Product | What it is | Why it's not the same fight |
|---|---|---|
| Resend / Postmark | Transactional send, great DX | No inbox, no reading, no governance |
| AWS SES | Wholesale send + inbound to S3 | The layer AgentMail is built on |
| Nylas | OAuth-connected Gmail/Outlook for apps | Closest to our BYO model; enterprise-priced, per-account, no agent governance |
| Gmail API | One Workspace tenant | Per-seat OAuth, no programmatic provisioning |
| Himalaya | CLI mail client | No agents, no policy, no audit |
| Inbound / Dead Simple Email | AgentMail-shaped startups | Same category, less capital |

---

## 3. Normalized cost at volume

Assumes one domain and, for Envelope, a mailbox you already pay for.

| Monthly agent volume | Envelope | AgentMail | Cloudflare |
|---|---:|---:|---:|
| 3k | $0–240/yr | $0 | $60/yr (Workers Paid, within free 3k) |
| 10k | $240/yr | $240/yr | $89/yr |
| 50k | $240–960/yr | $2,400/yr | $257/yr |
| 150k | $240–960/yr | $2,400/yr | $677/yr |
| 500k | $960/yr | negotiated | $2,148/yr |
| 5M | $960/yr | negotiated | $21,047/yr |

Two things fall out of this table, and they point in opposite directions.

**Cloudflare is 4–6× cheaper than AgentMail on transport and will get cheaper.**
Sending is a loss-leader for Workers compute. AgentMail's per-message premium is
paying for inbox semantics, not delivery — and that's a bundle Cloudflare can
unbundle at will.

**We're cheapest at every volume above ~10k, and flat.** But note the honest
reversal at the bottom: at trivial volume with no existing mailbox, both
competitors are cheaper than a commercial Envelope license. Don't fight for that
customer.

---

## 4. Product profiles — the DNA that predicts the roadmap

### AgentMail

Seed-stage, YC S25, $6M from General Catalyst, three founders, developer-led
growth, revenue proportional to message volume. Ships fast, markets hard, buys
category keywords. Their published traction (500+ business customers) implies
they're past experimentation and into land-and-expand.

**Structural pressures:**
- Metered revenue means they need volume, which means they need customers whose
  agents send a lot, which means enterprise, which means procurement asking
  security questions they currently can't answer.
- Their default domain is classified disposable, which attacks their headline
  signup/OTP use case from the outside.
- Greenfield-only TAM. Every serious customer eventually asks "can it use our
  real support inbox?"
- Cloudflare undercuts them 4–6× on transport with a free inbound tier.

### Cloudflare

Platform company. Ships primitives, not products. Prices adjacent services near
zero to pull developers onto Workers. Never builds an end-user application when
it can publish a reference app instead. Owns Area 1 (email security), the
Agents SDK, R2, D1, Vectorize, Workers AI, and the MCP tooling — every piece of
an inbox except the inbox.

**Structural pressures:**
- Agents Week positioning means email-for-agents has internal executive
  sponsorship; it will keep getting investment.
- They cannot ship IMAP or a webmail client — wrong company, wrong shape.
- Their strategic interest is commoditizing the layer *below* AgentMail so
  compute demand rises. AgentMail's margin is a rounding error to them.

---

## 5. What they ship next — inference, with confidence

Scored by likelihood in the next 12 months and by how much it hurts us.

### AgentMail

| Prediction | Confidence | Threat to us |
|---|:--:|:--:|
| **Connected accounts — OAuth into Gmail/Outlook so agents use your real mailbox** | **70%** | **High** — collides head-on with BYO-mailbox |
| Dedicated IPs / subdomain warmup / reputation dashboards | 65% | Low |
| Thin guardrails: approval webhook, allowlists, spend caps | 50% | **High** — attacks the oops-protection story |
| Framework integrations (LangChain, CrewAI, Vercel AI SDK, OpenAI Agents SDK) | 60% | Medium — distribution |
| Email-as-agent-memory: thread recall, contact graph, cross-inbox retrieval | 45% | Medium |
| SOC 2 / EU residency / self-serve BYO-cloud | 40% | Medium — unlocks the deals we win today |
| Multi-channel identity (SMS/voice for the same agent) | 30% | Low |
| Price cuts under Cloudflare pressure | 35% | Low |

The one that matters is the first. **If AgentMail ships OAuth connected
accounts, "use your existing mailbox" stops being our exclusive claim** and our
differentiation narrows to custody, governance depth, and flat pricing. That's
still defensible — a hosted service reading your Gmail is a different custody
proposition than a binary on your box — but the pitch gets harder. Their thin
guardrails release is the tell that it's coming: governance shows up on the
roadmap right when enterprise procurement starts asking.

### Cloudflare

| Prediction | Confidence | Threat to us |
|---|:--:|:--:|
| **Mailbox storage primitive** (first-party inbox binding, not a reference app) | **55%** | Low to us, **existential to AgentMail** |
| Email Sending GA + price cut | 75% | Low |
| Deeper Agents SDK email tooling, first-class MCP inbox tools | 70% | Medium — distribution |
| Programmatic address minting API on your zone | 50% | Medium — undercuts our P1 |
| **Injection/threat filtering for agent email** (Area 1 pointed at agents) | 45% | **High** — that's our P4 |
| Inbound attachment parsing via Workers AI | 55% | Low |
| Vectorize-backed thread search recipe | 50% | Low |
| IMAP access | <5% | — |
| Human webmail client | <5% | — |

The dangerous one is injection filtering. Cloudflare has a mature email-security
business, an edge to run it on, and every reason to extend it to agent traffic.
If they ship it before our Phase 4, we lose first-mover on the safety narrative
and get reduced to "the local version." **Phase 4.1 should move up.**

---

## 6. Where the market lands — 12 to 24 months

Stop thinking of this as one market. It's three layers, and they'll be won
separately:

```
┌─ GOVERNANCE ── policy, approval, attribution, custody, evidence
│                CONTESTED-BUT-EMPTY. No credible incumbent. ← us
├─ INBOX SEMANTICS ── storage, threading, search, extraction
│                AgentMail leads today. Cloudflare closes it, probably free.
└─ TRANSPORT + ADDRESSES ── MX, IPs, reputation, deliverability
                 Commodity. Cloudflare and SES race to zero.
```

**Transport is already commodity** and heading to roughly $0.10/1k. Nobody
builds a durable business there without owning the network.

**Inbox semantics is where AgentMail's premium lives, and it's the layer most
likely to be squeezed.** Threading, search, and parsing are a quarter of work
for a competent team, and Cloudflare has published the recipe. AgentMail's real
defense isn't features — it's provisioning ergonomics and the fact that most
teams don't want to assemble D1 + R2 + Workers AI themselves. That's worth
something, but it isn't worth 4× the transport price forever.

**Governance is empty and getting more valuable.** Every incident where an agent
emails the wrong person raises the value of the layer that stops it. Nobody in
the top three has shipped it. This is the whole thesis.

**Share, roughly and directionally:** hosted agent inboxes stay a small,
fast-growing niche dominated by greenfield agent startups; Cloudflare takes the
volume from anyone already on Workers; the far larger population — companies
with mailboxes that already work, who want agents on *those* — is barely served
by anyone. That last group is our market and it's the biggest of the three.

---

## 7. Opportunities

### 7.1 The free unlimited-address recipe (do this immediately)

Cloudflare Email Routing is **free, unlimited, catch-all capable, and forwards
to any destination**. Combine it with Envelope:

```
*@agents.yourdomain.com  ──CF Email Routing (free)──▶  your real mailbox
                                                            │
                                                    Envelope reads it,
                                                    routes by Delivered-To,
                                                    one agent per address
```

Unlimited per-agent addresses on your own domain, $0/month, no new vendor
holding your mail. It's the AgentMail pitch, free, with reputation you already
own — and it makes Cloudflare a supplier rather than a competitor.

Caveat to state plainly: outbound *from* those addresses needs your mailbox
provider to support send-as with aligned DKIM (Fastmail, Migadu, and Google
Workspace all do; some don't). Test and document per provider.

This should be a headline recipe in Phase 1 and a launch asset — it's a
five-minute setup that beats a funded product on its core promise.

### 7.2 Be the governance layer for all three

Interop, not war. Envelope already governs AgentMail over IMAP (Phase 0). Add an
inbound HTTP ingestion endpoint and it governs Cloudflare-routed mail too. The
positioning writes itself: **bring your own supply, we'll bring the brakes.**

New roadmap item — **Phase 2.7: inbound ingestion adapter.** An authenticated
HTTP endpoint that accepts an RFC822 message from a Cloudflare Worker (or any
webhook source) and lands it in the local index with the same rules, tagging,
and audit path as IMAP-sourced mail. Small, and it unlocks the entire
non-IMAP world.

### 7.3 Own the safety vocabulary before Cloudflare does

Move Phase 4.1 (injection provenance) and 4.5 (conformance suite) earlier. If
there's a public benchmark for agent-mail safety and we wrote it, Cloudflare
shipping edge filtering becomes validation of our category instead of
displacement. If they ship first, we're a footnote.

### 7.4 Target AgentMail's graduating customers

Their customers hit three walls in order: the disposable-domain flag, the bill
at scale, and their security team. Each wall is a migration trigger. Own the
search results for all three: "agentmail alternative", "agent email compliance",
"agent email approval workflow", "agentmail pricing at scale".

### 7.5 Nylas is the flank nobody's watching

Nylas does connected accounts for apps — closest thing to our BYO model — but
it's enterprise-priced, per-account, and has no agent governance. If AgentMail
ships OAuth, they're competing with Nylas, not us. Worth watching whether Nylas
adds agent primitives; they have the connections and the enterprise contracts.

---

## 8. Tripwires

Pre-decided responses, so we react in days rather than debating for a month.

| If this happens | Then |
|---|---|
| AgentMail ships OAuth connected accounts | Stop leading with "your existing mailbox." Lead with custody + oops protection + flat pricing. Publish the custody comparison the same week. |
| AgentMail ships an approval queue | Publish the depth comparison: ladder vs. toggle, per-agent clamps, evidence bundles. Accelerate Phase 4. Do not concede the category — concede the feature. |
| Cloudflare ships first-party inbox storage | Ship the ingestion adapter (2.7) within the quarter and publish "govern your Cloudflare inbox." Their storage becomes our supply. |
| Cloudflare ships agent email threat filtering | Reposition Phase 4 as local + provider-agnostic; emphasize it works on Gmail, Fastmail, and self-hosted Dovecot, not just their edge. |
| Transport price drops below $0.10/1k | Retire the cost argument as a lead; keep it as a footnote. Lead with custody and control. |
| A public agent-email incident (fleet sends spam / leaks data) | Ship the response within 48h. This is the moment oops protection sells itself. Have the post pre-drafted. |
| AgentMail raises a Series A | Expect a governance/enterprise push within two quarters. Pull Phase 4 forward. |

---

## 9. What this changes in the roadmap

| Change | Why |
|---|---|
| **Add Phase 2.7** — inbound HTTP ingestion adapter | Unlocks Cloudflare-routed mail and any non-IMAP source; makes "govern any supply" literally true |
| **Pull Phase 4.1 forward** (injection provenance) | Cloudflare may point Area 1 at agent mail within the year |
| **Pull Phase 4.5 forward** (conformance suite) | Whoever writes the benchmark defines the category |
| **Promote the CF Routing recipe into Phase 1** | Free unlimited per-agent addresses today, no code required |
| **Keep Phase 2.6 (OAuth) at top priority** | Also our defense if AgentMail ships connected accounts |
| **Deprioritize semantic search (3.3)** | Both competitors have it and neither wins deals with it; local FTS (3.1) is the real gap |
