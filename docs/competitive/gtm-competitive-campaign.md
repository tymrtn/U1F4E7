# GTM — competitive campaign against the hosted agent-inbox category

_Companion to `agentmail-teardown.md` and `roadmap-agent-mail-parity.md`.
Campaign window: 6 weeks, aligned to v1.1 → v1.2._

---

## 1. The strategic frame

AgentMail asked and answered: **"how does an agent get an inbox?"**

That question has a ceiling. Once every agent has an address, the question that
replaces it is: **"how do you let an agent touch the inbox your business
already runs on — and prove afterwards what it did?"**

Nobody owns that question. We already ship the answer: per-agent identities,
policy clamps, send-mode ceilings, a fail-closed Governor gate, a
human-in-the-loop approval queue, and an attributed audit trail. The campaign's
job is to make that question the one the market is asking.

**We are not the cheap alternative to AgentMail. We are the layer above it.**

### Positioning statement

> Envelope is the governed mailbox runtime for AI agents. It gives agents
> scoped, attributed, revocable authority over mailboxes you already own — any
> IMAP provider, self-hosted, $0 per message — so a fleet of agents can run your
> real email without you losing control of it.

### Category we're naming

**Agent mail governance.** Hosted inboxes are *supply*. Governance is *control*.
Every asset should reinforce that these are different layers, not competing
products — because the moment a buyer accepts that framing, we stop being
compared on inbox provisioning and start being compared on a dimension where
nobody else has an entry.

### Taglines (lead with the first)

1. **"An inbox is not a permission."**
2. "They give your agent an inbox. We give you control of the one you already have."
3. "Your domain. Your reputation. Your rules. Your audit trail."
4. "The agent didn't get an email address. It got a delegation."

---

## 2. Messaging pillars

| Pillar | Claim | Proof |
|---|---|---|
| **Authority, not access** | Agents get clamped, revocable delegations — not a shared credential | `envelope agent policy set`, send-mode ceiling ladder, `agent_policy_denied_action` |
| **Attribution** | Every action, every send decision, traced to a named agent | `envelope actions tail --agent`, Governor verdict badges, HMAC-signed event stream |
| **Your identity** | Send from the address counterparties already trust | Any IMAP provider, no DNS setup, no new domain |
| **$0 per message** | Flat annual license; marginal cost of a message is zero | Licensing table + cost calculator |
| **Custody** | Bodies, attachments, and threads never leave your infrastructure | Self-hosted, offline-capable, evidence bundles |
| **Interop, not lock-in** | We govern *any* mailbox — including theirs | v1.1 AgentMail provider profile |

## 3. ICP segments and the message each one gets

| Segment | Pain | Lead message | Entry product |
|---|---|---|---|
| **Solo builders / indie agent devs** | Want an agent on their own Gmail without wiring OAuth | "Add your email and password. Your agent has a mailbox in 60 seconds." | Free personal tier |
| **Agent-platform teams already on AgentMail** | Provisioning is solved; governance isn't. Nobody can answer "which agent sent that?" | "Keep your inboxes. Add the control plane." | v1.1 interop path → Team |
| **SMB ops teams (10–25 mailboxes)** | Want triage/scheduling/follow-up automation on their real support and sales mail | "Automate the inbox you already run, with a human approving every send." | Team / Growth |
| **Regulated + legal** | Cannot put message bodies on a third party's infrastructure | "Custody stays with you. Evidence bundles are read-only and verifiable." | Growth / Enterprise |
| **Self-hosters / homelab** | Ideological and practical: own the stack | "Runs offline. Any IMAP. FSL source-available." | Free → Team |

## 4. The cost argument (state the assumption honestly)

Assumption, said out loud in every asset: **you already pay for email.** If you
don't have a mailbox, a hosted inbox API is genuinely the faster start — say so.
For everyone else, the math:

| Agent email volume | Hosted inbox API (published rates) | Envelope |
|---|---:|---:|
| 3k msgs/mo | $0 (free tier) | $0 personal / $240 yr commercial |
| 10k msgs/mo | $240/yr | $240/yr Team, flat |
| 50k msgs/mo | $2,400/yr (Startup tier required) | $240–960/yr, flat |
| 150k msgs/mo | $2,400/yr | $240–960/yr, flat |
| 500k msgs/mo | Enterprise, negotiated | $960/yr, flat |

The line to hold: **their cost scales with how much your agent talks; ours
doesn't.** An agent in a reply loop is a billing incident on a metered plan and
a no-op on ours.

Ship this as an interactive calculator on the comparison page. Pull competitor
numbers from published pricing only, date-stamp them, and fix them within a week
of any change — a stale competitor price is the fastest way to lose the
credibility this whole campaign runs on.

## 5. Asset list

**Tier 1 — must ship before Week 1**

1. **Comparison page** — `envelope vs. hosted agent inboxes`. Includes an honest
   "when to use them instead" section: no mailbox yet, need hundreds of
   throwaway inboxes, want managed deliverability, don't want to run anything.
   The concession is what makes the rest believable.
2. **90-second demo video** — the fleet-on-shared-inbox story from
   `docs/launch-assets.md`: two agents, one mailbox, one clamped to draft-only,
   a denial with a stable code, then `actions tail` showing who did what.
3. **Cost calculator** — volume slider, flat line vs. metered line.
4. **"An inbox is not a permission" essay** — the category-defining piece.
   Argues that provisioning is solved and authority isn't, with the Launch HN
   prompt-injection thread as evidence that the category knows it.

**Tier 2 — Weeks 2–4**

5. **`docs/providers/agentmail.md`** + companion post: *"Govern your AgentMail
   inboxes with Envelope"* — five commands, real output, zero snark. Genuinely
   useful to their users; that's the point.
6. **Prompt-injection field guide** — how a malicious email escalates through an
   agent's mail tool, and what a send-mode ceiling actually stops. Positions us
   as the safety-serious vendor before Phase 4 ships the tooling.
7. **Migration guide** — from metered API to BYO mailbox, including the
   deliverability checklist (`envelope deliverability` output as the artifact).
8. **`envelope-skill.md` distribution** — Claude Code plugin marketplace, MCP
   registries, awesome-mcp lists, Cursor/Zed docs. Distribution beats messaging.

**Tier 3 — Weeks 5–6**

9. **Case study** — one real fleet-on-shared-inbox deployment with numbers.
10. **Conformance benchmark teaser** — announce the public agent-mail governance
    test corpus (Phase 4.5). Owning the benchmark is owning the vocabulary.

## 6. Six-week calendar

| Week | Ship | Publish | Channel focus |
|---|---|---|---|
| **0** | v1.1 (Phase 0 interop) | Comparison page, calculator, demo video | Assets staged, nothing announced |
| **1** | — | **"An inbox is not a permission"** | HN, Lobsters, r/selfhosted, X, LinkedIn. The essay leads, not the product |
| **2** | — | Comparison page + calculator push | SEO ("email api for ai agents", "agent inbox alternative"), roundup-site outreach with corrected/added Envelope rows |
| **3** | — | **"Govern your AgentMail inboxes"** | Their audience, in their subreddits and Discords, gracious tone. Also: MCP registry + plugin marketplace listings |
| **4** | v1.2 (per-agent identity, OAuth, HTTP API) | **Show HN #2: inbox-per-agent on the domain you already own** | HN front page attempt, YC-adjacent networks, X |
| **5** | — | Prompt-injection field guide + migration guide | Security newsletters, agent-dev communities |
| **6** | — | Case study + benchmark teaser | Podcasts, newsletters, sales follow-up on Week-4 inbound |

**Why the essay leads:** launching product-first invites a feature comparison we
partially lose today (Phase 0/1 aren't fully landed until Week 4). Launching
frame-first means the Week-4 product launch arrives into a conversation we
defined.

## 7. Objection handling

| Objection | Response |
|---|---|
| "AgentMail sets up in one API call, you need an app password." | True today, and it's the top item on our roadmap — OAuth device flow in v1.2. The 90 seconds buys you an address your customers already recognize and $0 per message forever. |
| "Doesn't self-hosting mean I run infrastructure?" | One binary, SQLite, systemd unit or container. No DNS, no MX, no IP warmup — that's the infrastructure we're saving you from. |
| "What about deliverability?" | You inherit your existing domain's reputation instead of sharing a pool with every other tenant. `envelope deliverability` audits SPF/DKIM/DMARC; v1.4 adds continuous DMARC report monitoring. |
| "We need hundreds of disposable inboxes." | Then use a hosted provider for supply — and put Envelope on top for governance. That's a supported configuration as of v1.1, not a concession. |
| "Is this just Himalaya with extra steps?" | Himalaya is a CLI mail client. Envelope is a multi-agent runtime with identity, policy, audit, and a fail-closed send gate. See the README comparison. |
| "Source-available isn't open source." | Correct, and we don't claim otherwise. FSL-1.1-ALv2: free for personal use, converts to Apache 2.0, flat annual commercial license — no per-seat, no per-message. |
| "Why should I trust an agent with my real mailbox?" | You shouldn't trust it — you should clamp it. Draft-only ceiling, recipient allowlists, human approval queue, revocable per-agent tokens, and an audit trail of every action. That's the entire product thesis. |

## 8. Rules of engagement

Non-negotiable. A competitive campaign that gets caught embellishing loses more
than it wins, and our whole pitch is trustworthiness.

1. **No astroturfing.** No sock puppets, no fake reviews, no seeded comments.
2. **No disparagement.** AgentMail built something good and we say so. We
   compete on a different axis; that reads as confidence, and it's also true.
3. **Cite only published material,** date-stamped, corrected within a week of any
   change.
4. **Never claim capabilities we don't ship.** If Phase 1 slips, the per-agent
   identity messaging slips with it. Roadmap items are labeled as roadmap.
5. **Never claim managed deliverability.** We audit DNS; we do not warm IPs.
6. **Concede the honest wins.** Every comparison asset carries a "when to use
   them instead" section.
7. **Their users are guests.** The Week-3 interop post is genuinely useful to
   AgentMail customers or it doesn't ship.

## 9. Metrics

| Metric | Baseline | 90-day target |
|---|---|---|
| Comparison-page sessions | 0 | 5,000 |
| Calculator → license-inquiry conversion | — | 3% |
| GitHub stars | current | +2,000 |
| `envelope quickstart` completions (opt-in telemetry or self-reported) | current | 3× |
| Commercial licenses issued | current | 3× |
| Inbound mentioning "governance" / "audit" / "attribution" | ~0 | 25% of inbound |
| Third-party roundups listing Envelope | 0 | 5 |
| Share of voice on "agent email governance" | 0 | #1 |

The pillar metric is the second-to-last one. If a quarter from now buyers are
asking about attribution and clamps instead of inbox provisioning, the campaign
worked regardless of what the other numbers did.

## 10. Immediate next actions

1. Land Phase 0 (AgentMail provider profile + docs + tests) — unblocks Week 3.
2. Draft "An inbox is not a permission" — the long pole; start now.
3. Build the comparison page and calculator from the teardown's scorecard tables.
4. Re-cut the `docs/launch-assets.md` demo to 90 seconds, denial-code moment
   front and center.
5. Prioritize OAuth device flow (Phase 2.6) — the objection that kills deals.
6. Set a weekly competitor-pricing check so the calculator never goes stale.
