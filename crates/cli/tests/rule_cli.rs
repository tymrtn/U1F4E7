// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! `envelope rule create` against the real binary in an isolated
//! `ENVELOPE_HOME`. No mailbox is contacted.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use envelope_email_store::Database;

fn envelope_bin() -> &'static str {
    env!("CARGO_BIN_EXE_envelope")
}

fn run_cli(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(envelope_bin())
        .args(args)
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
        .output()
        .expect("run envelope cli")
}

/// Seed one offline account so `rule create` resolves a default account.
/// Uses the insecure machine key (test-only).
fn seed_account(home: &Path) {
    let mut child = Command::new(envelope_bin())
        .args([
            "accounts",
            "add",
            "--email",
            "test@example.test",
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
}

fn stored_rule_count(home: &Path) -> i64 {
    let db = Database::open(&home.join("envelope-email/envelope.db")).expect("open db");
    db.conn()
        .query_row("SELECT COUNT(*) FROM rules", [], |r| r.get(0))
        .expect("count rules")
}

#[test]
fn rule_create_without_match_flags_is_refused_and_stores_nothing() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);

    let refused = run_cli(
        home,
        &["rule", "create", "--name", "oops", "--action", "delete"],
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success(), "stderr: {stderr}");
    assert!(
        stderr.contains("a rule needs at least one match condition"),
        "stderr: {stderr}"
    );
    assert_eq!(stored_rule_count(home), 0);

    let created = run_cli(
        home,
        &[
            "rule",
            "create",
            "--name",
            "scoped",
            "--match-from",
            "*@x.example",
            "--action",
            "delete",
        ],
    );
    assert!(
        created.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert_eq!(stored_rule_count(home), 1);
}
