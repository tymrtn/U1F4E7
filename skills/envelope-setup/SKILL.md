---
name: envelope-setup
description: >-
  Set up Envelope for this agent: install the envelope command, connect the
  user's email accounts, and connect the envelope MCP server with an agent
  token the user creates. Use when the user asks to set up or connect
  Envelope, when the envelope MCP server fails to start or is missing, or when
  the envelope command is not found.
---

# Set up Envelope

Envelope runs on the user's machine and reads the mailboxes they already have.
Setup has four steps. The user does the steps that involve passwords or the
agent token; you never see either.

## 1. Check for the envelope command

```bash
command -v envelope && envelope --version
```

If it is missing, show the user both install options and ask which to run.
Run one only after they agree:

```bash
curl -fsSL https://u1f4e7.com/install.sh | bash   # macOS or Linux, installs to ~/.local/bin
brew install tymrtn/u1f4e7/envelope               # Homebrew; builds from source
```

After the script installs to `~/.local/bin`, make sure that directory is on
`PATH`, then confirm with `envelope --version`.

## 2. Connect the user's email accounts

```bash
envelope accounts list --json
```

If no accounts are listed, ask the user to run this in their own terminal, once
per address:

```bash
envelope accounts add --email you@example.com
```

It asks for the password in a hidden prompt, so the user has to type it
themselves. Gmail, iCloud and Fastmail need an app password, which the
provider's security settings create. Do not ask for the password in chat.

## 3. Create the agent token

Ask the user to run this in their own terminal (pick a name for this agent,
such as `claude-code`, `codex` or `cursor`):

```bash
envelope agent create claude-code
```

It prints a token once. The user pastes it into their tool's secret setting:

- **Claude Code:** installing the plugin (`/plugin install envelope@envelope`)
  asks for it. To change it later, open `/plugin`, select envelope, and update
  its settings, or uninstall and reinstall the plugin.
- **Codex:** in an editor, add `export ENVELOPE_AGENT_TOKEN=<token>` to the
  shell profile that starts Codex (for example `~/.zshrc`), then restart Codex.
  Typing it at the prompt would leave the token in shell history. The plugin
  forwards that variable to `envelope mcp`. Every shell then carries the
  token, so the user's own operator commands are refused there; see
  "If a command is refused" below.
- **Cursor:** set `ENVELOPE_AGENT_TOKEN` in the Envelope plugin's settings
  (Plugins → Configure).

Agent names are unique and stay taken after a revoke, and the free tier allows
two active agents. `envelope agent list` shows them. If a name is taken, use a
new one (for example `claude-code-2`); if the user is at the limit, they can
revoke an agent they no longer use or reuse one agent's token in several tools.

Never ask the user to paste the token into the chat. If they do, or if the
token is lost, tell them to revoke it (`envelope agent revoke <name>`) and
create a new agent under a new name.

## 4. Check the connection

Restart the session if the tool needs it, then call the envelope MCP `accounts`
tool. Read the error if it fails:

| Error contains | Fix |
|---|---|
| `envelope: command not found`, or the server never starts | Step 1 |
| `ENVELOPE_AGENT_TOKEN is required for MCP startup` | The token is not configured: step 3 |
| `does not match any active agent identity` | The token is wrong or revoked: create a new agent under a new name in step 3 |
| `accounts` returns an empty list | Step 2 |

When `accounts` lists the user's addresses, setup is done. Continue with the
envelope skill.

## If one-time codes are refused

`envelope code --from <sender>` returns a code only when the user's mail
provider authenticated the sender (DMARC, or DKIM aligned with the From
domain). Always pass `--from` with the exact sender address or domain.

| Error | Meaning and fix |
|---|---|
| `sender_unauthenticated` | The sender failed authentication. Do not use a code from that message; tell the user. |
| `sender_unverifiable` | The provider recorded no authentication Envelope can trust. Microsoft 365 and Migadu mailboxes always get this, because their results cannot be told apart from ones a sender wrote. The user can allow unverified senders for that account in their own terminal, without the agent token: `env -u ENVELOPE_AGENT_TOKEN envelope config set otp.allow_unverified_senders you@example.com`. Ask them; do not run it yourself. |
| `timeout` | No matching code arrived. Check `--from`, `--account` and `--wait`. |

## If a command is refused

With an agent token, some commands run only for the user, and some need an
action granted by name.

| Error | Meaning and fix |
|---|---|
| `operator_only_command` | The command changes accounts, credentials, agents, policy, configuration or delivery routes, so it runs only without an agent token. Ask the user to run it in their own terminal. If their shell exports the token (the Codex setup above), they run it as `env -u ENVELOPE_AGENT_TOKEN envelope ...`. |
| `agent_policy_denied_action` | The agent's policy lacks the action. `rules.write`, `rules.webhook`, `rules.batch_ack`, `sieve.publish`, `watch.webhook` and `unsubscribe` must be named; `"*"` does not include them. The user can grant one: `env -u ENVELOPE_AGENT_TOKEN envelope agent policy set <name> --allow-actions '<current actions>,rules.write'`. Ask them; do not run it yourself. |
| `agent_token_invalid` | The token is unknown or revoked: create a new agent under a new name in step 3. |
