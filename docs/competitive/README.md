# Competitive analysis and GTM

Working documents for Envelope's position against the hosted agent-inbox
category (AgentMail and peers). Research current as of 2026-07-26.

| Document | What it is |
|---|---|
| [`agentmail-teardown.md`](agentmail-teardown.md) | What AgentMail ships, what it costs, where it's strong, where it's structurally exposed, and an honest two-way scorecard against Envelope |
| [`roadmap-agent-mail-parity.md`](roadmap-agent-mail-parity.md) | Five phases closing the gaps — interop wedge, per-agent identity, HTTP/MCP/SDK reach, retrieval parity, trust moat — with non-goals and invariants |
| [`feature-matrix-and-forecast.md`](feature-matrix-and-forecast.md) | Full three-way feature matrix (Envelope / AgentMail / Cloudflare Email Service), normalized cost at volume, product profiles, scored predictions of what each ships next, market-structure forecast, opportunities, and pre-decided tripwires |
| [`gtm-competitive-campaign.md`](gtm-competitive-campaign.md) | Positioning, messaging pillars, ICP segments, cost argument, asset list, six-week calendar, objection handling, snark guidance, metrics |

**The one-line thesis:** hosted inbox APIs answered "how does an agent get an
inbox." Nobody answered "what happens the first time it gets it wrong?" We ship
the brakes — draft-only defaults, approval queue, revocable per-agent tokens,
audit trail — for mailboxes you already own, including inboxes rented from
someone else. Externally we call that **oops protection**, not "governance."

Claims about Envelope in these documents are verified against the repo; claims
about competitors come from published material only and are date-stamped. See
the rules of engagement in the campaign doc before using any of this externally.
