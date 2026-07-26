# Envelope roadmap — closing the agent-mail gap

_Companion to `agentmail-teardown.md`. Target window: 2026-H2, v1.1 → v1.4.
Every "today" claim is verified against this repo at `be98752`; every proposed
surface is new work unless marked **exists**._

---

## Strategy in one paragraph

We are not going to become a hosted inbox provider. We are going to make
Envelope the **governed mailbox runtime** — the layer that gives an agent
scoped, attributed, revocable authority over a real mailbox — and we are going
to accept mail from *any* source, including AgentMail's own inboxes over IMAP.
Five gaps stand between us and that claim: no per-agent addresses, no remote
API surface, no SDKs, thin retrieval, and app-password-only auth. Everything
below closes one of those, in leverage order.

### Non-goals (write these down so we stop relitigating them)

- ❌ Hosting mailboxes, running MX, or operating IP pools.
- ❌ Provisioning DNS on the customer's behalf (we audit, we don't mutate).
- ❌ Bulk cold outreach features. That's the abuse vector that will define the
  category's reputation; we want to be the vendor that visibly declined it.
- ❌ Shipping our own LLM inference. The agent is the intelligence; Envelope is
  the execution and the guardrail. Retrieval and extraction plumbing, yes;
  a bundled model making send decisions, no.

---

## Phase 0 — The interop wedge (2 weeks, v1.1)

**Highest leverage work in this document, and nearly all of it already exists.**
AgentMail inboxes speak IMAP (`imap.agentmail.to`, IDLE) and SMTP
(`smtp.agentmail.to`:465/:587), authenticating with the inbox address as
username and the API key as password. `envelope accounts add` already accepts
`--imap-host`/`--smtp-host`/ports (`crates/cli/src/main.rs:534`). So Envelope
can govern an AgentMail inbox *today*, undocumented and untested.

| # | Deliverable | Detail |
|---|---|---|
| 0.1 | Provider profile | Add `agentmail.to` to `crates/email/src/discovery.rs` so `accounts add --email bot@agentmail.to` auto-discovers hosts/ports/TLS with no flags |
| 0.2 | `--provider agentmail` | Explicit opt-in flag documenting that the password field takes an API key, not a password |
| 0.3 | Tests | Discovery unit test + a fake-IMAP integration test. No live network, no real sends — per the send-safety invariants |
| 0.4 | `docs/providers/agentmail.md` | "Govern your AgentMail inboxes with Envelope" — add the inbox, set an agent policy clamp, draft-only ceiling, `actions tail` |
| 0.5 | Provider table row | README provider-support table |

**Why it matters:** it converts a competitor's installed base into our
addressable market, it costs a sprint, and it is the single most credible proof
that we're a control plane rather than a rival inbox vendor. It also gives the
campaign its most disarming asset (see GTM Week 3).

**Exit criteria:** `envelope accounts add --email <inbox>@agentmail.to
--password-stdin` works with zero host flags; `envelope watch` receives IDLE
push from it; docs page live.

---

## Phase 1 — Per-agent identity without becoming a mail host (4–6 weeks, v1.2)

**The gap:** they give each agent its own address; our agents share one mailbox
identity. **The insight:** you don't need a hosted inbox to give an agent its
own address — you need sub-addressing or a catch-all on a domain you already
own. `you+skippy-a3f9@yourdomain.com` and `skippy@bots.yourdomain.com` are both
real, deliverable, reputation-inheriting addresses that cost $0 and require no
new vendor.

| # | Deliverable | Detail |
|---|---|---|
| 1.0 | **Cloudflare Email Routing recipe** | Documentation, not code: `*@agents.yourdomain.com` catch-all routed free into your existing mailbox, Envelope splitting inbound by `Delivered-To`. Unlimited per-agent addresses at $0 with no new vendor. Ships ahead of 1.1 because it needs nothing built. Caveat per provider: outbound send-as requires aligned DKIM |
| 1.1 | `envelope identity mint --agent <name>` | Mints a plus-address (default) or catch-all-subdomain address; persists to a new `agent_identities`-linked `agent_addresses` table; prints once |
| 1.2 | `envelope identity list / show / rotate / revoke` | Rotate issues a fresh token-suffixed address and retires the old; revoke stops outbound use and installs a rule that files inbound to a quarantine folder |
| 1.3 | Outbound binding | Sends from an agent bind `From`/`Reply-To` to that agent's address. The address must be inside the agent's policy allowlist — the clamp still governs, minting never widens authority |
| 1.4 | Inbound routing | Route on `Delivered-To`/`X-Original-To`/plus-tag into per-agent virtual folders; auto-create the matching rule at mint time |
| 1.5 | Capability probe | Detect whether the provider supports sub-addressing and/or catch-all (`envelope doctor` extension) and report a stable status rather than silently minting a dead address |
| 1.6 | Dashboard | Per-agent address column in the Agent Cockpit; inbound-by-agent filter |
| 1.7 | Contract | New surface entries in `commands::contract`; MCP tools derived from it; schema bump if any existing shape changes |

**Positioning payoff:** "Inbox per agent — on the domain you already own, with
the reputation you already built, at $0 per message."

**Exit criteria:** two agents on one mailbox, each with a distinct address,
each seeing only its own inbound, each attributed in `actions tail`, and a
revoked agent's address demonstrably dead in both directions.

---

## Phase 2 — Reach: HTTP API, remote MCP, SDKs, container (6 weeks, v1.2 → v1.3)

**The gap:** we're reachable only from a shell on the same machine. Cloud agents
can't use us. `crates/dashboard/src/lib.rs` already exposes ~45 REST routes with
bearer-token auth, Tailscale identity allowlisting, CSRF, and SSE — that's a
product API wearing a UI's clothes.

| # | Deliverable | Detail |
|---|---|---|
| 2.1 | `envelope.http.v1` | Promote the dashboard REST surface to a documented, versioned public API generated from `commands::contract`. OpenAPI spec into `docs/schemas/` |
| 2.2 | Agent tokens over HTTP | `envtok_` identities currently authorize MCP calls; extend the same pre-dispatch authorization to HTTP so per-agent policy clamps apply identically on both surfaces. **Non-negotiable: HTTP must not be a policy bypass.** |
| 2.3 | `envelope mcp --http --bind <addr>` | Streamable-HTTP MCP transport alongside stdio, so hosted agents (Claude, Hermes, Codex) connect without a local process. Refuses non-loopback bind without auth, same rule as `serve` |
| 2.4 | TS + Python SDKs | Generated from the contract schema, published to npm/PyPI, CI-regenerated so drift is impossible. Table stakes for developer adoption |
| 2.5 | Container + deploy recipes | Multi-arch image, `docker compose` with a passphrase file via secrets, Fly/Railway one-click, systemd already **exists** |
| 2.6 | OAuth / XOAUTH2 | Gmail + Microsoft device-code flow. App-password-only is a hard blocker wherever an org disables them, and Microsoft keeps tightening. This is the highest-severity onboarding gap we have |
| 2.7 | Inbound HTTP ingestion adapter | Authenticated endpoint accepting an RFC822 message from a Cloudflare Worker or any webhook source, landing it in the local index with the same rules/tagging/audit path as IMAP-sourced mail. Makes "govern any supply" literally true for non-IMAP providers. Added after the Cloudflare analysis — see `feature-matrix-and-forecast.md` §7.2 |

**Exit criteria:** a cloud agent with only an `envtok_` and a URL can run the
full read/draft/approve loop, and hits the identical denial codes a local MCP
agent would.

---

## Phase 3 — Retrieval and extraction parity (6 weeks, v1.3)

**The gap:** they ship semantic search, auto-labeling, and attachment text
extraction. We ship IMAP `SEARCH` — server-side, exact-match, no local index
over bodies. We already persist `indexed_message_summaries`; we're one FTS table
from a real local index.

| # | Deliverable | Detail |
|---|---|---|
| 3.1 | Local FTS5 index | Bodies + headers + attachment text; `envelope search --local`, hybrid local+IMAP by default with a documented precedence order |
| 3.2 | `envelope attachment text <uid>` | PDF/DOCX/TXT/HTML → text, so agents stop base64-shuffling. Local extraction only, sandboxed parsers, size caps |
| 3.3 | Opt-in semantic search | Local embedding model, opt-in, index stored locally. Headline: semantic search where **no message body leaves the machine** — the version they structurally cannot offer |
| 3.4 | `envelope extract --schema <json-schema> <uid>` | Deterministic plumbing: normalized text + schema + validation of what the agent returns. We validate; the agent infers. On-brand and cheaper than competing on model quality |
| 3.5 | `envelope rule suggest` | Mine `message_tags`/`message_scores` history and propose rules with blast-radius preview. Our answer to auto-labeling, except the user sees and approves the rule |

**Exit criteria:** `search --local` beats IMAP `SEARCH` on recall for a 50k-message
mailbox, index build is resumable, and nothing in the path opens a network socket
that wasn't already open.

---

## Phase 4 — The trust moat (ongoing, v1.4+)

Where we go somewhere they can't follow. This is the durable differentiation.

> **Reprioritized 2026-07-26.** Items 4.1 and 4.5 move ahead of 4.3/4.4.
> Cloudflare owns Area 1 email security and has ~45% odds of pointing it at
> agent traffic within the year (`feature-matrix-and-forecast.md` §5). If they
> ship injection defense before we do, we lose the safety narrative and become
> "the local version." Conversely, 3.3 (semantic search) drops in priority —
> both competitors have it and neither wins deals with it.

| # | Deliverable | Detail |
|---|---|---|
| 4.1 | Prompt-injection defense | `envelope read --untrusted` returns bodies in a provenance-tagged envelope (`content_provenance: external`), with imperative-instruction heuristics surfaced as a structured `injection_signals` field — never silently stripped. The #1 criticism of this entire category, and nobody has shipped a real answer |
| 4.2 | DMARC feedback loop | Ingest aggregate DMARC reports **from your own mailbox** and fold them into `envelope deliverability` (which **exists** and audits SPF/DKIM/DMARC today). Turns a one-shot audit into continuous sender-reputation monitoring — a thing only a client with IMAP access to your domain's reports can do |
| 4.3 | Retention + legal hold | Evidence bundles **exist**; add scheduled collection, retention policy, and hold flags. Sells to legal/compliance, where custody is the whole purchase |
| 4.4 | Org roles | Multi-user role assignment on Team/Growth licenses — our analog to their multi-tenant Pods, scoped to an org's own domain |
| 4.5 | Injection + policy conformance suite | A public test corpus and a `envelope conformance` command that scores any agent-mail stack, ours included. Own the benchmark, own the category vocabulary |

---

## Sequencing and dependencies

```
Phase 0  ██                          weeks 1-2    ships alone, unblocks GTM week 3
Phase 1    ████████                  weeks 2-7    depends on nothing
Phase 2      ██████████              weeks 4-10   2.6 (OAuth) can start immediately
Phase 3            ████████          weeks 8-14   3.1 unblocks 3.3, 3.5
Phase 4                ██████████    weeks 12+    4.2 depends on existing deliverability cmd
```

Critical path to "we have no embarrassing gaps": **Phase 0 + 1 + 2.6**. That's
roughly eight weeks and it removes provisioning friction, per-agent identity,
and the app-password blocker — the three objections that end a sales
conversation. Everything after that is expansion.

## Release mapping

| Version | Contents | Campaign moment |
|---|---|---|
| v1.1 | Phase 0 | "Govern your AgentMail inboxes" post |
| v1.2 | Phase 1 + 2.1–2.3 + 2.6 | Show HN #2: inbox-per-agent on your own domain |
| v1.3 | Phase 2.4–2.5 + Phase 3 | SDK launch + "semantic search that never leaves your machine" |
| v1.4 | Phase 4 | Injection-defense whitepaper + conformance suite |

## Invariants this roadmap must not break

Restating the ones with teeth, because several phases brush against them
(see `CLAUDE.md` for the full set):

- **Send safety.** New surfaces (HTTP, remote MCP, minted identities) default to
  `draft-only` in agent contexts. Denials use stable JSON codes. A clamp never
  widens a policy. Tests never send real mail or mutate live mailboxes.
- **Contract.** Anything that changes an existing `--json` shape needs a new
  schema id; `docs/schemas/envelope.agent_contract.v1.json` and
  `docs/agent-contract.md` get updated in the same change.
- **Evidence.** Read-only against mailboxes: `EXAMINE` + `BODY.PEEK[]`, always.
- **Quickstart.** `--skip-network` still opens no sockets and mutates no bytes.
- **Secrets.** API keys, tokens, passwords, and full recipient addresses stay
  out of status JSON, logs, manifests, docs, and audit events — including the
  AgentMail API key introduced in Phase 0.

## Success metrics

| Metric | Baseline | 90-day target |
|---|---|---|
| Time from install to first governed agent action | ~5 min (app-password detour) | < 90 s (OAuth path) |
| Providers with zero-flag auto-discovery | 7 | 9 (+ AgentMail, + OAuth Gmail/MS) |
| Agents reachable without a local shell | 0 | HTTP + remote MCP GA |
| Search recall on a 50k mailbox | IMAP SEARCH only | local FTS + opt-in semantic |
| Commercial licenses issued | current | 3× |
