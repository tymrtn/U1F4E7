# Changelog

All notable changes to Envelope Email are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.3.0]: https://github.com/tymrtn/envelope-email/releases/tag/v0.3.0
