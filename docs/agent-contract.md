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
- Draft/reply/forward creation and draft edits support optional `--attach` paths. Attachment bytes are snapshotted into draft storage for review/send continuity, but contract/JSON output exposes only non-secret summaries (`filename`, `content_type`, `size`). Draft edits also support removing named attachments or clearing all attachments. Forwarding original source-message attachments is explicit via `draft forward --include-attachments`; it is not the default.
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

Allowed actual-send paths do **not** transmit immediately by default. They queue into the outbox/scheduled-send mechanism with a cooldown (`send_after`, default 120 seconds; override via `cooldown_seconds` or `ENVELOPE_SEND_COOLDOWN_SECONDS`). Immediate transmission is an explicit emergency bypass only: `send_now`/`--send-now` or `cooldown_seconds=0` plus `confirm_send_now`/`--confirm-send-now`; missing confirmation returns `immediate_send_requires_confirmation` and sends nothing.

Before any real SMTP transmission — both confirmed immediate bypasses and due outbox/scheduled sends — Envelope runs the Governor gate using **blind attribution**: Envelope derives the contextual attribute keys the send exhibits (thread/relationship/domain/recipient/content/stakes signals) and Governor opaquely scores/routes them against its `envelope` catalog, returning `allow`/`review`/`deny`. Envelope never reconstructs or duplicates Governor's weights or thresholds. The scheduled-send sweep re-derives the final attributes from the persisted draft immediately before SMTP; durable `review`/`deny` verdicts park the draft as `pending_review` (no per-sweep retry storm) while a transient gate failure leaves it queued for a later sweep. `ENVELOPE_GOVERNOR_MODE=required|warn|off` defaults to `required`; required mode fails closed on missing Governor, execution error, `review`, or `deny`, and only an explicit Governor `allow` permits SMTP. `ENVELOPE_GOVERNOR_BIN` selects the Governor CLI. Governor itself receives only the declared attribute keys plus a content-free justification (surface + draft id); Envelope's own audit payload additionally holds sanitized metadata (subject hash, recipient counts/domains/classes, surface, draft id, attachment counts/sizes/types, reply flag, and the declared attribute keys) — never bodies, attachment bytes, secrets, or full recipient addresses.

Send surfaces should return proof handles for follow-up automation. Queued sends return `status`, `draft_id`, `send_after`, `cooldown_seconds`, `queued_reason_code`, `queued_reason`, safe attachment summaries, and draft UI; the reason must make clear that Envelope intentionally queued the message in the outbox for a safety cooldown so the agent/operator has time to report and correct issues before SMTP transmission. Immediate/swept sends return `message_id` plus best-effort Sent mailbox proof (`sent_folder`, `sent_uid`, `sent_message_url`, and `sent_mail.lookup_status`). Every SMTP send now generates and returns a stable, non-empty RFC `message_id`, so `lookup_status` is never `no_message_id` after a successful transmission. For providers that do not auto-save SMTP submissions (generic IMAP/SMTP), Envelope appends an exact copy to the Sent folder using the same Message-ID; `sent_mail_appended` reports whether that copy was written and `sent_mail_append_skipped_reason` explains a skip (e.g. `provider_auto_saves_sent`). If the Sent UID is not available yet, the field remains `null` and `lookup_status` explains why.

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
