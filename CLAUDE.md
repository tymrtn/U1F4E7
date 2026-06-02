# Envelope Development Notes

## Commands
- Format: `cargo fmt --check`
- Tests: `cargo test --workspace`
- Full clippy currently has pre-existing store lint debt; use baseline-aware review before treating `cargo clippy --workspace --all-targets -- -D warnings` as a feature regression.

## Keychain import invariants
- `accounts import-keychain` must be explicit: metadata discovery first, no `security -w` password read unless `--confirm-read`, and no Envelope account mutation unless `--import` is also supplied after IMAP/SMTP auth verification.
- JSON statuses are the public contract: `found_candidate`, `no_candidate`, `oauth_or_token_only`, `auth_verified`, `auth_failed`, `imported`. Never include password/token values in status JSON, errors, tests, docs, or logs.
- OAuth/token-backed Mail.app accounts (Gmail/iCloud/etc. without internet-password entries) should report unsupported/app-password guidance rather than pretending raw passwords exist.

## Agent contract invariants
- `envelope contract` exports `envelope.agent_contract.v1`; update `docs/schemas/envelope.agent_contract.v1.json` and `docs/agent-contract.md` after intentional contract changes.
- MCP tool input schemas should be derived from `commands::contract` so CLI/MCP/Hermes/Codex advertise the same contract surface.
- Do not change existing command `--json` output shapes without compatibility notes; removals/renames/type changes require a new contract schema id.

## Send safety invariants
- Agent-facing send modes are `draft-only`, `confirm-send`, `allowlisted-send`, and `autonomous-send`; keep serialized names stable.
- MCP/agent contexts default to `draft-only`. Denials must use stable JSON codes/reasons and send-policy audit events must not include secret material or full recipient addresses.
- Tests must not send real email or mutate live mailboxes; policy tests stay pure and draft-only paths only create local drafts.

## Dashboard / Agent Cockpit invariants
- Agent Cockpit aggregate endpoints must stay read-only: no live auth probes, no IMAP mutations, no draft sends from aggregate load.
- If dashboard backend primitives do not exist yet, surface explicit `not_available`/follow-up states instead of pretending watches, failed-auth history, or draft approval actions are wired.
- Preserve the Rules Control Plane controls and safety bounds when adding dashboard surfaces.

## Evidence export invariants
- Evidence collection must be read-only against mailboxes: use `EXAMINE` and `BODY.PEEK[]`; never mark messages read or mutate mailbox state.
- Raw RFC822 `.eml` files are canonical evidence; preserve full headers and attachments inside the `.eml`.
- Evidence bundles must include verifiable manifest/index/checksum material and reject traversal or symlink tricks during verification.
- Thread expansion is header-based only for MVP (`Message-ID`, `In-Reply-To`, `References`) and must remain bounded; do not add subject-only fallback without explicit warning semantics and tests.
- Do not include secrets in manifests, logs, docs, or examples. Provenance paths/account metadata are intentionally included but should be treated as sensitive.

## Quickstart invariants
- `envelope quickstart --skip-network` must not open sockets, read/decrypt credentials, create config directories, create database files, or mutate existing database bytes.
- Account discovery for quickstart uses read-only existing database access; do not replace it with `Database::open_default()`.
- Network quickstart may read an existing credential-store passphrase but must never create one; use non-mutating credential-store access.
- Inbox peek remains `EXAMINE` + `BODY.PEEK[HEADER.FIELDS (...)]`; never `SELECT` or `BODY[]`.
