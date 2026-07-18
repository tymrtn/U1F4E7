// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

fn envelope_bin() -> &'static str {
    env!("CARGO_BIN_EXE_envelope")
}

fn run_envelope(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(envelope_bin())
        .args(args)
        // Explicit master key: `accounts add` no longer silently derives a
        // machine key, so automation must supply a key source. Fake material.
        .env("ENVELOPE_MASTER_KEY", "test-master-key-quickstart-smoke")
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
        .output()
        .expect("run envelope")
}

fn run_envelope_with_stdin(home: &Path, args: &[&str], input: &str) -> std::process::Output {
    let mut child = Command::new(envelope_bin())
        .args(args)
        .env("ENVELOPE_MASTER_KEY", "test-master-key-quickstart-smoke")
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
        .write_all(format!("{input}\n").as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait for envelope")
}

fn seed_account(home: &Path) {
    let output = run_envelope_with_stdin(
        home,
        &[
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
        ],
        "test-password",
    );
    assert!(
        output.status.success(),
        "seed account failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn paths_json(home: &Path) -> Value {
    let output = run_envelope(home, &["--json", "paths"]);
    assert!(
        output.status.success(),
        "paths failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("paths JSON")
}

#[test]
fn quickstart_skip_network_with_account_is_local_only_and_ok() {
    let temp = tempfile::tempdir().expect("temp HOME");
    seed_account(temp.path());

    let paths = paths_json(temp.path());
    let db_path = paths["database_path"].as_str().expect("database path");
    let before = fs::metadata(db_path).expect("database metadata before");

    let output = run_envelope(temp.path(), &["--json", "quickstart", "--skip-network"]);
    assert!(
        output.status.success(),
        "quickstart failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty(), "quickstart wrote stderr");

    let after = fs::metadata(db_path).expect("database metadata after");
    assert_eq!(
        after.len(),
        before.len(),
        "quickstart changed database size"
    );
    assert_eq!(
        after.modified().ok(),
        before.modified().ok(),
        "quickstart modified the database file"
    );

    let report: Value = serde_json::from_slice(&output.stdout).expect("quickstart JSON");
    assert_eq!(report["schema"], "envelope.quickstart.v1");
    assert_eq!(report["ok"], true);

    let phases = report["phases"].as_array().expect("phases");
    let phase_names: Vec<_> = phases.iter().map(|phase| &phase["name"]).collect();
    assert_eq!(phase_names, vec!["paths", "account"]);

    let next_steps = report["next_steps"].as_array().expect("next steps");
    assert!(
        next_steps
            .iter()
            .any(|step| step == "envelope mcp --config")
    );
    assert!(
        next_steps
            .iter()
            .any(|step| { step.as_str().unwrap_or_default().contains("draft-only") })
    );
}
