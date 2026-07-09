// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! CLI integration tests for `envelope license` commands and the end-to-end
//! agent-limit unlock flow. Tests run the built binary against isolated HOME
//! directories so no real mailbox or DB is touched.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn envelope_bin() -> &'static str {
    env!("CARGO_BIN_EXE_envelope")
}

fn run(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(envelope_bin())
        .args(args)
        .env("HOME", home)
        .output()
        .expect("run envelope")
}

fn json_stdout(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout was not JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

/// A valid license key under the defined format: `env-lic-` + ≥16 alnum/hyphen chars.
const VALID_KEY: &str = "env-lic-testkey1234567890";

// ── format validation ────────────────────────────────────────────────

#[test]
fn activate_bad_format_exits_nonzero_with_stable_code_json() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let out = run(temp.path(), &["--json", "license", "activate", "bad-key"]);
    assert!(
        !out.status.success(),
        "bad-format key must be rejected (non-zero exit)"
    );
    let payload = json_stdout(&out);
    assert_eq!(payload["status"], "error");
    assert_eq!(
        payload["error"]["code"], "license_key_invalid_format",
        "stable error code must be license_key_invalid_format"
    );
    // key_prefix is in the response — it may be a truncated version of the input.
    // Just verify the field exists; the redaction guarantee applies to valid keys (see
    // activate_never_echoes_full_key_in_output for that invariant).
    assert!(
        payload["key_prefix"].is_string(),
        "key_prefix should be a string field"
    );
}

#[test]
fn activate_bad_format_exits_nonzero_human_output() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let out = run(
        temp.path(),
        &["license", "activate", "no-prefix-here-at-all!!!"],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("license_key_invalid_format"),
        "human output must include the stable error code; got: {stderr}"
    );
}

// ── activate → status roundtrip ─────────────────────────────────────

#[test]
fn activate_then_status_roundtrip_json() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();

    // Status before activation: unlicensed.
    let pre = json_stdout(&run(home, &["--json", "license", "status"]));
    assert_eq!(pre["licensed"], false);
    assert_eq!(pre["key_prefix"], Value::Null);
    assert_eq!(pre["activated_at"], Value::Null);

    // Activate with a valid key.
    let act = run(home, &["--json", "license", "activate", VALID_KEY]);
    assert!(
        act.status.success(),
        "valid key must activate successfully; stderr: {}",
        String::from_utf8_lossy(&act.stderr)
    );
    let act_payload = json_stdout(&act);
    assert_eq!(act_payload["status"], "activated");

    // key_prefix must be present and must NOT contain the full secret suffix.
    let key_prefix = act_payload["key_prefix"]
        .as_str()
        .expect("key_prefix string");
    assert!(
        key_prefix.starts_with("env-lic-"),
        "prefix should include the key prefix chars"
    );
    assert!(
        !act_payload["key_prefix"]
            .as_str()
            .unwrap()
            .contains("testkey1234567890"),
        "key_prefix must not expose the secret suffix of the key"
    );

    // activated_at must be present.
    assert!(
        act_payload["activated_at"].is_string(),
        "activated_at must be a string"
    );

    // Status after activation: licensed.
    let post = json_stdout(&run(home, &["--json", "license", "status"]));
    assert_eq!(post["licensed"], true);
    assert_eq!(post["key_prefix"], act_payload["key_prefix"]);
    assert!(post["activated_at"].is_string());
    // Perpetual license: expires_at must be null in the status output.
    assert_eq!(
        post["expires_at"],
        Value::Null,
        "perpetual license must report expires_at as null"
    );
}

// ── idempotent re-activation ─────────────────────────────────────────

#[test]
fn activate_same_key_twice_is_idempotent() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();

    let first = run(home, &["--json", "license", "activate", VALID_KEY]);
    assert!(first.status.success());
    let first_payload = json_stdout(&first);
    let first_activated_at = first_payload["activated_at"].as_str().unwrap().to_string();

    // Second activate with the same key must succeed and report "already_active".
    let second = run(home, &["--json", "license", "activate", VALID_KEY]);
    assert!(
        second.status.success(),
        "re-activating same key must succeed; stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_payload = json_stdout(&second);
    assert_eq!(second_payload["status"], "activated");
    assert_eq!(second_payload["note"], "already_active");

    // activated_at must remain stable (not reset on idempotent re-activate).
    assert_eq!(
        second_payload["activated_at"].as_str().unwrap(),
        first_activated_at,
        "idempotent re-activate must not reset activated_at"
    );
}

// ── deactivate ───────────────────────────────────────────────────────

#[test]
fn deactivate_clears_license_and_reverts_to_unlicensed() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();

    // Activate first.
    assert!(
        run(home, &["license", "activate", VALID_KEY])
            .status
            .success()
    );
    assert_eq!(
        json_stdout(&run(home, &["--json", "license", "status"]))["licensed"],
        true
    );

    // Deactivate.
    let deact = run(home, &["--json", "license", "deactivate"]);
    assert!(
        deact.status.success(),
        "deactivate must succeed; stderr: {}",
        String::from_utf8_lossy(&deact.stderr)
    );
    let deact_payload = json_stdout(&deact);
    assert_eq!(deact_payload["status"], "deactivated");
    assert_eq!(deact_payload["had_active_license"], true);

    // Status must now show unlicensed.
    let post = json_stdout(&run(home, &["--json", "license", "status"]));
    assert_eq!(post["licensed"], false);
}

#[test]
fn deactivate_with_no_license_is_idempotent() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();

    // No license stored; deactivate should succeed anyway.
    let out = run(home, &["--json", "license", "deactivate"]);
    assert!(out.status.success());
    let payload = json_stdout(&out);
    assert_eq!(payload["status"], "deactivated");
    assert_eq!(payload["had_active_license"], false);
}

// ── key never leaks ──────────────────────────────────────────────────

#[test]
fn activate_never_echoes_full_key_in_output() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();

    let secret_key = "env-lic-supersecretkey12345678";

    for args in [
        vec!["license", "activate", secret_key],
        vec!["--json", "license", "activate", secret_key],
    ] {
        let out = run(home, &args);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stdout.contains("supersecretkey12345678"),
            "stdout must not contain the secret suffix; got: {stdout}"
        );
        assert!(
            !stderr.contains("supersecretkey12345678"),
            "stderr must not contain the secret suffix; got: {stderr}"
        );
    }
}

// ── end-to-end unlock: agent limit lifted after license activate ─────

#[test]
fn license_activate_unlocks_third_agent_create() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();

    // Create two agents to hit the free-tier cap.
    assert!(run(home, &["agent", "create", "one"]).status.success());
    assert!(run(home, &["agent", "create", "two"]).status.success());

    // Third creation denied without a license.
    let denied = run(home, &["--json", "agent", "create", "three"]);
    assert!(
        !denied.status.success(),
        "3rd agent without license must be denied"
    );
    let denied_payload = json_stdout(&denied);
    assert_eq!(denied_payload["status"], "denied");
    assert_eq!(
        denied_payload["error"]["code"],
        "agent_limit_license_required"
    );

    // Activate a valid license.
    let activated = run(home, &["--json", "license", "activate", VALID_KEY]);
    assert!(
        activated.status.success(),
        "license activate must succeed; stderr: {}",
        String::from_utf8_lossy(&activated.stderr)
    );
    assert_eq!(json_stdout(&activated)["status"], "activated");

    // Third agent creation now succeeds.
    let third = run(home, &["--json", "agent", "create", "three"]);
    assert!(
        third.status.success(),
        "3rd agent must be allowed after license activation; stderr: {}",
        String::from_utf8_lossy(&third.stderr)
    );
    let third_payload = json_stdout(&third);
    assert_eq!(third_payload["status"], "created");
    assert_eq!(third_payload["name"], "three");

    // A fourth agent is also allowed.
    let fourth = run(home, &["--json", "agent", "create", "four"]);
    assert!(
        fourth.status.success(),
        "4th agent must also be allowed after license activation"
    );
    assert_eq!(json_stdout(&fourth)["status"], "created");

    // After deactivating, the 5th agent is denied again if we're still at cap.
    let deact = run(home, &["license", "deactivate"]);
    assert!(deact.status.success());

    let fifth = run(home, &["--json", "agent", "create", "five"]);
    assert!(
        !fifth.status.success(),
        "agent creation must be denied after license is deactivated (4 active agents > free tier)"
    );
    assert_eq!(
        json_stdout(&fifth)["error"]["code"],
        "agent_limit_license_required"
    );
}
