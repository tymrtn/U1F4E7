# Cursor Marketplace listing

Paste-ready copy for [cursor.com/marketplace/publish](https://cursor.com/marketplace/publish).
After this repository is public on `main`, a human must submit the repo URL
there. Packaging in this repo does not publish the listing.

Product site (homepage): [https://u1f4e7.com](https://u1f4e7.com).

## Listing

**Name:** Envelope

**Category:** Inbox and Collaboration

**One-liner:** Add an agent to your email. Don't change your email.

**Bullets:**

- Bring your own mailbox: same address, same folders, same habits — Gmail, Fastmail, iCloud, Migadu, or any IMAP you already use. Not a newly provisioned agent inbox.
- Agent reads OTPs, handles replies, runs rules, and drafts for approval. Works with Claude Code, Codex, OpenHands, and Hermes.
- CLI + JSON + local stdio MCP (`envelope mcp`). Human in the loop by default; send modes are contextual policy, not a send block.
- Requires the `envelope` CLI on `PATH` and an `ENVELOPE_AGENT_TOKEN` from `envelope agent create`.

## Install (what reviewers and users need)

1. Install the CLI so `envelope` is on `PATH` (site order):

   ```bash
   curl -fsSL https://u1f4e7.com/install.sh | bash
   ```

   ```bash
   brew install tymrtn/u1f4e7/envelope
   ```

   `tymrtn/u1f4e7/u1f4e7` is a live Homebrew compat alias for the same formula.
   The curl script checks the release's SHA-256 and installs to `~/.local/bin`
   without sudo. Homebrew builds from source (Rust is a build dependency).

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
`skills/envelope/SKILL.md`. Logo: `assets/logo.svg`. Homepage:
[https://u1f4e7.com](https://u1f4e7.com).
