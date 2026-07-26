# GTM — competitive campaign against the hosted agent-inbox category

_Companion to `agentmail-teardown.md` and `roadmap-agent-mail-parity.md`.
Campaign window: 6 weeks, aligned to v1.1 → v1.2._

---

## 1. The frame

AgentMail answered "how does an agent get an inbox?" Fine. That question has a
ceiling, and we shouldn't spend a dollar arguing about it.

The question nobody has answered: **what happens the first time the agent gets
it wrong?** Not in the demo. In week three, at 3am, against your actual
customers, from the address your invoices come from.

We ship the answer already — draft-only defaults, send cooldowns, recipient
allowlists, an approval queue, revocable per-agent tokens, a fail-closed send
gate, and a receipt for every action. What we've been doing is describing it in
language nobody says out loud.

### Call it what it is: oops protection

`send-mode ceiling`, `policy clamp`, `Governor verdict`, `agent identity
attribution` — accurate, unreadable. From now on the external name for the
whole cluster is **oops protection**, and the individual pieces get plain
descriptions:

| Internal term | What we say publicly |
|---|---|
| Send-mode ceiling (`draft-only`) | Your agent can write email. It can't send it. |
| Recipient allowlist | It can only email people you've named. |
| Send cooldown + undo | There's a window to catch it before it goes out. |
| Governor gate | Every send gets checked, and a failed check means no send. |
| `actions tail --agent` | You can see exactly which agent did what, after the fact. |
| Per-agent token + revoke | Kill one agent's access without touching the others. |

Ship a `docs/oops-protection.md` and a README section under that name. It's the
most important copy change in this document.

### Taglines

Lead with the first two. Kill "governed mailbox runtime," "control plane," and
"an inbox is not a permission" — that's conference-panel language.

1. **Oops protection for agents with email access.**
2. **Your agent has email. Now give it brakes.**
3. They raised $6M to run mail servers. You already have one.
3b. Giving an agent an inbox is the easy half. We built the other one.
3c. Bootstrapped. No $6M, no board, no per-message fee.
4. Everyone's handing agents inboxes. Nobody's handing them brakes.
5. Draft-only by default. Your agent is new here.
6. Built on the assumption that your agent will screw up. Because it will.
7. $0 per message. It's your mailbox. You already paid for it.

### The weekend line

The top comment on every launch in this category is "I could build this in a
weekend with Claude." Don't fight it — take it, because for us it's true:

> You could build this in a weekend with Claude. I did. Then I spent six months
> on the part where it doesn't email your entire contact list at 3am.

That one line does three jobs: it disarms the dismissal, it's honest, and it
relocates the argument onto the ground where we win. Use it in the HN comment,
the README intro, and the demo voiceover.

### Positioning statement (internal)

> Envelope runs AI agents on mailboxes you already own — any IMAP provider,
> self-hosted, $0 per message — with the brakes, the approval queue, and the
> audit trail that keep a bad prompt from becoming an apology email.

## 2. Messaging pillars

| Pillar | What we say | Proof |
|---|---|---|
| **Brakes** | Your agent drafts; you approve. Or clamp it tighter. | Draft-only ceiling, allowlists, cooldown, Governor gate |
| **Receipts** | Every action traced to a named agent, after the fact | `actions tail --agent`, signed event stream |
| **Your address** | Send from the address people already recognize | Any IMAP provider, no DNS setup, no new domain |
| **$0 a message** | Flat annual license. Your agent looping isn't a billing event. | Licensing table + calculator |
| **It stays on your box** | Bodies and attachments never leave your infrastructure | Self-hosted, works offline, evidence bundles |
| **Works with theirs** | We govern any mailbox, including AgentMail's | v1.1 provider profile |

## 3. ICP segments

| Segment | Pain | Lead message | Entry |
|---|---|---|---|
| **Solo builders** | Want an agent on their Gmail without wiring OAuth | "Your email and password. Sixty seconds." | Free personal |
| **Teams already on AgentMail** | Provisioning solved, nobody can answer "which agent sent that?" | "Keep your inboxes. Add the brakes." | v1.1 interop → Team |
| **SMB ops (10–25 mailboxes)** | Triage and follow-up on real support/sales mail | "Automate the inbox you already run, with a human approving every send." | Team / Growth |
| **Regulated + legal** | Can't put message bodies on someone else's servers | "Your mail stays yours. Evidence bundles are read-only and verifiable." | Growth / Enterprise |
| **Self-hosters** | Own the stack | "One binary. Any IMAP. Runs offline." | Free → Team |

## 4. The cost argument

Say the assumption out loud in every asset: **you already pay for email.** If
you don't have a mailbox, a hosted inbox API is the faster start — say so and
move on. For everyone else:

| Agent volume | Hosted inbox API (published rates) | Envelope |
|---|---:|---:|
| 3k msgs/mo | $0 (free tier) | $0 personal / $240 yr commercial |
| 10k msgs/mo | $240/yr | $240/yr, flat |
| 50k msgs/mo | $2,400/yr (Startup tier required) | $240–960/yr, flat |
| 150k msgs/mo | $2,400/yr | $240–960/yr, flat |
| 500k msgs/mo | Enterprise, negotiated | $960/yr, flat |

The line: **their price goes up when your agent talks more. Ours doesn't.** A
runaway loop is a billing incident over there and a no-op here.

Ship it as a calculator. Pull competitor numbers from published pricing only,
date-stamp them, fix them within a week of any change. A stale competitor price
is the fastest way to lose the credibility this whole thing runs on.

## 5. Assets

**Before Week 1**

1. **Comparison page** — with an honest "use them instead if…" block: no mailbox
   yet, need hundreds of throwaway addresses, want someone else managing
   deliverability, don't want to run anything. The concession is what makes the
   rest believable.
2. **90-second demo** — two agents, one mailbox, one clamped to draft-only, the
   denial fires with a real error code, then `actions tail` shows who did what.
   The denial is the money shot. Lead with it, don't bury it at 0:70.
3. **Cost calculator** — flat line vs. metered line, one slider.
4. **The essay** — *"Everyone's handing agents inboxes. Nobody's handing them
   brakes."* The weekend line goes in the first three paragraphs.
5. **`docs/oops-protection.md`** — the plain-language safety page.

**Weeks 2–4**

6. **`docs/providers/agentmail.md`** + post: *"Govern your AgentMail inboxes
   with Envelope"* — five commands, real output, zero snark. It has to be
   genuinely useful to their customers; that's the entire point.
7. **Prompt-injection field guide** — how a malicious email walks an agent into
   sending something, and what a draft-only ceiling actually stops.
8. **Migration guide** — metered API → your own mailbox, with the
   `envelope deliverability` output as the artifact.
9. **Skill distribution** — `envelope-skill.md` into the Claude Code plugin
   marketplace, MCP registries, awesome-mcp lists, Cursor/Zed docs.
   Distribution beats messaging.

**Weeks 5–6**

10. **Case study** with real numbers.
11. **Conformance benchmark teaser** — a public test corpus for agent-mail
    safety. Owning the benchmark is owning the vocabulary.

## 6. Six-week calendar

| Week | Ship | Publish | Where |
|---|---|---|---|
| **0** | v1.1 (interop) | Assets staged | Nothing announced |
| **1** | — | **The brakes essay** | HN, Lobsters, r/selfhosted, X, LinkedIn. Essay leads, product follows |
| **2** | — | Comparison page + calculator | SEO ("email api for ai agents", "agentmail alternative"), roundup-site outreach |
| **3** | — | **"Govern your AgentMail inboxes"** | Their communities, gracious tone. Plus MCP registry + marketplace listings |
| **4** | v1.2 (per-agent addresses, OAuth, HTTP API) | **Show HN #2** | HN front page attempt, X, YC-adjacent networks |
| **5** | — | Injection field guide + migration guide | Security newsletters, agent-dev communities |
| **6** | — | Case study + benchmark teaser | Podcasts, newsletters, Week-4 inbound follow-up |

Why the essay leads: a feature-by-feature comparison today partially loses on
provisioning, and Phases 0/1 don't fully land until Week 4. Set the frame first,
launch the product into a conversation we already shaped.

## 7. Objection handling

| Objection | Response |
|---|---|
| "They set up in one API call, you need an app password." | True today. OAuth device flow lands in v1.2. Those ninety seconds buy an address your customers recognize and $0 per message forever. |
| "Isn't self-hosting infrastructure?" | One binary and a SQLite file. No DNS, no MX, no IP warmup — that's the infrastructure we're saving you. |
| "What about deliverability?" | You inherit your own domain's reputation instead of sharing a pool with every other tenant's agent. `envelope deliverability` audits SPF/DKIM/DMARC; continuous DMARC monitoring lands in v1.4. |
| "We need hundreds of disposable inboxes." | Then buy those from a hosted provider and put Envelope on top. Supported configuration as of v1.1, not a concession. |
| "Just Himalaya with extra steps?" | Himalaya is a mail client. This is a multi-agent runtime with identity, policy, audit, and a send gate that fails closed. |
| "Source-available isn't open source." | Correct, and we don't pretend otherwise. FSL-1.1-ALv2, converts to Apache 2.0, flat annual commercial license, no per-seat, no per-message. |
| "Why would I trust an agent with my real mailbox?" | Don't trust it. Clamp it. Draft-only, allowlisted recipients, a human approving sends, revocable tokens, and a log of everything it did. That's the product. |
| "Their inboxes are fine for signups and OTPs." | `curl -s https://api.usercheck.com/domain/agentmail.to` returns `"disposable": true` — same bucket as mailinator. Signup forms call those APIs. Your own domain doesn't have that problem. |

## 8. How to be snarky without being a jerk

Snark is approved. It works here because the product's whole personality is
"we assume this will go wrong." But it has a shape:

1. **The funding contrast is approved and on-strategy.** "They raised $6M to
   wrap SES; we're self-funded and built the hard half" is fair game — it's a
   fact about capital structure paired with a fact about scope, and
   bootstrapped-vs-funded is a positioning a lot of buyers actively prefer.
   Say it. The line to hold is *easy problem, not bad team*: the inbox layer
   genuinely is the easy half, and running abuse ops at scale genuinely is
   hard. Mock the funding-to-difficulty ratio, never their competence.
   - Good: "Giving an agent an inbox is the easy half. We built the other one."
   - Good: "Self-funded, and we didn't need $6M to wrap SES."
   - Good: "Our roadmap answers to users instead of a term sheet."
   - Bad: anything implying they're incompetent, fraudulent, or coasting.
   - Note the expiry: this line stops being ours the day we take money. Use it
     hard while it's true.
2. **Every jab must survive one command.** If a line can't be checked by a
   reader in ten seconds (`curl`, `dig`, a published pricing page), cut it.
   The disposable-domain line qualifies. Guesses about their internals don't.
3. **Self-deprecate first.** The weekend line concedes more about us than any
   line concedes about them. That's what earns the rest.
4. **No astroturfing.** No sock puppets, no seeded comments, no fake reviews.
   Not a tone question — a hard rule.
5. **Don't run the deliverability-audit hit piece.** We could publish
   `envelope deliverability --domain agentmail.to` output. It would go mildly
   viral and it would make us the vendor that audits competitors' DNS for
   content. Use the general argument — shared domain vs. yours — instead.
   _(Also: I checked. Their setup is correct. The apex has no SPF but they send
   from `mail.agentmail.to` with `include:amazonses.com -all` and `p=reject` at
   the apex. Anyone who "finds" this and publishes it will be wrong in public.)_
6. **Never claim what we don't ship.** If Phase 1 slips, the per-agent-address
   messaging slips with it. Roadmap gets labeled roadmap.
7. **Never claim managed deliverability.** We audit DNS. We don't warm IPs.
8. **Their users are guests.** The Week-3 interop post is useful to AgentMail
   customers or it doesn't ship.

## 9. Metrics

| Metric | Baseline | 90-day target |
|---|---|---|
| Comparison-page sessions | 0 | 5,000 |
| Calculator → license inquiry | — | 3% |
| GitHub stars | current | +2,000 |
| `quickstart` completions | current | 3× |
| Commercial licenses | current | 3× |
| Inbound mentioning brakes / approval / audit | ~0 | 25% |
| Third-party roundups listing Envelope | 0 | 5 |

The one that matters is the second-to-last. If buyers are asking "what stops it
from sending the wrong thing" instead of "how many inboxes do I get," the
campaign worked regardless of the other numbers.

## 10. Next actions

1. Land Phase 0 (AgentMail provider profile + docs + tests) — unblocks Week 3.
2. Write `docs/oops-protection.md` and re-cut the README around that language.
3. Draft the brakes essay. Long pole; start now.
4. Build the comparison page and calculator off the teardown scorecard.
5. Re-cut the demo to 90 seconds with the denial in the first 20.
6. Prioritize OAuth device flow — the objection that ends deals.
7. Weekly competitor-pricing check so the calculator never goes stale.
