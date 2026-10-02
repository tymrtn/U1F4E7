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
  forwards that variable to `envelope mcp`.
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
