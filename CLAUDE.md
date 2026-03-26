# CLAUDE.md — Envelope Email (Rust)

## What This Is

Envelope Email is a **clean email client** — BYO-mailbox, IMAP/SMTP, with agent-native primitives (JSON on every command, auto-discovery, scriptable). It gives OpenClaw agents email capabilities.

**Envelope has optional Governor integration** for safety. When `ENVELOPE_GOVERNOR=true`,
destructive/outbound commands are routed through the `governor` CLI's scoring engine before
execution. Blind attribution and send-zone routing still belong to Governor externally.
See `crates/cli/src/governor.rs` for the integration layer.

## Repo & License

- **GitHub:** tymrtn/envelope-email-rs
- **License:** FSL-1.1-ALv2 (see LICENSE)
- **Copyright:** 2026 Tyler Martin

## Workspace Structure (4 crates)

```
crates/
├── cli/          # Binary crate — clap-based CLI (`envelope-email`)
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
│       ├── imap.rs           # IMAP operations (async-imap + rustls)
│       ├── smtp.rs           # SMTP send (lettre + rustls)
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
cargo run -p envelope-email-cli -- inbox --json

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
| `governor status` | ✅ Implemented | Show governor integration status |
| `governor test-send` | ✅ Implemented | Dry-run send through governor scoring |
| `governor test-delete` | ✅ Implemented | Dry-run delete through governor scoring |

## Governor Integration

Governor integration is built into the CLI as an optional safety layer.

### How It Works

When `ENVELOPE_GOVERNOR=true` (env var), Envelope shells out to `governor admin score --attr <key> --json`
before executing governed commands. If Governor returns `"deny"`, Envelope aborts with an error.

### Governed Commands

| Command | Attrs | Condition |
|---------|-------|-----------|
| `send` | `outbound`, `email_send` | Always when governor enabled |
| `delete` | `destructive` | Always when governor enabled |
| `move` | `destructive` | Only when destination is Trash/Junk/Spam/Deleted |
| `draft send` | `outbound`, `email_send` | Always when governor enabled |

### Ungoverned Commands (always passthrough)

`inbox`, `read`, `search`, `folders`, `flag`, `accounts list`, `copy`, `draft create`, `draft list`

### Configuration

- `ENVELOPE_GOVERNOR=true` — enable governor checks
- `GOVERNOR_PATH=/path/to/governor` — custom governor binary path (default: `governor` in PATH)
- `--no-governor` — CLI flag to bypass all governor checks (emergency use)

### Files

- `crates/cli/src/governor.rs` — Core governor logic (check, is_enabled, is_destructive_folder)
- `crates/cli/src/commands/governor.rs` — `envelope-email governor` subcommand (status, test-send, test-delete)

### Legacy Integration Points

These existing pieces remain for external Governor integration:

1. **Action Log** (`store/src/action_log.rs`) — Records agent actions with confidence scores.
2. **License Store** (`store/src/license_store.rs`) — License activation gates compose/attributes/actions.
3. **`compose` command** — License-gated stub.
4. **`attributes` command** — Stub for listing scoring attributes.
5. **`actions tail` command** — Stub for showing decisions.

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
