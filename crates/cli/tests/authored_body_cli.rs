// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! End-to-end proof that an authored body carrying literal `\n` text never
//! reaches the stored draft that way.
//!
//! Real bug (2026-08-28): an agent composed through a shell, the shell handed
//! Envelope the two characters `\` and `n`, and the draft review page showed a
//! wall of text with visible `\n` markers where the paragraph breaks should be.
//!
//! These tests drive the real binary with a draft-only send (no network) and
//! read the stored draft back out of the database.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use envelope_email_store::Database;
use serde_json::Value;

fn envelope_bin() -> &'static str {
    env!("CARGO_BIN_EXE_envelope")
}

fn run_envelope(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(envelope_bin())
        .args(args)
        .env("ENVELOPE_MASTER_KEY", "test-master-key-authored-body")
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
        .output()
        .expect("run envelope")
}

fn seed_account(home: &Path) {
    let mut child = Command::new(envelope_bin())
        .args([
            "--json",
            "accounts",
            "add",
            "--email",
            "agent@example.test",
            "--password-stdin",
            "--name",
            "Agent Test",
            "--smtp-host",
            "smtp.example.test",
            "--smtp-port",
            "587",
            "--imap-host",
            "imap.example.test",
            "--imap-port",
            "993",
        ])
        .env("ENVELOPE_MASTER_KEY", "test-master-key-authored-body")
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn envelope");
    child
        .stdin
        .as_mut()
        .expect("envelope stdin")
        .write_all(b"test-password\n")
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait for envelope");
    assert!(
        output.status.success(),
        "seed account failed\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Draft-only send: exercises the whole authored-body path without SMTP or IMAP.
fn draft_only_send(home: &Path, subject: &str, body: &str) -> Value {
    let output = run_envelope(
        home,
        &[
            "send",
            "--to",
            "member@example.net",
            "--subject",
            subject,
            "--body",
            body,
            "--send-mode",
            "draft-only",
            "--attr",
            "informational",
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "send failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("send JSON")
}

fn stored_body(home: &Path, result: &Value) -> String {
    let paths = run_envelope(home, &["--json", "paths"]);
    let paths: Value = serde_json::from_slice(&paths.stdout).expect("paths JSON");
    let db_path = paths["database_path"].as_str().expect("database path");
    let db = Database::open(Path::new(db_path)).expect("open database");
    let draft_id = result["draft_id"].as_str().expect("draft id");
    db.get_draft(draft_id)
        .expect("get draft")
        .expect("draft exists")
        .text_content
        .expect("draft body")
}

#[test]
fn a_shell_escaped_body_is_repaired_before_it_is_stored() {
    let temp = tempfile::tempdir().expect("temp HOME");
    seed_account(temp.path());

    // Exactly what a shell hands over for --body "Hi,\n\nThanks".
    let result = draft_only_send(
        temp.path(),
        "We have your questionnaire",
        "Hi Alexander,\\n\\nThank you for completing it.\\n\\nTyler",
    );

    let body = stored_body(temp.path(), &result);
    assert_eq!(
        body, "Hi Alexander,\n\nThank you for completing it.\n\nTyler",
        "the stored draft must hold real line breaks"
    );
    assert!(
        !body.contains("\\n"),
        "no literal escape may survive into the draft: {body:?}"
    );

    let notice = &result["input_normalization"];
    assert_eq!(notice["applied"], Value::Bool(true));
    assert_eq!(notice["fields"][0]["field"], "body");
    assert_eq!(notice["fields"][0]["newlines_converted"], 4);
    assert!(
        notice["verify"]
            .as_str()
            .expect("verify text")
            .contains("before you report this task complete"),
        "the agent must be told to check the result: {notice}"
    );
}

#[test]
fn an_ambiguous_body_is_flagged_and_left_exactly_as_written() {
    let temp = tempfile::tempdir().expect("temp HOME");
    seed_account(temp.path());

    // Real line breaks AND a literal sequence: the text may be *about* escapes.
    let raw = "Escape a newline with \\n in the shell.\nThat is the whole trick.";
    let result = draft_only_send(temp.path(), "About escapes", raw);

    assert_eq!(
        stored_body(temp.path(), &result),
        raw,
        "ambiguous text must survive verbatim"
    );
    let notice = &result["input_normalization"];
    assert_eq!(notice["applied"], Value::Bool(false));
    assert_eq!(notice["fields"][0]["action"], "left_as_written");
    assert_eq!(notice["fields"][0]["newlines_left_as_written"], 1);
}

#[test]
fn a_clean_body_is_untouched_and_reports_nothing() {
    let temp = tempfile::tempdir().expect("temp HOME");
    seed_account(temp.path());

    let raw = "Hi Alexander,\n\nThanks for the note.\n\nTyler";
    let result = draft_only_send(temp.path(), "Clean body", raw);

    assert_eq!(stored_body(temp.path(), &result), raw);
    assert!(
        result.get("input_normalization").is_none(),
        "a clean body must not be annotated: {result}"
    );
}
