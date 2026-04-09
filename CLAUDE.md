# CLAUDE.md — Envelope Email (Rust)

**Current version: 0.3.0** (see [CHANGELOG.md](CHANGELOG.md))

## What This Is

Envelope Email is a **clean email client** — BYO-mailbox, IMAP/SMTP, with agent-native primitives (JSON on every command, auto-discovery, scriptable). It gives OpenClaw agents email capabilities.

**The binary is called `envelope`** (the Cargo package is `envelope-email` to preserve the crates.io slot; `[[bin]] name = "envelope"` in `crates/cli/Cargo.toml`). Never use `envelope-email` as a command in documentation — it's `envelope ...`.

## 🚫 HARD RULE: No Governor / Policy / Scoring Code

**Envelope has no governor integration and must never gain one.** This is a public,
single-purpose email client. Policy, scoring, blind attribution, "safety bypasses,"
action gating — none of that belongs here.

Specifically forbidden in this repo:
- Any `mod governor`, `governor.rs`, or `commands/governor.rs` file
- Any `--no-governor` / `--bypass-*` / `--skip-*` safety bypass flag (self-documenting backdoor)
- Any `ENVELOPE_GOVERNOR` / `GOVERNOR_PATH` env var reading
- Any shell-out to a `governor` binary from any command
- Any dependency on the envelope-governor crate
- Any README/docs section describing "optional governor integration"

If a caller wants governance, **they wrap Envelope from the outside**:

```
governor envelope send --to alice@example.com -- --attr user_requested --just "reply"
```

Envelope's job is email. Policy lives in the caller. Keep this repo clean.

**Why this rule exists:** Commit `9e67d9b feat: optional Governor integration for
governed send/delete/move` introduced coupling and a `--no-governor` bypass flag into
the public repo. Both shipped undetected until the user spotted them. Future agents
reading old docs/commits must not reintroduce this.

## 🚫 HARD RULE: Every `.rs` file must be declared via `mod`

Every `.rs` file in `crates/*/src/` (except `lib.rs`, `main.rs`, `mod.rs`, `build.rs`,
`bin/*`, `tests/*`) must be reachable from a crate root via `pub mod` or `mod`. Cargo
silently ignores orphan files — they compile clean and never run. This has already
eaten two features.

**Enforcement:** `ci/check-orphans.sh` runs on every CI push. It walks each crate's
`src/` directory and fails with a non-zero exit if any `.rs` file is not referenced
in a `mod` declaration. Run it locally before committing:

```bash
./ci/check-orphans.sh
```

If you're adding a new file `crates/foo/src/bar.rs`, you MUST add `pub mod bar;`
(or `mod bar;`) to `crates/foo/src/lib.rs` (or a reachable `mod.rs`) in the same
commit. See `docs/ORPHANS-AUDIT.md` for the history of what happens when this rule
gets ignored.

## Repo & License

- **GitHub:** [tymrtn/envelope-email](https://github.com/tymrtn/envelope-email) (the repo was renamed from `envelope-email-rs` to `envelope-email` before Apr 9 2026; old URL still redirects but use the new one in docs)
- **License:** FSL-1.1-ALv2 (see LICENSE)
- **Copyright:** 2026 Tyler Martin

## Release History

- **v0.3.0** (2026-04-09) — Binary renamed `envelope-email` → `envelope`. Full dashboard rewrite (three-pane email client at localhost:3141). Snooze + threading features restored from the 27f3919 silent regression. Reply/reply-all with header threading. SMTP attachments. Orphan detection CI guard. All governor integration removed. See CHANGELOG.md.
- **v0.2.x** — Initial public Rust CLI. Had governor integration (removed in 0.3.0) and silent orphans (restored in 0.3.0).

## Workspace Structure (4 crates)

```
crates/
├── cli/          # Binary crate — clap-based CLI (`envelope` binary, `envelope-email` package)
│   └── src/
│       ├── main.rs           # CLI arg parsing + dispatch
│       └── commands/         # One file per command group
│           ├── accounts.rs   # accounts add/list/remove
│           ├── attachments.rs # attachment list/download
│           ├── drafts.rs     # draft create/list/send/discard
│           ├── flags.rs      # flag add/remove
│           ├── folders.rs    # list IMAP folders
│           ├── inbox.rs      # list messages
│           ├── messages.rs   # move/copy/delete
│           ├── read.rs       # read single message
│           ├── search.rs     # IMAP search
│           ├── send.rs       # send email via SMTP
│           ├── serve.rs      # start dashboard server
│           ├── common.rs     # shared helpers
│           └── mod.rs
├── email/        # Library — IMAP client, SMTP sender, DNS auto-discovery
│   └── src/
│       ├── discovery.rs      # MX/SRV → IMAP/SMTP host resolution
│       ├── folders.rs        # Folder detection with provider-aware resolution
│       ├── imap.rs           # IMAP operations (async-imap + rustls)
│       ├── provider.rs       # Provider detection + canonical folder mapping
│       ├── smtp.rs           # SMTP send (lettre + rustls)
│       ├── threading.rs      # Email threading (subject normalization, thread building)
│       ├── errors.rs
│       └── lib.rs
├── store/        # Library — SQLite persistence, crypto, models
│   └── src/
│       ├── accounts.rs       # Account CRUD
│       ├── action_log.rs     # Action audit log (for Governor integration)
│       ├── crypto.rs         # AES-GCM encryption, Argon2 key derivation
│       ├── db.rs             # Database init + connection
│       ├── drafts.rs         # Draft CRUD
│       ├── license_store.rs  # License key storage
│       ├── models.rs         # Shared data models
│       ├── errors.rs
│       └── lib.rs
└── dashboard/    # Library — Axum-based localhost web UI
    └── src/
        └── lib.rs            # REST API + embedded HTML dashboard
```

## Build & Test

```bash
# Build all crates
cargo build

# Build release binary
cargo build --release

# Run tests
cargo test

# Run the CLI directly
cargo run -p envelope-email --bin envelope -- inbox --json

# Check formatting + lints
cargo fmt --check
cargo clippy
```

## CLI Commands (complete list from main.rs)

| Command | Status | Description |
|---------|--------|-------------|
| `accounts add/list/remove` | ✅ Implemented | Manage email accounts |
| `inbox` | ✅ Implemented | List messages in folder |
| `read <uid>` | ✅ Implemented | Read single message |
| `search "<query>"` | ✅ Implemented | IMAP search |
| `send` | ✅ Implemented | Send via SMTP |
| `move <uid>` | ✅ Implemented | Move message to folder |
| `copy <uid>` | ✅ Implemented | Copy message to folder |
| `delete <uid>` | ✅ Implemented | Delete message |
| `flag add/remove` | ✅ Implemented | Manage message flags |
| `folders` | ✅ Implemented | List IMAP folders |
| `attachment list/download` | ✅ Implemented | Manage attachments |
| `draft create/list/send/discard` | ✅ Implemented | Draft management |
| `serve` | ✅ Implemented | Localhost dashboard |
| `compose` | 🔒 Stub (license gate) | Licensed tier placeholder |
| `license activate/status` | ⚠️ Partial | Status works; activate is stub |
| `attributes` | ⚠️ Stub | Not yet implemented |
| `actions tail` | ⚠️ Stub | Not yet implemented |

## No Governor Integration

Envelope does not call, shell out to, or know anything about the Governor. If you
want governance around destructive/outbound commands, wrap the invocation:

```
governor envelope send --to ... -- --attr user_requested --just "reason"
```

Envelope's job is email. Policy lives in the caller.

## Provider Detection & Folder Resolution

Different email providers use different IMAP folder naming conventions. The provider
detection system in `crates/email/src/provider.rs` handles this permanently:

### How It Works

1. **Auto-detection on first connection**: When an account's `provider_type` is NULL, the
   system lists IMAP folders and detects the provider (Gmail → `[Gmail]/` prefix, Dovecot →
   `INBOX.` prefix, Exchange → "Deleted Items"/"Junk E-mail", Standard → flat names).

2. **Stored in DB**: The detected `provider_type` is stored in the `accounts` table and
   reused on subsequent connections (no IMAP round-trip needed).

3. **Canonical folder resolution**: All code uses logical names (`"drafts"`, `"sent"`,
   `"trash"`, `"spam"`, `"archive"`) and calls `resolve_folder(provider, "drafts")` to get
   the actual IMAP name (`"[Gmail]/Drafts"` for Gmail, `"Drafts"` for Standard, etc.).

4. **Backward compatibility**: If `provider_type` is NULL, the system falls back to trying
   all known folder name candidates (the old behavior) and detects the provider for next time.

### Provider Types

| Type | Example Providers | Drafts | Sent | Trash |
|------|-------------------|--------|------|-------|
| `gmail` | Gmail | `[Gmail]/Drafts` | `[Gmail]/Sent Mail` | `[Gmail]/Trash` |
| `standard` | Migadu, Fastmail | `Drafts` | `Sent` | `Trash` |
| `dovecot` | Self-hosted Dovecot | `INBOX.Drafts` | `INBOX.Sent` | `INBOX.Trash` |
| `exchange` | Outlook.com | `Drafts` | `Sent Items` | `Deleted Items` |
| `unknown` | Fallback | Tries all candidates | Tries all candidates | Tries all candidates |

### Key Files

- `crates/email/src/provider.rs` — `ProviderType`, `detect_provider()`, `resolve_folder()`, `classify_folder()`
- `crates/email/src/folders.rs` — `detect_drafts_folder()`, `detect_sent_folder()`, `detect_folder()`, `classify_folders()`
- `crates/store/src/db.rs` — `get_provider_type()`, `set_provider_type()`, `detected_folders` table

### Rules for New Folder References

**Never hardcode folder names.** Always use one of:
- `resolve_folder(provider, "drafts")` — when you know the provider type
- `detect_drafts_folder(client, db, account_id)` — when you need to detect + cache
- `detect_folder(client, db, account_id, "trash")` — generic detection for any type

## Rules for Contributors

1. **Governor integration is opt-in.** The scoring integration in `governor.rs` uses `governor admin score` — it does not implement scoring logic itself. Blind attribution, send zones, and scoring weights belong to the Governor project, not Envelope.
2. **JSON on every command.** Every command must support `--json` for agent consumption.
3. **Auto-discovery by default.** Users provide email + password; IMAP/SMTP hosts are discovered via DNS.
4. **Credentials via pluggable backend.** Passwords encrypted with AES-256-GCM in SQLite. The master passphrase is managed by `--credential-store`:
   - `file` (default): encrypted in `~/.config/envelope-email/credentials.json`, keyed by `ENVELOPE_MASTER_KEY` env var or a machine-specific seed (hostname + username). Works on headless Linux, locked-screen macOS, servers — zero external deps.
   - `keychain`: OS keychain via `keyring` crate (requires `keychain` cargo feature). Use for interactive desktop workflows.
   The file backend is the default because it works everywhere. Keychain is opt-in.
5. **SQLite for state.** All persistent data in `~/.config/envelope-email/` via rusqlite.
6. **Every file starts with the copyright header:**
   ```rust
   // Copyright (c) 2026 Tyler Martin
   // Licensed under FSL-1.1-ALv2 (see LICENSE)
   ```
