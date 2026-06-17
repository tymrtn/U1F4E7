# Envelope Agent Contract

Envelope exposes a versioned agent contract as `envelope.agent_contract.v1`.

Generate the live contract:

```bash
envelope contract
```

Generate one surface:

```bash
envelope contract --surface inbox
```

The checked-in schema snapshot is:

```text
docs/schemas/envelope.agent_contract.v1.json
```

## Compatibility rules

- Existing command `--json` output shapes are not changed by the contract export.
- Optional additions are compatible within `envelope.agent_contract.v1`.
- Removals, renames, required-field changes, or type changes require a new schema id.
- MCP tool input schemas are derived from `crates/cli/src/commands/contract.rs` so CLI, MCP, Hermes, and Codex advertise the same surface.

## Surfaces

The v1 contract covers:

- inbox
- read
- search
- thread
- draft
- send
- contextual draft MCP tools: `create_reply_draft`, `create_forward_draft`, `modify_draft`, `get_draft`, `send_draft`
- watch
- otp
- rules
- evidence

## Read-only list/search limits

Agent-facing CLI/MCP `inbox` and `search` surfaces default to `limit: 25`, accept `limit: 1..=1000`, and reject out-of-range limits before opening an IMAP connection. Dashboard aggregate endpoints keep their own lower defaults/caps and are not governed by this agent limit.

`search` also accepts an optional `roles` array (`inbox`, `drafts`, `sent`, `trash`, `spam`, `archive`, `starred`). When present it replaces the literal `folder`, resolves provider-specific layouts (e.g. `INBOX/sent`, `[Gmail]/Sent Mail`) to every matching folder, includes the source folder on each result, and errors if a requested role resolves to zero folders. Search stays read-only.

## Send safety

Agent-facing send modes are stable strings:

- `draft-only`
- `confirm-send`
- `allowlisted-send`
- `autonomous-send`

MCP defaults agent send/reply flows to `draft-only`. Denials use stable JSON codes and policy audit events avoid secret material and full recipient addresses.

Send surfaces should return proof handles for follow-up automation: `message_id` plus best-effort Sent mailbox proof (`sent_folder`, `sent_uid`, `sent_message_url`, and `sent_mail.lookup_status`). If the Sent UID is not available yet, the field remains `null` and `lookup_status` explains why.

## Evidence

The `evidence` surface is read-only against source mailboxes (IMAP `EXAMINE` + `BODY.PEEK[]`; the source message is never mutated). It covers three commands:

- `evidence collect` / `evidence verify` — raw RFC822 `.eml` bundles with manifest, index, and checksum material.
- `evidence attachment export` — source-provenance attachment export. It preserves raw attachment bytes exactly, SHA-256 hashes them, and writes per-source-message output under `<encoded_folder>-<uidvalidity>-<uid>/`: the original bytes under a sanitized normalized filename, `attachment_provenance.json` (machine-readable provenance per attachment), and `SOURCE_NOTE.md` (human-readable source identifiers). Select with `--uid` (optionally `--attachment <exact name>`, or all attachments if omitted) or `--query '<RAW IMAP SEARCH>'` with an optional case-insensitive `--filename-glob`; `--uid` and `--query` are mutually exclusive. With `--extract-text`, DOCX (`word/document.xml`) and `text/*` attachments get a sibling `<normalized>.txt`; extraction failures preserve the original file and record `extraction_error` without failing the export (PDF is recorded as `pdf_extraction_unsupported`).

## Updating

After intentional contract changes:

```bash
cargo run -q -p envelope-email -- contract > docs/schemas/envelope.agent_contract.v1.json
python3 -m json.tool docs/schemas/envelope.agent_contract.v1.json >/dev/null
cargo test -p envelope-email contract -- --nocapture
```
