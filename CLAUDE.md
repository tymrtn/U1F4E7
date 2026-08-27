# Envelope Development Notes

## Commands
- Format: `cargo fmt --check`
- Tests: `cargo test --workspace`
- Full clippy currently has pre-existing store lint debt; use baseline-aware review before treating `cargo clippy --workspace --all-targets -- -D warnings` as a feature regression.

## V1 release parity
- Every V1 patch that lands in the shared runtime (the installed `envelope` CLI/dashboard Tyler and agents both use) must ship as a matching public release: version bump, changelog entry, tag. No dogfood-only divergence in the shared binary.
- V1 dev builds stay isolated: `-dev`-labelled installs never replace the shared runtime and never substitute for cutting the public release.
- V2 is a separate track. V2 work must not delay a V1 patch release, and V1 patch releases must not wait on V2 reconciliation.

## Keychain import invariants
- `accounts import-keychain` must be explicit: metadata discovery first, no `security -w` password read unless `--confirm-read`, and no Envelope account mutation unless `--import` is also supplied after IMAP/SMTP auth verification.
- JSON statuses are the public contract: `found_candidate`, `no_candidate`, `oauth_or_token_only`, `auth_verified`, `auth_failed`, `imported`. Never include password/token values in status JSON, errors, tests, docs, or logs.
- OAuth/token-backed Mail.app accounts (Gmail/iCloud/etc. without internet-password entries) should report unsupported/app-password guidance rather than pretending raw passwords exist.

## Agent contract invariants
- `envelope contract` exports `envelope.agent_contract.v2` (v1 is retained as historical documentation); update `docs/schemas/envelope.agent_contract.v2.json` and `docs/agent-contract.md` after intentional contract changes.
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

## Writing rules (docs, README, marketing, commit messages, PR bodies)

Applies to prose written for humans. Code comments follow the surrounding code
instead. The failure mode these rules target is writing that reads as
machine-generated: abstract, evenly-cadenced, confident about nothing in
particular.

### Banned constructions

- **Antithesis as a crutch.** "It's not X, it's Y." "X isn't just Y — it's Z."
  Occasionally earns its place; as a default sentence shape it's the single
  loudest tell. One per document, maximum.
- **Aphoristic taglines.** "An inbox is not a permission." If a line sounds
  like it belongs on a conference slide, cut it or replace it with the concrete
  claim underneath.
- **Compulsive triads.** Three parallel items because three feels complete, not
  because there are three things. Use two. Use four. Use one.
- **The restating summary sentence.** A paragraph ends, then a sentence
  explains what the paragraph meant. Delete it; the reader was there.
- **Bolding the punchline of every paragraph.** Bold is for scanning tables and
  genuine warnings. When everything is emphasized, nothing is.
- **Balanced sentence rhythm.** Three medium sentences in a row is a tell. Vary
  length hard — a nine-word sentence next to a forty-word one.
- **Abstract category nouns for concrete things.** "Governed mailbox runtime,"
  "control plane," "the layer above." Say what it does: "it holds the agent's
  drafts until you approve them."

### Banned vocabulary

`delve`, `leverage` (as a verb), `robust`, `seamless`, `elevate`, `landscape`,
`realm`, `testament to`, `tapestry`, `in today's fast-paced`, `it's worth
noting`, `at its core`, `unlock` (metaphorical), `journey`, `empower`,
`game-changing`, `best-in-class`, `crucially`, `notably`.

Words that are fine in moderation and slop in bulk — cap each at roughly once
per document: `genuinely`, `structurally`, `durable`, `load-bearing`, `table
stakes`, `wedge`, `moat`, `surface` (as a noun), `posture`, `primitive`.

### Positive rules

- **Concrete over abstract.** A number, a command, a filename, a dollar figure.
  "Cheaper" is slop; "$0.35 per 1,000" is writing.
- **Every claim checkable.** If a reader can't verify it with a command, a
  citation, or a file path, either add the source or delete the claim.
- **Say the uncertainty.** "~70% likely," "I checked, this doesn't hold," "we
  haven't tested this on Outlook." False confidence is the most damaging kind
  of slop because it's the kind that gets published.
- **Concede first.** Where a competitor or alternative wins, say so plainly and
  early. It's true, and it's what makes the rest credible.
- **Short words.** "Use," not "utilize." "Stops," not "mitigates."
- **Cut the preamble.** Start at the finding, not at the context for the
  finding.

### Before shipping any document

1. Reread the first sentence of every paragraph. If they'd read as a coherent
   list of claims on their own, good. If they read as throat-clearing, rewrite.
2. Search for `—` and `isn't just`. Cut most of them.
3. Find the three most abstract nouns and replace each with a thing.
4. Check every number and quoted price against a source, with a date.
