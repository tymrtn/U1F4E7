// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Crash recovery and receipts, end to end against the real binary in an
//! isolated `ENVELOPE_HOME`, with no network.
//!
//! A process killed mid-send leaves its row `sending`. These tests seed that
//! state directly (the kill itself is exercised by the Mailroom B4 bench) and
//! check what the next `draft show` / `draft send` makes of it, and that every
//! send operation leaves a receipt a reader can match to the message.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use envelope_email_store::Database;
use serde_json::Value;

fn envelope_bin() -> &'static str {
    env!("CARGO_BIN_EXE_envelope")
}

fn run_cli(home: &Path, args: &[&str]) -> Output {
    Command::new(envelope_bin())
        .args(args)
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
        .env_remove("ENVELOPE_SEND_COOLDOWN_SECONDS")
        .output()
        .expect("run envelope cli")
}

fn json_of(out: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{') || l.trim_start().starts_with('['))
        .unwrap_or_else(|| {
            panic!(
                "no JSON on stdout (exit {:?})\nstdout: {stdout}\nstderr: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            )
        });
    serde_json::from_str(line).unwrap_or_else(|_| serde_json::from_str(&stdout).expect("json"))
}

fn app_dir(home: &Path) -> PathBuf {
    home.join("envelope-email")
}

fn open_db(home: &Path) -> Database {
    Database::open(&app_dir(home).join("envelope.db")).expect("open db")
}

fn seed_account(home: &Path) -> String {
    let mut child = Command::new(envelope_bin())
        .args([
            "accounts",
            "add",
            "--skip-login-check",
            "--email",
            "sender@example.test",
            "--password-stdin",
            "--smtp-host",
            "smtp.example.test",
            "--smtp-port",
            "587",
            "--imap-host",
            "imap.example.test",
            "--imap-port",
            "993",
            "--insecure-machine-key",
            "--json",
        ])
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn accounts add");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"pw\n")
        .expect("write password");
    let out = child.wait_with_output().expect("wait accounts add");
    assert!(out.status.success(), "seed account failed");
    open_db(home)
        .conn()
        .query_row("SELECT id FROM accounts LIMIT 1", [], |r| r.get(0))
        .expect("account id")
}

fn host_name() -> String {
    let out = Command::new("hostname").output().expect("hostname");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// SHA-256 of the lease token `lost-token`, as a claim records it.
const LOST_TOKEN_SHA256: &str = "76a345a783c39b9a1d0343ab6d2fa54baab38c47dcdcda2176b260c3b635dfbe";

/// A draft left `sending` by a process that died mid-attempt: the state a
/// SIGKILL leaves behind. `phase` is how far the attempt had got. The dead
/// owner's lock file stays on disk, unlocked, as a killed process leaves it.
fn stranded_draft(home: &Path, account_id: &str, phase: &str) -> String {
    let db = open_db(home);
    let draft = db
        .create_draft(
            account_id,
            "alice@example.test",
            Some("Crash test"),
            Some("Line one\nLine two"),
            None,
            None,
            None,
            None,
            Some("cli"),
        )
        .expect("create draft");
    let attempt_id = uuid::Uuid::new_v4().to_string();
    let metadata = serde_json::json!({
        "agent_body_text": "Line one\nLine two",
        "send_attempt": {
            "format": 1,
            "attempt_id": attempt_id,
            "message_id": "<stranded@example.test>",
            "phase": phase,
            "owner": {"pid": 999_999, "host": host_name()},
            "claimed_at": chrono::Utc::now().to_rfc3339(),
            "surface": "cli_draft_send",
            "agent_id": null,
            "lease_sha256": LOST_TOKEN_SHA256,
            "seq": 1,
        }
    });
    db.conn()
        .execute(
            "UPDATE drafts SET status = 'sending', operation_token = 'lost-token',
                metadata = ?1, updated_at = datetime('now')
             WHERE id = ?2",
            rusqlite::params![metadata.to_string(), draft.id],
        )
        .expect("strand draft");
    let locks = app_dir(home).join("send-locks");
    std::fs::create_dir_all(&locks).expect("lock dir");
    std::fs::write(locks.join(format!("{attempt_id}.lock")), b"").expect("dead lock file");
    draft.id
}

/// A draft left `sending` by a binary that recorded no attempt, claimed
/// `minutes_ago` minutes ago.
fn legacy_stranded_draft(home: &Path, account_id: &str, minutes_ago: i64) -> String {
    let db = open_db(home);
    let draft = db
        .create_draft(
            account_id,
            "alice@example.test",
            Some("Legacy claim"),
            Some("Body"),
            None,
            None,
            None,
            None,
            Some("cli"),
        )
        .expect("create draft");
    db.conn()
        .execute(
            "UPDATE drafts SET status = 'sending', operation_token = 'old-binary',
                updated_at = datetime('now', ?1)
             WHERE id = ?2",
            rusqlite::params![format!("-{minutes_ago} minutes"), draft.id],
        )
        .expect("strand draft");
    draft.id
}

fn status_of(home: &Path, draft_id: &str) -> String {
    let out = run_cli(home, &["draft", "show", draft_id, "--json"]);
    assert!(
        out.status.success(),
        "draft show failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    json_of(&out)["status"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn send_receipts(home: &Path) -> Vec<Value> {
    let out = run_cli(home, &["actions", "tail", "--limit", "50", "--json"]);
    assert!(
        out.status.success(),
        "actions tail failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows: Value = serde_json::from_slice(&out.stdout).expect("actions json");
    rows.as_array()
        .expect("array")
        .iter()
        .filter(|r| r["action_type"] == "send")
        .cloned()
        .collect()
}

fn taken(receipt: &Value) -> Value {
    serde_json::from_str(receipt["action_taken"].as_str().expect("action_taken"))
        .expect("receipt body is JSON")
}

// ── Stale `sending` rows ──────────────────────────────────────────────────

/// Pilot failure: `draft_send_now/after_data/kill` left every row `sending`
/// forever, and the retry refused with "not sendable". A dead owner that had
/// started the body may have delivered: the row must read
/// `delivery_uncertain`, and the retry must say so without transmitting.
#[test]
fn a_dead_owner_mid_body_is_parked_delivery_uncertain_on_retry() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    let account_id = seed_account(home);
    let draft_id = stranded_draft(home, &account_id, "transmitting");

    let retry = run_cli(
        home,
        &[
            "draft",
            "send",
            &draft_id,
            "--attr",
            "informational",
            "--send-now",
            "--confirm-send-now",
            "--json",
        ],
    );
    assert!(
        !retry.status.success(),
        "an uncertain outcome is not a success"
    );
    let body = json_of(&retry);
    assert_eq!(body["status"], "delivery_uncertain", "retry output: {body}");
    assert_eq!(body["draft_id"], draft_id.as_str());
    assert_eq!(body["retryable"], false);
    assert_eq!(status_of(home, &draft_id), "delivery_uncertain");
}

/// `draft show` alone resolves the stranded row, so a reader never sees a
/// `sending` row whose owner is gone.
#[test]
fn draft_show_resolves_a_dead_owners_row() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    let account_id = seed_account(home);
    let draft_id = stranded_draft(home, &account_id, "transmitting");

    assert_eq!(status_of(home, &draft_id), "delivery_uncertain");

    let receipts = send_receipts(home);
    let parked = receipts
        .iter()
        .find(|r| r["draft_id"] == draft_id.as_str() && r["action_status"] == "delivery_uncertain")
        .unwrap_or_else(|| panic!("no delivery_uncertain receipt in {receipts:?}"));
    let body = taken(parked);
    assert_eq!(body["reason"], "owner_dead");
    assert_eq!(body["phase"], "uncertain");
    assert_eq!(
        body["recipients"],
        serde_json::json!(["alice@example.test"])
    );
    assert_eq!(parked["message_id"], "<stranded@example.test>");
}

/// A dead owner that never started the body sent nothing: the claim goes back
/// to `draft` so the next attempt can send it once.
#[test]
fn a_dead_owner_before_the_body_is_released() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    let account_id = seed_account(home);
    let draft_id = stranded_draft(home, &account_id, "claimed");

    assert_eq!(status_of(home, &draft_id), "drafted");
}

/// An owner that still holds its lock is mid-send: nothing may touch its row.
#[test]
fn a_live_owners_row_is_left_alone() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    let account_id = seed_account(home);
    let draft_id = stranded_draft(home, &account_id, "transmitting");
    let db = open_db(home);
    let attempt_id: String = db
        .conn()
        .query_row(
            "SELECT json_extract(metadata, '$.send_attempt.attempt_id') FROM drafts WHERE id = ?1",
            [&draft_id],
            |r| r.get(0),
        )
        .expect("attempt id");
    let lock = std::fs::File::open(
        app_dir(home)
            .join("send-locks")
            .join(format!("{attempt_id}.lock")),
    )
    .expect("lock file");
    lock.lock().expect("hold the owner lock");

    assert_eq!(status_of(home, &draft_id), "sending");
    let retry = run_cli(
        home,
        &[
            "draft",
            "send",
            &draft_id,
            "--attr",
            "informational",
            "--send-now",
            "--confirm-send-now",
            "--json",
        ],
    );
    assert!(!retry.status.success());
    let body = json_of(&retry);
    assert_eq!(body["status"], "sending", "retry output: {body}");
    assert_eq!(body["retryable"], true);
    drop(lock);
}

/// Rows claimed by a binary that recorded no attempt are parked once the
/// lease has passed; younger ones are left for their owner.
#[test]
fn a_legacy_claim_is_parked_only_after_the_lease() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    let account_id = seed_account(home);
    let old = legacy_stranded_draft(home, &account_id, 20);
    let young = legacy_stranded_draft(home, &account_id, 1);

    assert_eq!(status_of(home, &old), "delivery_uncertain");
    assert_eq!(status_of(home, &young), "sending");
}

// ── Receipts and intents ──────────────────────────────────────────────────

fn queue_send(home: &Path, extra: &[&str]) -> Output {
    let mut args = vec![
        "send",
        "--to",
        "alice@example.test",
        "--subject",
        "Crash test",
        "--body",
        "Line one\nLine two",
        "--attr",
        "informational",
        "--json",
    ];
    args.extend_from_slice(extra);
    run_cli(home, &args)
}

/// Pilot failure: 0 `action_log` rows for 450 operations. A queued send must
/// leave a receipt carrying the operation id, principal, recipients, payload
/// digests and outcome, and `draft show` must show the queued body.
#[test]
fn a_queued_send_leaves_a_complete_receipt() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    let account_id = seed_account(home);

    let out = queue_send(home, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let queued = json_of(&out);
    assert_eq!(queued["status"], "queued");
    let draft_id = queued["draft_id"].as_str().expect("draft_id").to_string();

    let receipts = send_receipts(home);
    assert_eq!(
        receipts.len(),
        1,
        "one operation, one receipt: {receipts:?}"
    );
    let receipt = &receipts[0];
    assert_eq!(receipt["draft_id"], draft_id.as_str());
    assert_eq!(receipt["account_id"], account_id.as_str());
    assert_eq!(receipt["action_status"], "queued");
    let body = taken(receipt);
    assert_eq!(body["receipt"], "envelope.send_receipt.v1");
    assert_eq!(
        body["recipients"],
        serde_json::json!(["alice@example.test"])
    );
    assert_eq!(body["payload_sha256"].as_str().map(str::len), Some(64));
    // The Mailroom bench's payload_hash("Crash test", ["alice@example.test"],
    // "Line one\nLine two"), computed by its Python implementation.
    assert_eq!(
        body["semantic_sha256"],
        "523f0d8bc7e76d999b5ce167fed8429b8e54365cd26984d1b84fc28f5594881e"
    );

    let shown = json_of(&run_cli(home, &["draft", "show", &draft_id, "--json"]));
    assert_eq!(shown["content"]["agent_body_text"], "Line one\nLine two");
}

/// Rerunning an identical queued send returns the queued draft instead of
/// queueing a second copy.
#[test]
fn rerunning_an_identical_queued_send_returns_the_same_draft() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);

    let first = json_of(&queue_send(home, &[]));
    let second = json_of(&queue_send(home, &[]));
    assert_eq!(first["draft_id"], second["draft_id"]);
    assert_eq!(second["status"], "queued");
    assert_eq!(second["idempotent_replay"], true);
    let rows: i64 = open_db(home)
        .conn()
        .query_row("SELECT COUNT(*) FROM drafts", [], |r| r.get(0))
        .expect("count");
    assert_eq!(rows, 1);
}

/// An explicit key is the caller's operation id: the same key with different
/// content is a conflict and sends nothing; a different key is a new message.
#[test]
fn an_explicit_idempotency_key_binds_one_payload() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);

    let first = json_of(&queue_send(home, &["--idempotency-key", "op-1"]));
    assert_eq!(first["status"], "queued");

    let drifted = run_cli(
        home,
        &[
            "send",
            "--to",
            "alice@example.test",
            "--subject",
            "Crash test",
            "--body",
            "Different body",
            "--attr",
            "informational",
            "--idempotency-key",
            "op-1",
            "--json",
        ],
    );
    assert!(!drifted.status.success());
    let conflict = json_of(&drifted);
    assert_eq!(conflict["status"], "idempotency_key_conflict");
    assert_eq!(conflict["draft_id"], first["draft_id"]);

    let other = json_of(&queue_send(home, &["--idempotency-key", "op-2"]));
    assert_ne!(other["draft_id"], first["draft_id"]);
    let rows: i64 = open_db(home)
        .conn()
        .query_row("SELECT COUNT(*) FROM drafts", [], |r| r.get(0))
        .expect("count");
    assert_eq!(rows, 2);
}
