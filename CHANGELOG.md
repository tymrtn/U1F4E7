# Changelog

All notable changes to Envelope Email are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.12.2] — 2026-07-04

### Fixed

- Draft and Sent-copy `From` headers now use the same account-name fallback and RFC5322-safe quoting as SMTP sends, so draft/reply/MCP proof copies no longer regress to bare addresses when `display_name` is unset.
- Sent proof append for `draft send` now runs even when the local draft has no IMAP Drafts UID, so local-only drafts on generic IMAP/SMTP providers can still produce durable Sent-folder proof after SMTP acceptance.
- Sent append failures now surface stable skipped reasons instead of silently reporting only SMTP success, and draft JSON includes a `sync_status_reason` when a draft is local-only.
- Folder detection candidates now include lowercase slash layouts such as `INBOX/draft` and `INBOX/sent` used by inbox.eu/martin.fm-style accounts.

## [0.12.1] — 2026-07-04

### Fixed

- Outbound SMTP From headers now fall back to the account name when `display_name` is unset or blank, so accounts such as `Tyler Martin <tyler@martin.fm>` no longer send as a bare email address. Envelope now builds the default From mailbox through lettre's mailbox builder so quoted display names are serialized safely.

## [0.12.0] — 2026-07-02

### Added

- **Dashboard authentication for tailnet/remote exposure.** The dashboard REST API now enforces a credential on every `/api` route whenever an auth method is configured, closing the hole where a `tailscale serve` front-end (or any non-loopback bind) exposed read/delete/send-mail and account management to every reachable device with no authentication.
  - **Bearer token** — `Authorization: Bearer <token>` or `X-Envelope-Token: <token>`, compared in constant time. The agent path for Hermes/OpenClaw/scripted clients. Configure with `envelope config set dashboard.auth_token <token>` (stored `0600`, never echoed) or `ENVELOPE_DASHBOARD_TOKEN`.
  - **Tailscale identity allowlist** — a request whose `Tailscale-User-Login` (injected by `tailscale serve`) is allowlisted is authorized without a token, so a human just opens the `.ts.net` URL. Configure with `envelope config set dashboard.tailscale_allow "you@tailnet.ts.net,agent@tailnet.ts.net"` or `ENVELOPE_DASHBOARD_TAILSCALE_ALLOW`.
- **`envelope serve --bind <addr>`** — bind an explicit address. **Fail-closed:** binding a non-loopback address with no auth configured is refused before the socket opens.
- **New config keys** `dashboard.auth_token` (secret; presence-only in `get`) and `dashboard.tailscale_allow`, resolved env-first then persisted config.

### Changed

- `GET /api/health` returns a minimal `status`/`service`/`version` payload to unauthenticated callers; absolute filesystem paths (`binary_path`, `database_path`, `app_data_dir`) are disclosed only to authorized callers. Open loopback mode is unchanged, so local `envelope doctor` drift detection still sees full paths.
- Loopback (`127.0.0.1`) with no auth configured stays open — local dev, the desktop app, and stdio MCP are unaffected.
- Corrected the dashboard's stale "localhost-only by default" doc claim; the CORS allowlist is documented as a browser-only defense, not the access control.

## [0.11.7] — 2026-07-02

### Fixed

- Dashboard draft review deep links now open the exact linked local draft in Agent Cockpit, including the reviewable detail card, instead of only selecting the account and leaving operators with nothing to review.

## [0.11.6] — 2026-07-01

### Added

- **Governor blind-attribution send gate** — real SMTP transmission now derives sanitized Envelope-specific attribute keys and calls `governor score --catalog envelope`; Envelope treats Governor's opaque `allow` / `review` / `deny` route as authoritative instead of synthesizing shell-command metadata or shadow-scoring policy.
- **Outbox-first actual sends** — allowed sends queue into the scheduled-send/outbox path by default with a safety cooldown; immediate SMTP transmission requires an explicit confirmed bypass.
- **Attachment parity for agent sends and drafts** — send, reply, forward, draft edit/send, scheduled send, and MCP paths snapshot attachment bytes while exposing only safe summaries in JSON and logs.
- Scheduled sends can snapshot and deliver attachments safely, with attachment summaries exposed in `scheduled list` and no base64 payload leakage in outputs.
- `envelope accounts copy-password` provides local secure clipboard credential handoff with non-secret audit metadata.
- `envelope doctor` now emits structured diagnostics, dry-run repair plans, and safe DB/credential backup repair actions.
- Dashboard `/api/health` exposes version/binary/backend/path diagnostics for stale-service drift checks.
- `envelope evidence attachment export` exports attachment bytes with source provenance, hashes, safe paths, optional text extraction, and contract/docs coverage.

### Changed

- Dashboard draft approval now queues through the shared outbox/Governor path instead of using a direct SMTP helper, preserving cooldown, attachments, sent-proof bookkeeping, and contextual reply headers.
- Scheduled-send sweeps re-derive final send attributes from the persisted draft immediately before SMTP and park durable `review` / `deny` verdicts for review rather than retry-storming every sweep.
- The agent contract documents queued-send fields, immediate-send confirmation fields, attachment inputs, and Governor/outbox semantics.

### Fixed

- Contextual replies keep `In-Reply-To` and `References` through queued delivery, avoiding orphaned `Re:` messages.
- Successful sends continue to return stable proof handles, including Message-ID and best-effort Sent mailbox lookup status.
- Dashboard email reader collapses quoted replies without enabling scripts and re-measures iframe height on native toggle.
- Native setup instructions gained password-copy handoff and a read-only dashboard backend endpoint.

## [0.11.0] — 2026-06-17

### Added

- **Account signature CLI** — `envelope accounts signature show|set|clear` manages text/HTML account signatures without direct database edits.
- **Native client setup helper** — `envelope accounts setup-instructions` prints non-secret IMAP/SMTP host, port, security, and username values for Mail.app-style setup.
- **Role-based search** — `envelope search --role/--roles sent,archive ...` resolves provider/custom folder layouts such as `INBOX/sent` and searches every matching folder.
- **Canonical agent skill** — `docs/agents/envelope-skill.md` documents Envelope as the runtime for Hermes, Claude Code, Codex, and similar harnesses.

### Changed

- Bare search terms are normalized to IMAP `TEXT` searches so agent queries like `Hillan` no longer silently miss present messages.
- `read` output now preserves full `to_addrs` and `cc_addrs` recipient lists while keeping scalar compatibility fields.
- IMAP-draft sends from the dashboard delete the original IMAP draft only after SMTP send succeeds.
- Backup export help now documents the `--batch-size` memory tradeoff.

### Fixed

- `read --json` strict JSON behavior is regression-tested against control characters.
- Backup verification now flags symlinked/unsafe extra archive entries instead of traversing them.
- Restore-state sidecar reads/writes reject symlinked paths and unsafe parents.
- Backup JSON mode emits machine-readable fatal error events before returning failure.
- Dashboard deep-link fallback coverage now includes message and cockpit routes.

## [0.10.0] — 2026-05-30

The dashboard's interaction model is now fully baked: the half-wired
controls from the Phase 1 shell either do what they imply or are hidden
when they don't apply, and a Gmail-class keyboard/selection layer lands on
top. Every interaction in this release was verified in-browser against a
real cached-message dataset before shipping.

### Added

- **Gmail-style keyboard shortcuts** — `j`/`k` move the focused row,
  `Enter`/`o` open it, `x` selects, `s` stars, `e` archives, `#` deletes,
  `r`/`a` reply / reply-all, `c` composes, `/` focuses search, `Esc` closes
  the topmost surface, and `?` toggles a discoverable cheat-sheet modal
  (also reachable from the header). Shortcuts are suppressed while typing in
  an input or while a modal is open. A single shortcut table drives both the
  handler and the cheat sheet.
- **Shift-click range selection** — selecting a row and shift-clicking
  another toggles the whole contiguous range, matching Gmail.
- **Focused message row** — the active row for keyboard navigation has a
  visible affordance and scrolls into view.

### Changed

- **Contextual affordances are now a first-class rule.** Controls that have
  no useful action in the current state are hidden rather than rendered
  broken. The account "Reconnect" control only appears when an account is
  actually unhealthy; the bulk toolbar collapses to a hint when nothing is
  selected. Architectural status notes no longer leak into button labels or
  accessible names.
- **Star toggling persists to IMAP `\Flagged`** through the existing flags
  endpoint, instead of being local-only.
- **Account reconnect runs the real IMAP/SMTP `verify` flow** and refreshes
  the health badge, instead of returning a "not wired yet" string.
- **Bulk archive and delete are wired** to the existing per-message
  endpoints with bounded concurrency, per-item failure reporting, and
  terminal status summaries that auto-clear instead of lingering.
- **HTML email reader auto-sizes to its content** (collapse-then-measure,
  clamped to 760px) so short messages no longer sit inside a tall empty
  box. The email iframe grants `allow-same-origin` solely for height
  measurement; scripts/forms remain disabled and message HTML is still
  sanitized, so no email-controlled code can run.
- **Cache-missing accounts read as an unwarmed cache**, not a hard failure:
  no false "all accounts failed" wall, no spurious Reconnect buttons, and a
  clean single refresh CTA.

### Fixed

- Reply / Reply-All no longer open an empty composer when no message is
  selected; they surface a friendly prompt instead.

### Preserved

- Agent Cockpit aggregate endpoints remain read-only — no live auth probes,
  IMAP mutations, or draft sends from aggregate load. Per-account draft
  actions stay the mutating surface.

## [0.9.0] — 2026-05-20

### Added

- **Dashboard Phase 1 Core Shell** — rebuilt the localhost dashboard as an
  operator mail client with a left mailbox sidebar, middle message list, and
  permanent right reader pane. The sidebar exposes Unified Inbox,
  Today/Needs Attention, Snoozed, Sent, Drafts, All Mail, and account mailbox
  groups with nested folders when folder metadata is available.
- **Agent Cockpit attention strip** — the cockpit now starts as a compact
  read-only attention strip and expands into the existing watches, event
  buckets, drafts/actions, auth/action errors, rule runs, and due snoozes
  panels.
- **Message primitive rows and bulk triage shell** — message list rows now carry
  a shared `message` primitive shape with state, actions, audit event,
  render hint, rollback token, and concise equivalent CLI metadata. The message
  list adds dense mail-client controls for selection, local star affordance,
  sender, subject, snippet, labels, attachment hints, date, and an honest
  bulk toolbar that stays non-mutating until backend execution is wired.
- **Account health primitive and sidebar badges** — account rows now expose a
  local-only `account_health` primitive with compact health badges, sync
  freshness, provider capability hints, sanitized failure reasons, and an honest
  reconnect affordance that stays non-mutating until recovery is wired.
- **Path diagnostics** — `envelope paths` (alias: `envelope doctor`)
  prints the resolved database path, file credential path, config/app-data
  directory, current `HOME`, and warnings when those locations are under
  agent-harness directories such as `/private/tmp`, `/tmp`, `/var/folders`,
  or `.codex`.

### Changed

- Workspace and active desktop shell versions bumped to 0.9.0.
- Dashboard first paint now lands on the cached Unified Inbox/local operator
  surfaces and avoids automatic account selection that would probe live IMAP
  folders or messages.

[0.11.0]: https://github.com/tymrtn/U1F4E7/releases/tag/v0.11.0
[0.10.0]: https://github.com/tymrtn/U1F4E7/releases/tag/v0.10.0
[0.9.0]: https://github.com/tymrtn/U1F4E7/releases/tag/v0.9.0

## [0.5.0] — 2026-04-19

### Added

- **IMAP IDLE event stream** — `envelope watch` opens a persistent
  IMAP IDLE connection and emits JSON events on new mail in real time.
  Supports stdout, webhook (`--webhook <url>`), and SQLite event storage.
  Reconnects automatically on connection drop with exponential backoff.
  25-minute IDLE cycle stays under RFC 2177's 29-minute server timeout.

- **Verification code extraction** — `envelope code --wait 60` blocks
  until a verification/OTP code arrives, extracts it from the message
  body (regex patterns for explicit labels, OTP-style codes, HTML-prominent
  numbers, and standalone digits), and prints it to stdout. Pipe-friendly:
  `CODE=$(envelope code --wait 60)`. Filters by `--from` domain and
  `--subject` pattern.

- **MCP server** — `envelope mcp` starts a Model Context Protocol server
  over stdio, exposing 12 tools: inbox, read, search, send, reply,
  move_message, flag, folders, tag, contacts, accounts, and rule_run.
  `envelope mcp --config` prints a ready-to-paste JSON config snippet
  for Claude Code, Cursor, or Zed. Envelope is the only MCP email server
  that works against any IMAP provider (Gmail, Outlook, Migadu, Fastmail,
  self-hosted Dovecot).

- **Scheduled send** — `envelope send --to ... --at "monday 9am"` creates
  a draft with a scheduled send time. The `envelope serve` background
  ticker sends due messages automatically. `envelope scheduled list` and
  `envelope scheduled cancel <id>` manage the queue. Reuses the snooze
  datetime parser (ISO 8601, relative offsets, natural language).

- **Contacts** — `envelope contacts add/list/show/tag/untag/import`.
  Local contact store in SQLite with freeform tags (JSON array). Tags
  integrate with the rules engine: `--match-contact-tag vendor` creates
  a rule that matches any message from a contact tagged "vendor".
  `envelope contacts import --from-inbox` bootstraps the contacts table
  from inbox senders.

- **Webhook rule actions** — `envelope rule create --action webhook=<url>`
  POSTs message context as JSON to the webhook URL when a rule matches.
  10-second timeout, fire-and-forget. Enables integrating Envelope's
  rules engine with external systems (n8n, Make, custom scripts).

- **SQLite schema migrations** — Replaced hand-rolled `CREATE TABLE IF
  NOT EXISTS` with `rusqlite_migration` (v1.3). Tracks schema version
  via `PRAGMA user_version`. Existing databases upgrade seamlessly.
  All v0.5.0 tables (events, contacts) are added as versioned migrations.

- **Agent Workflows help section** — `envelope --help` now shows a
  dedicated "Agent Workflows" section with copy-paste one-liners for
  watch, code extraction, scheduled send, contacts import, and MCP setup.

### Changed

- Workspace version bumped to 0.5.0.
- `regex` added as a dependency (for verification code extraction).
- `rusqlite_migration` added as a dependency (schema versioning).
- `ContactHasTag` added to the rules engine `MatchExpr` enum. Rules
  can now match on the sender's contact tags, not just message-level tags.

[0.5.0]: https://github.com/tymrtn/U1F4E7/releases/tag/v0.5.0

## [0.4.1] — 2026-04-19

### Fixed

- Exposed `snooze check-replies` subcommand — was implemented but not
  wired into the clap dispatch.
- Dashboard compose handler updated for `from_override` parameter added
  to `SmtpSender::send`.

### Changed

- Repo renamed from `tymrtn/envelope-email` to `tymrtn/U1F4E7`. Old URL
  redirects. Brew tap moved to `tymrtn/homebrew-u1f4e7`
  (`brew install tymrtn/u1f4e7/u1f4e7`). Python prototype archived at
  `tymrtn/U1F4E7-python`.

[0.4.1]: https://github.com/tymrtn/U1F4E7/releases/tag/v0.4.1

## [0.4.0] — 2026-04-14

### Added

- **Rules engine** — agents create mail rules that Envelope enforces
  deterministically. `envelope rule create --name "..." --match-from "..."
  --action move=Junk`. Rules evaluate match expressions (FROM/TO/SUBJECT
  globs, tag checks, score thresholds) against messages and execute actions
  (move, flag, unflag, snooze, delete, unsubscribe, add tag). All-match
  default with optional `stop` flag per rule. Batch execution in groups of
  50 with progress reporting.
- **Message tagging + scoring** — `envelope tag set <uid> --score urgent=0.9
  --tag newsletter`. Scores are float dimensions (0.0–1.0), tags are
  freeform strings. Keyed on Message-ID (stable across folder moves).
  Rules can match on tags and scores for agent-trained junk filtering.
- **List-Unsubscribe** — `envelope unsubscribe <uid> --confirm`. Parses
  RFC 2369 `List-Unsubscribe` and RFC 8058 one-click POST headers.
  Dry-run by default (shows what it would do), `--confirm` to execute.
  Never auto-follows GET URLs (tracking risk). Supports HTTPS POST
  and mailto fallback.
- **Sieve export** — `envelope rule export`. Generates RFC 5228 Sieve
  scripts from rules that use pure IMAP-level matches (FROM/TO/SUBJECT).
  Tag/score-based rules are local-only and skipped with a warning.
  ManageSieve upload deferred to v0.5.
- **Background unsnooze ticker** — `envelope serve` now spawns a tokio
  task that sweeps the snooze queue every 60 seconds and returns due
  messages to their original folders automatically.
- **IMAP connection retry** — dashboard folder handler retries with a
  fresh connection on stale IMAP pooled connections.
- **Loading indicators** — dashboard shows "Loading folders…" / "Loading
  messages…" / "Loading message…" while IMAP fetches are in flight.
- **Account list collapse** — sidebar shows 3 accounts by default with
  a "+ N more" toggle for large account lists.
- **Account label in inbox title** — shows "INBOX — tyler@example.com"
  so you always know which account's inbox you're looking at.
- **Rich `--help`** — getting-started examples, agent usage patterns,
  and provider list in the top-level help output.

### Changed

- Workspace version bumped to 0.4.0.
- `reqwest` added as a dependency (for HTTPS unsubscribe).

### Fixed

- **RFC 2047 subject decoding** — IMAP ENVELOPE subjects now decode
  `=?utf-8?q?...?=` and `=?utf-8?b?...?=` encoded words instead of
  showing raw encoded strings. Handles Q-encoding, B-encoding, UTF-8,
  and multiple consecutive encoded words with whitespace folding.
- Sequential folder/message loading — folders load before messages
  (was racing, causing "no account selected" in sidebar).
- Folder error recovery with retry button on IMAP failures.

[0.4.0]: https://github.com/tymrtn/U1F4E7/releases/tag/v0.4.0

## [0.3.0] — 2026-04-09

### Added

- **Full dashboard rewrite.** `envelope serve` now launches a complete
  three-pane email client (folder sidebar, inbox list, reader + composer
  drawers) at [http://localhost:3141](http://localhost:3141). Ported the
  Instrument Sans / DM Mono light-theme aesthetic from the Python U1F4E7
  prototype. HTML/CSS/JS bundled into the binary via `rust-embed` — a
  single `cargo install` ships the whole UI.
- **REST API backing the dashboard** (`/api/*`) with routes for accounts,
  folders (with unread counts), messages (list/read/flag/move/delete/search),
  attachments, compose, reply, drafts, snoozed, threads, and stats.
- **Reply / reply-all** with correct header threading. New
  `envelope_email_transport::reply` module builds `In-Reply-To` and
  `References` headers from the parent message, handles 11 international
  `Re:`/`Fwd:` prefixes, and excludes the account owner from reply-all
  Cc. Works from both the CLI (via `envelope send`) and the dashboard
  reader.
- **SMTP attachments.** `envelope send --to x --attach file.pdf --attach other.png`
  wraps the message body in `multipart/mixed` with one part per file.
  Content-Type detection via `mime_guess`. The dashboard composer
  base64-encodes files client-side and posts them in the JSON envelope.
- **Snooze feature** (`envelope snooze set|list|cancel`, `envelope unsnooze`).
  Flexible datetime parsing: ISO 8601, relative (`2h`, `3d`, `1w`),
  natural (`tomorrow`, `monday`, `next week`). Escalation tiers,
  waiting-reply tracking, per-account IMAP `Snoozed` folder.
- **Threading** (`envelope thread show|list|build`). RFC 2822
  header walking (`Message-ID`, `In-Reply-To`, `References`) with a
  normalized-subject fallback for messages missing threading headers.
  11-language subject prefix stripping (English, German, French, Spanish,
  Dutch, Italian, Portuguese, Swedish/Norwegian).
- **`envelope folders`** now shows per-folder `exists / unseen`
  counts via the IMAP `STATUS` command, both in human output and `--json`.
- **`envelope read <uid>`** uses `BODY.PEEK[]` so reading a message does
  NOT auto-set the `\Seen` flag on the server. Explicit `envelope flag add
  <uid> seen` is required to mark as read.
- **`envelope mark_seen`** helper (library API) for callers that want
  explicit read-marking after `fetch_message`.
- **Orphan detection CI guard** (`ci/check-orphans.sh`). Fails when any
  `.rs` file in `crates/*/src/` is not declared via `mod` — prevents
  the class of silent regression that lost the snooze and threading
  features in commit `27f3919` (see `docs/ORPHANS-AUDIT.md`).
- **`docs/ORPHANS-AUDIT.md`** — post-mortem of the 27f3919 regression
  and the measures taken to prevent recurrence.

### Changed

- **Binary renamed from `envelope-email` to `envelope`.** The Cargo
  package name remains `envelope-email` to preserve the crates.io slot,
  but the binary target is now `envelope`. `cargo install envelope-email`
  installs a binary called `envelope`. Users who installed 0.1.x via
  `cargo install` or Homebrew need to either re-run the install or
  update their PATH.
- `envelope folders` text output gained the `exists / unseen` columns.
- `envelope serve` default port remains 3141. The old 4-endpoint stub
  is gone; the full dashboard is the only option now.
- `crates/dashboard` no longer embeds HTML as a Rust string — it lives
  in `static/` and is bundled at compile time via `rust-embed`.

### Fixed

- **Restored the orphaned snooze feature** (`crates/store/src/snoozed.rs`
  and `crates/cli/src/commands/snooze.rs`, ~1,276 lines). These files
  shipped in commit `27f3919` but were never declared via `mod` and
  never compiled — the feature silently didn't exist. Recreated the
  missing `SnoozedMessage` model, `snoozed` table DDL, and wired the
  impl + CLI into the module tree. Added a `snoozed.reply_received`
  column and `escalation_tier` column that the orphan code expected.
- **Restored the orphaned threading feature** (`crates/email/src/threading.rs`,
  `crates/store/src/threads.rs`, `crates/cli/src/commands/thread.rs`,
  ~2,211 lines). Same silent regression from commit `27f3919`. Recreated
  `Thread` + `ThreadMessage` models, `threads` + `thread_messages` +
  `thread_sync_state` table DDL, and wired everything into `mod`. Fixed
  drift where `thread_messages.id` was defined as `TEXT` but the code
  expected `INTEGER PRIMARY KEY AUTOINCREMENT`. Fixed `Option<String>`
  handling in display code paths.
- `threading::normalize_subject` now delegates to a new
  `strip_reply_prefixes` helper that preserves case; `normalize_subject`
  lowercases the result for thread grouping only. Previously the
  lowercasing leaked into any caller using it for display.
- `envelope read` correctly uses `BODY.PEEK[]` (guarded by a unit test).

### Removed

- **All governor / policy / scoring integration** (`crates/cli/src/governor.rs`,
  `crates/cli/src/commands/governor.rs`, the `Governor` clap subcommand,
  and — most importantly — the `--no-governor` CLI flag that was a
  self-documenting backdoor on a public tool). If you want governance
  around destructive or outbound operations, wrap Envelope from outside:
  `governor envelope send ... -- --attr user_requested`. Envelope's
  job is email.

### Notes

- This is the first release with a proper CHANGELOG. Prior releases
  (0.1.0, 0.2.x) shipped without one; their commit history is the only
  record.
- Total work landed in 0.3.0: ~5,000 lines added, 8 commits, 113 tests
  passing (40 store + 63 email + 10 dashboard), zero clippy warnings in
  new code, `ci/check-orphans.sh` clean.

[0.3.0]: https://github.com/tymrtn/U1F4E7/releases/tag/v0.3.0
