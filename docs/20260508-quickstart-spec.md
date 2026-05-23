# `envelope quickstart` — Implementation Spec

Author: backend-eng (kanban t_ab430939)
Date: 2026-05-08
Status: SPEC ONLY — no implementation, install, or publish without separate approval.
Target version: 0.7.x (ships alongside `envelope migrate` GA, gated behind tests + Tyler approval).

---

## 1. Goal

The first useful agent-mailbox workflow, in ~30 seconds, on a workstation that already has at least one Envelope account configured. If no account exists, drop into a guided setup that ends in the same useful-workflow checkpoint. The success state is *not* "we showed the user help text" — it is a real round-trip against a real mailbox using shared Envelope HOME.

`envelope quickstart` is a verification-and-orientation command, not a setup command in the destructive sense. It must never:
- mutate the credential store
- mutate mailbox state (no flags, no moves, no deletes, no `\Seen`)
- print secrets, tokens, OAuth material, webhook URLs, or DB contents
- silently switch backends (keychain ↔ file) or re-route HOME

It must always:
- respect `HOME` and shared Envelope HOME conventions from `CLAUDE.md`
- be idempotent — running it ten times in a row is indistinguishable from running it once
- emit a stable JSON contract under `--json`
- exit non-zero on any failed phase (auth, IMAP fetch, paths drift)

---

## 2. UX surface

### 2.1 Default (text) mode

```
$ envelope quickstart
Envelope quickstart
─────────────────────────
[1/4] paths       ok            /Users/wondermonkey/.hermes/shared/envelope-home
[2/4] account     ok            tyler@spainexpat.com  (id acc_01H…)
[3/4] imap auth   ok            imap.spainexpat.com:993 in 412 ms
[4/4] inbox peek  ok            5 messages, newest 2026-05-08 09:14 from billing@stripe.com

Ready. Try:
  envelope inbox --limit 10
  envelope code --from stripe --wait 60
  envelope serve
```

### 2.2 No-account mode (guided)

```
$ envelope quickstart
[1/4] paths       ok
[2/4] account     none configured

No accounts found in shared Envelope HOME.
Choose one path:

  a) Import an existing macOS Mail.app account
       envelope accounts import-keychain --email you@example.com --confirm-read --import
  b) Add an account by hand
       envelope accounts add --email you@example.com

Re-run `envelope quickstart` after adding an account.
```

Exit code: 2 (no-account; recoverable). No prompts in MVP — quickstart is non-interactive. Interactive `--guided` flag is deferred to a follow-up; spec'd in §9.

### 2.3 JSON mode

`envelope quickstart --json` emits a single JSON object whose shape is the public contract (see §4). Stdout is JSON only, no banners. Stderr stays clean unless a phase emits a structured warning.

### 2.4 Flags

| Flag | Purpose | Default |
|---|---|---|
| `--json` | machine-readable output | off |
| `--account <id-or-email>` | pin a specific account; otherwise use `default_account()` | none |
| `--folder <name>` | folder to peek | `INBOX` |
| `--peek-limit <n>` | messages to fetch headers-only | `5` |
| `--timeout-secs <n>` | per-phase soft timeout | `15` |
| `--skip-network` | run phases 1–2 only, no IMAP connect (CI/local-dev) | off |

No flag changes credential backend or HOME — those remain controlled by the global `--credential-store` flag and environment, exactly like `envelope paths`.

---

## 3. Phase contract

Quickstart is exactly four phases, executed in order, short-circuiting on failure (except `paths` which is reported but not fatal unless HOME is missing).

| # | Phase | Function | Network | Mutation | Fatal on fail |
|---|-------|----------|---------|----------|---------------|
| 1 | `paths` | reuse `commands::paths::collect_report()` | no | no | only if HOME missing |
| 2 | `account` | `Database::default_account()` or resolve `--account` | no | no | yes (exit 2) |
| 3 | `imap_auth` | `transport::imap::connect()` only — no SELECT | yes | no | yes (exit 3) |
| 4 | `inbox_peek` | `transport::imap::fetch_inbox()` with EXAMINE-equivalent semantics | yes | no | yes (exit 4) |

`fetch_inbox` today uses `SELECT`. For quickstart we MUST add an EXAMINE-only path (or guarantee read-only). See §6 invariant 3.

### Per-phase status enum (public contract)

```
ok | skipped | warn | error
```

`warn` is reserved for soft drift (HOME not pointing at shared Envelope HOME, clippy-style nags). It does not change exit code.

---

## 4. JSON contract

Stable. Treat additions as backwards-compatible; removals/renames are breaking and require a version bump.

```json
{
  "schema": "envelope.quickstart.v1",
  "ok": true,
  "elapsed_ms": 1840,
  "phases": [
    {
      "name": "paths",
      "status": "ok",
      "elapsed_ms": 3,
      "details": {
        "credential_backend": "keychain",
        "database_path": "/Users/wondermonkey/.hermes/shared/envelope-home/envelope.db",
        "app_data_dir": "/Users/wondermonkey/.hermes/shared/envelope-home",
        "home": "/Users/wondermonkey",
        "warnings": []
      }
    },
    {
      "name": "account",
      "status": "ok",
      "elapsed_ms": 8,
      "details": {
        "id": "acc_01H…",
        "email": "tyler@spainexpat.com",
        "imap_host": "imap.spainexpat.com",
        "imap_port": 993,
        "smtp_host": "smtp.spainexpat.com",
        "smtp_port": 465,
        "source": "default"
      }
    },
    {
      "name": "imap_auth",
      "status": "ok",
      "elapsed_ms": 412,
      "details": { "host": "imap.spainexpat.com", "port": 993, "tls": true }
    },
    {
      "name": "inbox_peek",
      "status": "ok",
      "elapsed_ms": 1417,
      "details": {
        "folder": "INBOX",
        "message_count": 5,
        "newest_date": "2026-05-08T09:14:03Z",
        "newest_from_domain": "stripe.com"
      }
    }
  ],
  "next_steps": [
    "envelope inbox --limit 10",
    "envelope code --from stripe --wait 60",
    "envelope serve"
  ]
}
```

### Failure shape

```json
{
  "schema": "envelope.quickstart.v1",
  "ok": false,
  "elapsed_ms": 8,
  "failed_phase": "account",
  "phases": [
    { "name": "paths", "status": "ok", "elapsed_ms": 3, "details": { … } },
    {
      "name": "account",
      "status": "error",
      "elapsed_ms": 5,
      "error": {
        "code": "no_account_configured",
        "message": "No accounts found in shared Envelope HOME.",
        "remediation": [
          "envelope accounts import-keychain --email you@example.com --confirm-read --import",
          "envelope accounts add --email you@example.com"
        ]
      }
    }
  ]
}
```

### Reserved error codes

| Code | Phase | Cause |
|---|---|---|
| `home_missing` | paths | `HOME` env var unset |
| `home_drift` | paths | warn-only; non-fatal |
| `no_account_configured` | account | `default_account()` returned `None`, no `--account` |
| `account_not_found` | account | `--account <x>` did not resolve |
| `imap_dns_failed` | imap_auth | resolver error |
| `imap_tls_failed` | imap_auth | TLS handshake error |
| `imap_auth_failed` | imap_auth | LOGIN/AUTHENTICATE rejected |
| `imap_timeout` | imap_auth | exceeded `--timeout-secs` |
| `inbox_peek_failed` | inbox_peek | EXAMINE/FETCH error |

NEVER include passwords, tokens, TLS cert PEMs, or full server banners in `error.message`. Banners are fine post-redaction (host:port + class only).

---

## 5. Files (exact list)

New:
- `crates/cli/src/commands/quickstart.rs` — phase runner, public `run(...)` + `run_json(...)`, plus `QuickstartReport` types.
- `crates/cli/tests/quickstart_smoke.rs` — integration smoke (process-level, `assert_cmd`, no real network — uses `--skip-network`).

Modified:
- `crates/cli/src/main.rs`
  - Add `Commands::Quickstart { json: bool, account: Option<String>, folder: String, peek_limit: u32, timeout_secs: u64, skip_network: bool }` (insert near `Paths`).
  - Dispatch arm calls `commands::quickstart::run(...)`.
- `crates/cli/src/commands/mod.rs` — `pub mod quickstart;`
- `crates/email/src/imap.rs` (or wherever `connect`/`fetch_inbox` live) — add `examine_inbox(client, folder, limit) -> Result<Vec<MessageHeader>>` that uses `EXAMINE` + `BODY.PEEK[HEADER.FIELDS (FROM SUBJECT DATE)]`. Existing `fetch_inbox` stays untouched.
- `README.md` — one short "Quickstart" subsection (added only after merge, not in this spec).
- `CHANGELOG.md` — entry under Unreleased.

Not touched:
- credential store, account schema, dashboard, rules, evidence, migrate. Quickstart is read-only orientation; it must not regress those surfaces.

---

## 6. Invariants (non-negotiable)

1. **Read-only against mailboxes.** Phase 4 uses `EXAMINE` + `BODY.PEEK[]`, never `SELECT` + `FETCH BODY[]`. No flag mutation, no implicit `\Seen`. Mirrors evidence-export invariants in `CLAUDE.md`.
2. **No secrets anywhere.** Status JSON, error messages, logs, tests, fixtures, docs. Audit pass required before merge.
3. **No credential store mutation.** Quickstart never writes to keychain or credential file. If `--account` is unresolved, fail; do not "helpfully" import.
4. **Idempotent.** Running N times produces no observable side effects beyond log/stdout. No temp files, no `Last-Quickstart-At` markers, no telemetry pings.
5. **HOME-aware.** Reuse `paths::collect_report()` — do not re-implement HOME resolution. Drift is a warn, missing HOME is fatal.
6. **JSON is contract.** Field renames or removals require version bump in `schema` (`envelope.quickstart.v1` → `v2`). New optional fields are fine.
7. **Bounded.** `peek_limit` capped at 25 server-side. `timeout_secs` capped at 60. No "fetch all" mode; that's what `inbox` is for.
8. **No interactive prompts in MVP.** Spec'd guided/interactive mode is a follow-up. `quickstart` running in CI must never block on stdin.
9. **Exit codes are stable.** 0 ok, 2 no-account, 3 imap auth/connect, 4 imap peek, 1 anything else (paths/HOME/internal). Documented in `--help` and tests.

---

## 7. TDD plan

Order of implementation. Each step ends green before the next starts.

### 7.1 Unit: report shape and serialization

- `crates/cli/src/commands/quickstart.rs::tests`
  - `phases_serialize_in_order` — phases array order is paths, account, imap_auth, inbox_peek.
  - `failure_short_circuits` — once a phase errors, subsequent phases are absent (NOT `skipped` — absent), `ok: false`, `failed_phase` set.
  - `success_emits_next_steps` — `next_steps` non-empty when all phases ok.
  - `error_redacts_secrets` — feed a synthetic IMAP error containing `password=hunter2`; assert no `hunter2` substring in any serialized field.
  - `schema_field_pinned` — exact string `"envelope.quickstart.v1"`.
  - `exit_code_for_failed_phase` — table-driven: paths→1, account→2, imap_auth→3, inbox_peek→4.

### 7.2 Unit: paths phase

- Reuses existing `paths::collect_report`. New tests:
  - `paths_phase_warns_on_home_drift` — synthetic report with non-empty warnings → status `warn`, non-fatal.
  - `paths_phase_errors_on_missing_home` — `home: None` → status `error`, code `home_missing`, fatal.

### 7.3 Unit: account phase

- `account_phase_uses_default_when_unspecified`
- `account_phase_errors_on_no_accounts` — `Database::default_account()` returns `None` → code `no_account_configured`, remediation list non-empty, exit 2.
- `account_phase_resolves_explicit_account` — pass `--account <email>`, mock store has it.
- `account_phase_errors_on_unknown_account` — code `account_not_found`.

### 7.4 Unit: imap_auth + inbox_peek (with mock IMAP)

Use the existing IMAP transport's mock/test harness if one exists; otherwise stub the `Transport` trait at the `quickstart` boundary. Do NOT introduce a new mock framework just for this.

- `imap_auth_records_elapsed_ms`
- `imap_auth_redacts_banner` — server banner contains `* OK [CAPABILITY … LOGIN-REFERRALS]` with a fake credential — assert post-redaction stripped.
- `imap_auth_errors_classify` — separate codes for DNS / TLS / auth / timeout.
- `inbox_peek_uses_examine` — assert the IMAP command stream contains `EXAMINE INBOX` and never `SELECT INBOX`. This is the read-only invariant test; if it fails, ship is blocked.
- `inbox_peek_uses_body_peek` — assert `BODY.PEEK[` appears, `BODY[` (without `.PEEK`) does not.
- `inbox_peek_caps_limit_at_25`.

### 7.5 Process-level smoke (`crates/cli/tests/quickstart_smoke.rs`)

Use `assert_cmd` against the built binary, with `--skip-network`:

- `quickstart_skip_network_text_ok` — ephemeral HOME, seeded sqlite store with one account fixture, asserts text output contains `paths       ok` and `account     ok` and exits 0.
- `quickstart_skip_network_no_account_text` — empty store, exit 2, stderr-or-stdout contains "No accounts found".
- `quickstart_skip_network_json_schema` — `--json --skip-network`, parse stdout as JSON, assert `schema == "envelope.quickstart.v1"`, `phases[0].name == "paths"`, `phases[1].name == "account"`, length 2.
- `quickstart_help_documents_exit_codes` — `--help` includes each numeric exit code.
- `quickstart_idempotent` — run twice, snapshot DB hash before/after, assert equal.

### 7.6 Manual / out-of-CI smoke (documented, not automated)

These run against a real mailbox before merge; they live in §8 (smoke matrix), not in CI.

---

## 8. Smoke matrix (pre-merge, manual)

Run from a clean workstation with `HOME=/Users/wondermonkey` and shared Envelope HOME. Each row is one command; all must pass.

| # | Command | Expected |
|---|---|---|
| 1 | `envelope quickstart` (1 account configured) | text output, all 4 phases ok, exit 0, < 5s on broadband |
| 2 | `envelope quickstart --json` | parseable JSON v1, `ok: true`, exit 0 |
| 3 | `envelope quickstart` (no accounts) | guided text, exit 2, no DB writes |
| 4 | `envelope quickstart --json` (no accounts) | failure JSON with `error.code == "no_account_configured"`, exit 2 |
| 5 | `envelope quickstart --account does-not-exist@x` | exit 2, code `account_not_found` |
| 6 | `envelope quickstart --skip-network` | phases 1–2 only, exit 0 |
| 7 | `envelope quickstart` with intentionally wrong password (test account) | exit 3, code `imap_auth_failed`, no password leaked anywhere in stdout/stderr |
| 8 | `envelope quickstart` then `envelope inbox --limit 5` | inbox listing unchanged in flag state (no `\Seen` newly added). Confirm with Mail.app or another client. |
| 9 | `envelope quickstart` ten times in a row | DB byte-identical across runs (hash check) |
| 10 | `envelope quickstart --folder Archive` | peeks Archive folder, exit 0 |
| 11 | `cargo fmt --check && cargo test --workspace` | green |
| 12 | `cargo clippy --workspace --all-targets -- -D warnings` on the new crate's files only (baseline-aware) | green for new code |

Row 8 is the read-only proof. If it fails, the feature does not ship.

---

## 9. Out of scope for MVP (follow-up tickets)

- Interactive `--guided` mode that runs `accounts import-keychain` walkthrough.
- SMTP send-self test as a fifth phase (touches mailbox state — needs separate approval).
- OAuth-token quickstart for Gmail/iCloud (depends on OAuth account support landing).
- Watch/IDLE smoke as a phase.
- Telemetry (intentionally never).
- Dashboard "Quickstart" tile.

Each gets its own kanban card after MVP is merged and Tyler approves the surface.

---

## 10. Constraints from `CLAUDE.md` (re-stated for reviewer)

- Canonical impl is `u1f4e7-repo/`; do not touch archived Python.
- Public command path: `/Users/wondermonkey/.local/bin/envelope`. Quickstart must work through the wrapper without extra shims.
- Shared Envelope HOME: `/Users/wondermonkey/.hermes/shared/envelope-home` — quickstart paths phase must report this when configured.
- Version: target `0.7.x`; do not bump in this spec.
- No regression of dashboard rules-control work; quickstart adds a new command, no shared modules edited.
- No credentials or DB contents in any output.
- `cargo fmt --check`, `cargo test --workspace` must pass. Clippy uses baseline-aware review — quickstart code itself must be clippy-clean at `-D warnings`.
- Do not install or package binary until tests + review + Tyler approval.

---

## 11. Reviewer checklist (paste into PR)

- [ ] All 4 phases present, in order, in both text and JSON output.
- [ ] `schema: "envelope.quickstart.v1"` is exact.
- [ ] `EXAMINE`+`BODY.PEEK[]` enforced by test (`inbox_peek_uses_examine`).
- [ ] No `SELECT` in quickstart code path.
- [ ] No password/token/banner leakage in any test fixture, log, or assertion message.
- [ ] Exit codes 0/1/2/3/4 documented in `--help`.
- [ ] Idempotency test passes (DB hash unchanged after 10 runs).
- [ ] `--skip-network` works without DNS or sockets.
- [ ] No new clippy warnings introduced.
- [ ] No changes to credential store, evidence, migrate, dashboard rules.
- [ ] CHANGELOG entry under Unreleased.
- [ ] Spec smoke matrix §8 rows executed manually; results pasted into PR.

---

End of spec.
