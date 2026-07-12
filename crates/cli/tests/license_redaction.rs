// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn envelope_bin() -> &'static str {
    env!("CARGO_BIN_EXE_envelope")
}

fn run_activate(home: &Path, json: bool, key: &str) -> std::process::Output {
    let mut command = Command::new(envelope_bin());
    if json {
        command.arg("--json");
    }
    let mut child = command
        .args(["license", "activate", "--key-stdin"])
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn envelope");
    child
        .stdin
        .as_mut()
        .expect("license stdin")
        .write_all(format!("{key}\n").as_bytes())
        .expect("write license key");
    child.wait_with_output().expect("wait for envelope")
}

#[test]
fn license_activate_never_echoes_key_material() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let secret = "env_live_secret_license_key_do_not_log";

    for json in [false, true] {
        let output = run_activate(temp.path(), json, secret);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            !output.status.success(),
            "activation is not implemented yet"
        );
        assert!(
            !stdout.contains(secret),
            "stdout leaked license key: {stdout}"
        );
        assert!(
            !stderr.contains(secret),
            "stderr leaked license key: {stderr}"
        );
    }
}

#[test]
fn legacy_positional_license_key_is_rejected_without_echoing_it() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let secret = "env-lic-supersecretkey12345678";
    let output = Command::new(envelope_bin())
        .args(["license", "activate", secret])
        .env("HOME", temp.path())
        .output()
        .expect("run envelope");

    assert!(!output.status.success(), "legacy input must be rejected");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains(secret),
        "stdout leaked positional key: {stdout}"
    );
    assert!(
        !stderr.contains(secret),
        "stderr leaked positional key: {stderr}"
    );
    assert!(
        stderr.contains("--key-stdin"),
        "migration error must name secure input: {stderr}"
    );
}
