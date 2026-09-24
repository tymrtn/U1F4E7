// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)
//
// First-run behavior a new user hits: `accounts add` must prove the login
// before saving it, and `doctor` must exit non-zero on an error status.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

fn envelope(home: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_envelope"))
        .args(args)
        .env("ENVELOPE_MASTER_KEY", "test-master-key-first-run")
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn envelope");
    if let Some(input) = stdin {
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(format!("{input}\n").as_bytes())
            .unwrap();
    }
    drop(child.stdin.take());
    child.wait_with_output().expect("wait for envelope")
}

/// A loopback port with nothing listening, so the IMAP login fails fast.
fn closed_port() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port.to_string()
}

fn add_args<'a>(port: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "accounts",
        "add",
        "--email",
        "new-user@example.test",
        "--password-stdin",
        "--smtp-host",
        "127.0.0.1",
        "--smtp-port",
        "587",
        "--imap-host",
        "127.0.0.1",
        "--imap-port",
        port,
    ];
    args.extend_from_slice(extra);
    args
}

fn account_count(home: &Path) -> usize {
    let out = envelope(home, &["--json", "accounts", "list"], None);
    if !out.status.success() {
        return 0;
    }
    let value: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    value.as_array().map(Vec::len).unwrap_or(0)
}

#[test]
fn accounts_add_refuses_to_save_a_login_it_could_not_verify() {
    let home = tempfile::tempdir().unwrap();
    let port = closed_port();
    let out = envelope(home.path(), &add_args(&port, &[]), Some("wrong-password"));

    assert!(
        !out.status.success(),
        "add must fail when the login check fails"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("login check"), "stderr: {stderr}");
    assert!(stderr.contains("--skip-login-check"), "stderr: {stderr}");
    assert_eq!(account_count(home.path()), 0, "nothing is saved");
}

#[test]
fn accounts_add_skip_login_check_saves_for_offline_setup() {
    let home = tempfile::tempdir().unwrap();
    let port = closed_port();
    let out = envelope(
        home.path(),
        &add_args(&port, &["--skip-login-check"]),
        Some("offline-password"),
    );

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(account_count(home.path()), 1);
}

#[test]
fn doctor_exits_non_zero_on_an_error_status() {
    let home = tempfile::tempdir().unwrap();
    let out = envelope(home.path(), &["--json", "doctor"], None);

    let report: Value = serde_json::from_slice(&out.stdout).expect("doctor prints JSON");
    assert_eq!(report["status"], "missing_db");
    assert_eq!(report["severity"], "error");
    assert_eq!(out.status.code(), Some(1));
}
