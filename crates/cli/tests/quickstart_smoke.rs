// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn envelope_bin() -> &'static str {
    env!("CARGO_BIN_EXE_envelope")
}

fn run_envelope(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(envelope_bin())
        .args(args)
        .env("HOME", home)
        .output()
        .expect("run envelope")
}

fn seed_account(home: &Path) {
    let output = run_envelope(
        home,
        &[
            "--json",
            "accounts",
            "add",
            "--email",
            "agent@example.test",
            "--password",
            "test-password",
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
