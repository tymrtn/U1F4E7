# Cursor Marketplace listing

Paste-ready copy for [cursor.com/marketplace/publish](https://cursor.com/marketplace/publish).
After this repository is public on `main`, a human must submit the repo URL
there. Packaging in this repo does not publish the listing.

## Listing

**Name:** Envelope

**Category:** Inbox and Collaboration

**One-liner:** BYO-mailbox email for Cursor agents — your IMAP/SMTP, your domain, any provider.

**Bullets:**

- Bring your own mailbox: Gmail, Fastmail, Migadu, iCloud, Outlook, or any IMAP/SMTP account — not a rented AgentMail-style inbox
- Local stdio MCP (`envelope mcp`) with agent-native tools to read, search, draft, send, and organize mail
- Requires the `envelope` CLI on `PATH` (`brew install tymrtn/u1f4e7/u1f4e7`) and an `ENVELOPE_AGENT_TOKEN` from `envelope agent create`

## Install (what reviewers and users need)

1. Install the CLI so `envelope` is on `PATH`:

   ```bash
   brew install tymrtn/u1f4e7/u1f4e7
   ```

2. Add a mailbox, then create an agent identity (token is printed once):

   ```bash
   envelope accounts add --email you@example.com
   envelope agent create cursor
   ```

3. Install this plugin from the Cursor Marketplace (or, before listing, from
   the Git repository). Set `ENVELOPE_AGENT_TOKEN` under Plugins → Configure.
   Do not commit the token.

4. Confirm the binary Cursor will spawn:

   ```bash
   which envelope
   envelope mcp --config
   ```

The plugin manifest is `.cursor-plugin/plugin.json`. MCP config is `mcp.json`
(`command`: `envelope`, `args`: `["mcp"]`). The skill lives at
`skills/envelope/SKILL.md`. Logo: `assets/logo.svg`.
