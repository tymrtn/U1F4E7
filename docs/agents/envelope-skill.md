# Envelope agent skill

Canonical, distribution-ready operating guide for agents (Hermes, OpenClaw,
Claude Code, Codex, and similar) running Envelope. This is the single source of
truth for how an agent should operate Envelope from first principles. Hand it to
a fresh agent with no prior Envelope context.

All examples use anonymized placeholders (`you@example.com`, `imap.example.com`).
Never copy real addresses, domains, passwords, or tokens into this document.

---

## 1. What Envelope is

Envelope is a mailbox runtime for semi-autonomous agents. It turns an existing
IMAP/SMTP mailbox into programmable infrastructure: reading, searching,
threading, drafting, sending after approval, rules, watch/IDLE, OTP extraction,
scheduled send, evidence export, backup/restore, and a localhost control-plane
dashboard.

Envelope is the **canonical email control plane**. For agentic email work it
replaces general-purpose mail clients such as Himalaya. Do not reach for raw
IMAP/SMTP tools, Himalaya, or provider web UIs when an Envelope command exists.
Raw IMAP/SMTP probing is acceptable only as a read-only diagnostic when Envelope
state is stale or suspect, and any finding must be routed back into Envelope as
the durable control plane.

## 2. The public command is `envelope`

Users and agents run the `envelope ...` CLI. That is the entire public surface.

- Internal Rust crate names (`envelope-email`, `envelope-email-store`,
  `envelope-email-transport`, `envelope-email-dashboard`) are **developer/test**
  names only. Never invoke them as if they were commands.
- `cargo run`, `target/`, and crate names are build/test artifacts, not runtime
  state. Do not confuse a build with an install.

## 3. Prefer `--json`

Every command supports `--json`. Agents should pass `--json` and parse the
structured output rather than scraping human text. JSON output shapes are a
stable contract (see §11). Examples below show `--json` where an agent would use
it.

## 4. Install and runtime model

- Installed command: a single `envelope` binary on `PATH`.
- State lives in an app-data directory (OS-specific): the message/rules/events
  database and the credential store. Run `envelope paths` to see exactly which
  database, credential file, and `HOME` are active.
- Credential backends: `file` (default, encrypted) or `keychain` (macOS),
  selected with `--credential-store`.
- HOME/profile drift is the most common operator failure: different shells or
  agent harnesses can point at different `HOME`s, so Envelope appears to have
  missing or different accounts. Always confirm with `envelope paths` first.

```bash
envelope paths --json          # database / credential / HOME / drift warnings
envelope quickstart --account you@example.com --json   # auth + inbox peek probe
envelope doctor --json         # classify auth/state health (decrypt test, drift)
envelope doctor --check-auth --account you@example.com --json   # + read-only IMAP login probe
envelope doctor --repair --dry-run --json   # plan bounded, backup-first repair (no mutation)
```

`envelope doctor` is a structured auth/state diagnosis. Unlike `paths` (pure
path report), it classifies why mailbox operations can fail even when account
metadata reads fine — distinguishing `credential_decrypt_failed` from
`decrypted_but_imap_auth_failed`. `--repair` performs an always-safe backup of
DB/credential files before any mutation; riskier repairs are reported as
not-available rather than performed silently. It never prints secrets and never
sends email.

## 5. Account setup and discovery

Never hard-code accounts. Discover them.

```bash
envelope accounts list --json
envelope accounts add --email you@example.com --password <app-password>
envelope accounts setup-instructions --account you@example.com --client mailapp --json
```

`setup-instructions` prints only non-secret IMAP/SMTP host/port/security/username
for configuring a native mail client. It never prints the password.

### Secure clipboard credential handoff (local keyboard workflows only)

When a local operator is at the keyboard and needs the stored password to paste
into a native client (Mail.app, a browser login, a provider setup screen), copy
it straight to the OS clipboard instead of printing it anywhere:

```bash
# Copy the stored password to the clipboard (never printed to stdout/stderr/logs)
envelope accounts copy-password --account you@example.com --json

# Pick a specific credential when an account stores distinct IMAP/SMTP passwords
envelope accounts copy-password --account you@example.com --kind imap-password

# Auto-clear the clipboard after 45 seconds (best-effort)
envelope accounts copy-password --account you@example.com --ttl 45

# Print setup fields AND copy the password to the clipboard in one step
envelope accounts setup-instructions --account you@example.com --copy-password --json
```

Behavior and safety:

- The secret is written only to the OS clipboard tool's stdin (`pbcopy` on
  macOS; `wl-copy`/`xclip`/`xsel` on Linux). It is never printed, returned, or
  logged.
- Output is metadata only: account, credential kind, clipboard backend, and
  paste guidance. A non-secret `credential.clipboard_handoff` audit event is
  recorded.
- If an account stores distinct IMAP/SMTP passwords, `--kind` is required
  (`password`, `imap-password`, or `smtp-password`).
- This is transient local convenience, not secure storage, and is intentionally
  unavailable for any remote/headless delivery path.

### Provider-specific quirks for native client setup

- **Migadu**: IMAP `imap.migadu.com:993` (SSL/TLS), SMTP `smtp.migadu.com:465`
  (SSL/TLS) or `:587` (STARTTLS). Use the mailbox password, not your Migadu
  admin account password. The username is the full email address.
- **Gmail / Google Workspace**: Requires an app password (2FA must be enabled);
  the regular account password will not authenticate over IMAP/SMTP. IMAP
  `imap.gmail.com:993`, SMTP `smtp.gmail.com:465`/`:587`. OAuth-only accounts
  cannot use raw-password clipboard handoff — generate an app password first.
- **iCloud**: App-specific password required; IMAP `imap.mail.me.com:993`, SMTP
  `smtp.mail.me.com:587`.
- **macOS Mail.app**: After entering settings, Apple's flow ends at a "Select
  apps to use with this account" screen — enable Mail there. If Internet
  Accounts stalls, configure the account manually with the fields from
  `setup-instructions` rather than the provider auto-setup.

## 6. Core mailbox workflows

```bash
envelope folders --account you@example.com --json
envelope inbox --account you@example.com --limit 20 --json
envelope read 42 --account you@example.com --json
envelope search 'FROM "boss@example.com" SINCE 1-Jan-2026' --account you@example.com --json
```

Organize messages:

```bash
envelope flags 42 --add \Seen --account you@example.com
envelope move 42 --to Archive --account you@example.com
envelope copy 42 --to Backup --account you@example.com
envelope delete 42 --account you@example.com
envelope tag add 42 --tag follow-up --account you@example.com
```

Threads, contacts, attachments:

```bash
envelope thread 42 --account you@example.com --json
envelope contacts list --account you@example.com --json
envelope attachments list 42 --account you@example.com --json
```

## 7. Drafts and sending — safety first

**Every outbound message starts as an Envelope draft.** Never create a loose
`.eml` file as a draft substitute. `.eml` is only for archive/evidence/export
artifacts.

```bash
envelope draft create \
  --account you@example.com \
  --to recipient@example.com \
  --subject "Subject" \
  --body-file ./body.txt \
  --json
```

- Before drafting a reply, inspect the thread (`envelope thread` / `envelope
  read`) so context is correct.
- Use explicit `--account`, `--to`, `--subject`, body, and approved `--cc`.
- Reply/forward with `envelope draft reply` / `envelope draft forward`.

Send modes are `draft-only`, `confirm-send`, `allowlisted-send`,
`autonomous-send`. Agent/MCP contexts default to **draft-only**. Never send
without explicit approval unless a separately approved automation policy exists.
After approval:

```bash
envelope draft send <draft-id> --account you@example.com --json
```

Scheduled send:

```bash
envelope send --to recipient@example.com --subject "..." --body "..." \
  --at '2026-06-20T09:00:00+02:00' --account you@example.com --json
```

## 8. Agent workflows

- **Watch (IMAP IDLE push):** `envelope watch --account you@example.com --json`
  emits NDJSON events as new mail arrives, so agents do not poll.
- **OTP / verification codes:** `envelope code --wait 120 --account you@example.com`
  retrieves a code when one arrives. Treat codes as secrets — never log them.
- **Rules:** `envelope rule create/list/preview/run/enable/disable`. Preview
  before run; rules are mailbox policy, not agent notifications.
- **Events / actions logs:** `envelope events list --json`,
  `envelope actions list --json` surface what happened, with secrets redacted.
- **MCP server:** `envelope mcp` exposes the agent contract surface to MCP
  clients; tool schemas derive from the same contract as the CLI.

## 9. Dashboard / control plane

```bash
envelope serve --port 3141     # localhost operator dashboard
```

Treat the dashboard as a local control plane. Aggregate/read views are
read-only: loading them must not change flags, send mail, run rules, or decrypt
credentials. Share it off-box only through an explicitly configured tunnel; do
not bind it to public interfaces casually.

## 10. Backup, restore, migrate — decision guidance

- **Routine durable copy:** `envelope backup export` → `backup verify` →
  `backup restore`. This is the staged, idempotent, resumable path. Prefer it.
- **Live IMAP-to-IMAP migration:** `envelope migrate` is a distinct, heavier
  command family for moving a mailbox between servers. Use it only when a real
  server-to-server move is required, never as a substitute for backup.
- **Always dry-run first** for migrate/restore and confirm the planned
  copy/skip/failure counts before any live operation. Migration and restore must
  be idempotent and copy-only unless a destructive step is separately approved.

## 11. Machine-readable contract

```bash
envelope contract --json        # exports envelope.agent_contract.v1
```

The contract is the stable, versioned description of agent-facing command
shapes and event payloads. Parse it instead of guessing. Removing, renaming, or
type-changing a `--json` field requires a new contract schema id, so existing
shapes are safe to depend on within a major contract version.

## 12. Safety checklist

- Never print, log, or transmit passwords, tokens, OAuth secrets, OTP codes,
  webhook URLs with secrets, or raw credential-store contents.
- Read-only by default. No flag changes, sends, rule runs, or deletes without
  explicit operator authorization.
- No passive watchdog crons that mutate mailboxes.
- Treat `dry-run` as a promise: it must compute real expectations, not pretend.
- Public issues, docs, and test fixtures use anonymized/synthetic data only.
- Confirm `envelope paths` before concluding accounts/state are "missing."

## 13. Troubleshooting

| Symptom | First check |
|---|---|
| "No accounts" but you expect some | `envelope paths` — HOME/store drift |
| Metadata lists fine but mailbox ops fail | credential decrypt vs IMAP auth — run `envelope doctor --check-auth --json` |
| Dashboard errors while CLI succeeds | installed dashboard binary/version drift — compare `GET /api/health` `version`/`binary_path` against `envelope doctor --json`; dashboard folder errors also carry a `diagnostics` block with the running version/backend |
| `read --json` won't parse | report it; JSON must round-trip — file an issue |
| Search returns nothing for a present message | try field-qualified (`FROM`/`TEXT`/`SUBJECT`) and the right folder |

When Envelope feels "missing" or unreliable, do not reach for a custom
wrapper. Either file a concrete Envelope issue or correct the workflow here.
