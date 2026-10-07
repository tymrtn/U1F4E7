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
            "--skip-login-check",
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

/// A dry run never connects, so it shows what a confirmed publish does in
/// each state the server can be in, for the option the operator chose.
#[test]
fn publish_sieve_dry_run_shows_what_happens_to_an_active_script() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);
    let created = run_cli(
        home,
        &[
            "rule",
            "create",
            "--name",
            "archive",
            "--match-from",
            "*@sender.example",
            "--action",
            "move=Archive",
        ],
    );
    assert!(created.status.success());

    let plan = |extra: &[&str]| -> serde_json::Value {
        let mut args = vec!["--json", "rule", "publish-sieve"];
        args.extend_from_slice(extra);
        let out = run_cli(home, &args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let plan: serde_json::Value = serde_json::from_slice(&out.stdout).expect("plan JSON");
        assert_eq!(plan["status"], "dry_run");
        assert_eq!(plan["network_used"], false);
        assert_eq!(plan["exported_count"], 1);
        assert_eq!(plan["activation"]["deletes_scripts"], false);
        plan["activation"].clone()
    };

    let refuse = plan(&[]);
    assert_eq!(refuse["on_another_active_script"], "refuse");
    assert!(
        refuse["if_another_script_active"]
            .as_str()
            .unwrap()
            .starts_with("refuse and name that script; nothing is uploaded")
    );

    let keep = plan(&["--keep-existing"]);
    assert_eq!(keep["on_another_active_script"], "keep_existing");
    let keep_text = keep["if_another_script_active"].as_str().unwrap();
    assert!(
        keep_text.contains("upload \"envelope-rules-wrapper\", which runs that script first")
            && keep_text.contains("Without include support: refuse"),
        "{keep_text}"
    );

    let replace = plan(&["--replace-active", "roundcube"]);
    assert_eq!(replace["on_another_active_script"], "replace_active");
    assert!(
        replace["if_another_script_active"]
            .as_str()
            .unwrap()
            .starts_with(
                "if it is \"roundcube\": switch \"roundcube\" off (it stays on the server)"
            )
    );

    let text = run_cli(
        home,
        &["rule", "publish-sieve", "--replace-active", "roundcube"],
    );
    let stdout = String::from_utf8_lossy(&text.stdout);
    assert!(text.status.success());
    assert!(
        stdout.contains("another script active: if it is \"roundcube\"")
            && stdout.contains("Envelope never deletes a script on the server."),
        "{stdout}"
    );

    let both = run_cli(
        home,
        &[
            "rule",
            "publish-sieve",
            "--keep-existing",
            "--replace-active",
            "roundcube",
        ],
    );
    assert!(!both.status.success());
    assert!(
        String::from_utf8_lossy(&both.stderr).contains("cannot be used with"),
        "{}",
        String::from_utf8_lossy(&both.stderr)
    );
}
