# Agents at a glance — two agents, one inbox

Envelope lets multiple AI agents operate against the same mailbox under separate
identities, each with its own send ceiling and account scope. Drafts are attributed,
a human approves before anything leaves, and any agent token can be revoked
independently without touching the others.

---

## 1. Create agent identities

```bash
# Create Skippy — the main assistant agent
envelope agent create skippy
# Token is printed ONCE. Store it immediately — it is not shown again.
# Example output:
#   agent: skippy
#   token: env-agent-<64-char-hex>   ← copy this now
```

```bash
# Create a second agent for a specialized task (e.g., a triage bot)
envelope agent create triage-bot
# Store this token too.
```

---

## 2. Set per-agent policies

```bash
# Skippy: read inbox + create drafts only; cannot send autonomously
envelope agent policy set skippy \
  --allow-accounts you@example.com \
  --allow-actions inbox.read,draft.create \
  --send-mode-ceiling draft-only

# Triage-bot: read + flag + move, no send at all
envelope agent policy set triage-bot \
  --allow-accounts you@example.com \
  --allow-actions inbox.read,message.flag,message.move \
  --send-mode-ceiling draft-only

# Verify policies
envelope agent policy show skippy
envelope agent policy show triage-bot
```

Send-mode ceilings (from most to least restrictive):
`draft-only` → `confirm-send` → `allowlisted-send` → `autonomous-send`

An agent operating under `draft-only` can never send directly — `draft create` +
human approval is the only path to outbound mail.

---

## 3. Wire each agent's MCP environment

In each agent's MCP config (e.g., Claude Code `mcp add-json`), set the bearer token
as an environment variable:

```bash
# Get the base MCP config
envelope mcp --config

# Then for Skippy's Claude Code instance:
claude mcp add-json envelope '{
  "command": "/path/to/envelope",
  "args": ["mcp"],
  "env": {
    "HOME": "/home/you",
    "ENVELOPE_AGENT_TOKEN": "env-agent-<skippy-token>"
  }
}'

# For triage-bot's separate MCP instance:
claude mcp add-json envelope '{
  "command": "/path/to/envelope",
  "args": ["mcp"],
  "env": {
    "HOME": "/home/you",
    "ENVELOPE_AGENT_TOKEN": "env-agent-<triage-bot-token>"
  }
}'
```

Each MCP session is siloed. Skippy cannot see triage-bot's token and vice versa.

---

## 4. The draft → approve → send loop

With `draft-only` policies, agents can only create drafts. A human reviews and
approves in the dashboard cockpit before anything leaves.

```bash
# Agent creates a draft (via MCP tool: create_reply_draft / create_forward_draft)
# Human opens the dashboard and reviews:
envelope serve    # http://localhost:3141 → Agent Cockpit

# After human approval, send the draft:
envelope draft send <draft-id>
```

---

## 5. Audit and observe

```bash
# Tail all actions attributed to Skippy
envelope actions tail --agent skippy

# Tail triage-bot's actions
envelope actions tail --agent triage-bot

# All actions across all agents for an account
envelope actions tail --account you@example.com
```

---

## 6. License tiers and free usage

- Free tier: up to 2 agent identities per account.
- Beyond 2: `envelope license activate env-lic-<your-key>` (see commercial licensing
  in the README).

```bash
envelope license status
envelope license activate env-lic-<your-key>
```

---

## Trust boundary and revocation

- Each agent token is a one-way hash in the database. The raw token never leaves
  the moment of creation.
- Revoking an agent takes effect at the next MCP session — any currently open stdio
  session continues until it restarts.
- Revoke immediately:

```bash
envelope agent revoke triage-bot
# triage-bot's token is now invalid; any running MCP session using it will fail
# on the next tool call.
```

- `envelope agent list` shows names, prefixes, and status — never token hashes.
