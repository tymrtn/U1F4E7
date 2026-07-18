# Launch asset shot list — Envelope v2 → 1.0.0

_Drafted 2026-07-11. Every command below was verified against `crates/cli/src/main.rs`,
`crates/cli/src/commands/*.rs`, `crates/cli/src/mcp.rs`, `docs/agent-fleet-shared-inbox.md`,
`docs/quickstart.md`, and `docs/install-linux.md` on `feat/v2`. Nothing here is invented —
if a command couldn't be confirmed in source it's flagged in the "Unverified" note at the
bottom instead of guessed at._

The headline demo is **agent fleet on a shared inbox**: two independently-tokened
agents, one mailbox, per-agent policy clamps, attributed drafts, human-in-the-loop
approval, and a full audit trail. Everything else supports that story.

---

## Recording setup

- **Terminal**: 100x28 cols, a monospace font at 16-18pt (JetBrains Mono / SF Mono),
  dark theme, no shell prompt clutter (`PS1='$ '` for the recording session only).
- **Clean demo account**: a throwaway mailbox (not `ty@tmrtn.com`) so send/receive
  traffic in the recording isn't real correspondence. A free Fastmail/Migadu test
  box or a dedicated Gmail app-password account works.
- **Isolated data dir**: point Envelope at a scratch config root so nothing touches
  the real `~/Library/Application Support/envelope-email/` state:
  ```bash
  export ENVELOPE_HOME=/tmp/envelope-demo
  mkdir -p "$ENVELOPE_HOME"
  ```
  Confirm `envelope paths` reports the scratch root before recording anything.
- **Reset between takes**: `rm -rf "$ENVELOPE_HOME"` and re-run `accounts add` to
  get a clean agent/token/draft state for retakes.
- Never run these commands against a mailbox with real mail unless the scene
  explicitly needs live IMAP (scene 7's VPS bootstrap does, using the throwaway
  account's app password).

---

## Scene 1 — Two agent identities, one inbox

```bash
envelope agent create skippy
envelope agent create triage-bot
envelope agent list
```

**What the viewer sees**: each `create` prints a token exactly once (`env-agent-<64-hex>`)
with the on-screen warning that it's shown once and can't be recovered. `agent list`
then shows both agents by name/token-prefix/status — never the raw token.

**Why this sells it**: proves agents aren't a single shared credential — each one is
a real, independently-revocable identity, visible in one list.

---

## Scene 2 — Per-agent policy clamp (restricted agent blocked)

```bash
envelope agent policy set triage-bot \
  --allow-accounts you@example.com \
  --allow-actions inbox.read,message.flag,message.move \
  --send-mode-ceiling draft-only

envelope agent policy show triage-bot

# Simulate triage-bot's MCP session and have it attempt to send —
# an action its policy does not include:
ENVELOPE_AGENT_TOKEN=<triage-bot-token> \
  echo '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"send","arguments":{"account":"you@example.com","to":"someone@example.com","subject":"test","body":"test"}}}' \
  | envelope mcp
```

**What the viewer sees**: `policy show` prints triage-bot's allow-lists (no `send`
action). The `tools/call` response comes back as an error payload carrying the
stable code `agent_policy_denied_action` and a reason string — no crash, no silent
drop, no partial send.

**Why this sells it**: the clamp is structural, not a prompt instruction the agent
could talk itself out of. A misbehaving or compromised agent physically cannot
reach outbound mail its policy excludes.

---

## Scene 3 — Draft → approve, attributed per agent

```bash
# Skippy is scoped to draft-only + inbox/draft actions:
envelope agent policy set skippy \
  --allow-accounts you@example.com \
  --allow-actions inbox.read,draft.create \
  --send-mode-ceiling draft-only

# Skippy's MCP session creates a reply draft (via the create_reply_draft tool):
ENVELOPE_AGENT_TOKEN=<skippy-token> \
  echo '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"create_reply_draft","arguments":{"uid":<message-uid>,"body":"Thanks, following up now."}}}' \
  | envelope mcp

# Human opens the cockpit and reviews the pending draft:
envelope serve
# -> http://localhost:3141 shows the draft attributed to "skippy", awaiting approval

# After human review, approve/send it:
envelope draft list
envelope draft send <draft-id>
```

**What the viewer sees**: the dashboard cockpit (`crates/dashboard/src/handlers/cockpit.rs`,
`drafts.rs`) lists the draft tagged with the authoring agent's name and a pending
state; clicking through to send fires the Governor-gated actual send path
(`crates/cli/src/commands/governor_gate.rs`), so the recording shows both "agent
proposed" and "human executed" as visually distinct steps.

**Why this sells it**: this is the whole pitch in one loop — agents propose, humans
dispose, and the system remembers who wrote what.

---

## Scene 4 — Per-agent audit trail

```bash
envelope actions tail --agent skippy
envelope actions tail --agent triage-bot
envelope actions tail --account you@example.com
```

**What the viewer sees**: the first two calls return disjoint, agent-scoped action
lists (Skippy's draft-create, triage-bot's denied send attempt from Scene 2); the
third shows the merged timeline for the whole account with each row visibly
attributed.

**Why this sells it**: every agent action — allowed or denied — lands in one
queryable audit log keyed by agent identity. Nothing is invisible after the fact.

---

## Scene 5 — Third identity blocked by license gate

```bash
envelope agent create researcher-bot
# denied: free tier is capped at 2 active agents (skippy + triage-bot already exist)
# JSON mode shows the stable code cleanly:
envelope agent create researcher-bot --json

envelope license status
envelope license activate env-lic-<your-16plus-char-key>
envelope agent create researcher-bot
```

**What the viewer sees**: the first `create` exits non-zero with
`"code": "agent_limit_license_required"` and a reason naming the 2-agent free-tier
cap. After `license activate` (stable failure code for a malformed key is
`license_key_invalid_format` — worth showing once with a bad key for contrast),
the same `create` command succeeds.

**Why this sells it**: the monetization boundary is a single, honest CLI gate —
no nag screens, no feature crippling, just "you're past the free tier, here's the
unlock."

---

## Scene 6 — Watch stream / event push

```bash
envelope watch --json
# in a second terminal pane, send a test email to the demo account from
# elsewhere, then watch the JSON event appear the moment IMAP IDLE fires
```

**What the viewer sees**: a long-lived process sitting quietly, then a single JSON
event appearing instantly on new mail — no polling delay. Optionally repeat with
`--webhook <url>` or `--deliver` to show the durable delivery path firing an HTTP
POST.

**Why this sells it**: push, not poll — the fleet reacts to mail in real time
instead of the demo looking like a cron job.

---

## Scene 7 — VPS bootstrap: curl-install to first inbox peek

On a fresh Linux box (a disposable cloud VM, not a machine with real state):

```bash
curl -fsSL https://raw.githubusercontent.com/tymrtn/U1F4E7/main/dist/install.sh | bash
which envelope

envelope accounts add --email you@example.com --password <app-password>
envelope quickstart
envelope inbox --limit 20
```

**What the viewer sees**: a truly empty machine going from `curl | bash` to a
working, authenticated inbox peek in one uninterrupted terminal recording —
`quickstart`'s 4 phases (paths → account → IMAP auth → inbox peek) print pass/fail
as they go, ending in real message subjects.

**Why this sells it**: "self-hosted, no Docker, no config file spelunking" is a
claim this scene proves rather than states — total time on screen should be well
under the 10-minute quickstart budget.

---

## Scene 8 — The webmail dashboard (cockpit) in a browser

```bash
envelope serve
# -> http://localhost:3141
```

**What the viewer sees**: triage view (inbox list with per-message actions),
a bulk-action pass (`crates/dashboard/src/handlers/mod.rs` bulk endpoints — select
several messages, flag/move/archive together), and a compose flow
(`crates/dashboard/src/handlers/compose.rs`) sending a fresh message end to end,
alongside the agent cockpit view from Scene 3 showing pending agent drafts.

**Why this sells it**: it's a full webmail client, not just a CLI utility — the
dashboard is where a non-technical human actually lives day to day.

---

## Asset checklist

| Asset | Scenes | Format | Feeds |
|---|---|---|---|
| Agent fleet hero GIF | 1 + 2 + 3 (trimmed, ~20-30s) | GIF, terminal | u1f4e7.com hero, README top |
| Full agent fleet walkthrough | 1–5 | Screen recording (MP4 or long GIF) | Show HN post, r/selfhosted post |
| Policy denial close-up | 2 | Still PNG (terminal, code visible) | README "safety model" section |
| Cockpit draft-approval screenshot | 3 + 8 | PNG, browser | u1f4e7.com feature section, README |
| Audit trail screenshot | 4 | PNG, terminal | README, r/selfhosted (self-hosters care about audit) |
| License gate screenshot | 5 | PNG, terminal (JSON mode) | Pricing/commercial page, Show HN comments (transparency) |
| Watch/push GIF | 6 | GIF, terminal split-pane | README, Show HN |
| VPS bootstrap GIF | 7 | GIF, terminal (fresh VM) | r/selfhosted, install docs, Show HN top comment bait |
| Dashboard tour screenshots (triage/bulk/compose) | 8 | PNG set (3-4 shots) | u1f4e7.com feature grid, README |

---

## Unverified / needs a live check before recording

- **`dist/install.sh` one-liner path**: the script exists and its `--version` /
  `--bin-dir` / `--allow-root` flags are read from its own header, but it has not
  been run end-to-end against a real GitHub release in this session — confirm a
  tagged release with matching tarball assets exists before recording Scene 7, or
  the curl-install will 404.
- **Exact cockpit UI copy** ("draft by agent X awaiting approval") in Scene 3 is
  inferred from the dashboard handler names (`cockpit.rs`, `drafts.rs`, `agents.rs`)
  and the agent-fleet doc's framing — the literal on-screen label wasn't grepped
  from frontend template strings. Check the rendered page before treating that
  phrase as verbatim UI text.
