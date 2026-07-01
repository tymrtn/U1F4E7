// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! End-to-end tests for the Governor send gate against a stubbed Governor CLI.
//!
//! These tests never send real email and never invoke the real Governor
//! binary; they point the gate at a tiny shell-script stub so we can assert the
//! allow/deny/review wiring deterministically.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;

use envelope_email_transport::outbound::{
    GovernorConfig, GovernorMode, GovernorRequest, SendSurface, gate,
};

/// Write an executable shell-script stub that prints `body` on stdout and exits 0.
fn stub_governor(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "#!/bin/sh").unwrap();
    // Emit the canned JSON regardless of arguments.
    writeln!(f, "cat <<'EOF'\n{body}\nEOF").unwrap();
    drop(f);
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

fn sample_request() -> GovernorRequest {
    GovernorRequest::build(
        "acct-1",
        Some("envelope.test"),
        "Quarterly numbers",
        "alice@example.com",
        None,
        None,
        SendSurface::Scheduled,
        Some("draft-1"),
        &[],
        false,
    )
}

#[test]
fn allow_verdict_from_stub_permits_send() {
    let dir = tempfile::tempdir().unwrap();
    let bin = stub_governor(
        dir.path(),
        "governor-allow",
        r#"{"decision":"allow","state":"allowed","score":0.9}"#,
    );
    let config = GovernorConfig {
        mode: GovernorMode::Required,
        bin: bin.to_string_lossy().to_string(),
    };
    let outcome = gate(&config, &sample_request());
    assert!(outcome.allowed, "stubbed allow verdict must permit send");
    assert_eq!(outcome.decision, "allow");
    assert!(outcome.block_code.is_none());
}

#[test]
fn review_verdict_from_stub_blocks_when_required() {
    let dir = tempfile::tempdir().unwrap();
    let bin = stub_governor(
        dir.path(),
        "governor-review",
        r#"{"decision":"review","state":"review_required","score":-0.04,"review_ticket":{"id":"review-9"}}"#,
    );
    let config = GovernorConfig {
        mode: GovernorMode::Required,
        bin: bin.to_string_lossy().to_string(),
    };
    let outcome = gate(&config, &sample_request());
    assert!(!outcome.allowed, "review verdict must block when required");
    assert_eq!(outcome.block_code.as_deref(), Some("governor_blocked"));
    assert_eq!(outcome.review_ticket_id.as_deref(), Some("review-9"));
}

#[test]
fn deny_verdict_from_stub_blocks_when_required() {
    let dir = tempfile::tempdir().unwrap();
    let bin = stub_governor(
        dir.path(),
        "governor-deny",
        r#"{"decision":"deny","state":"blocked"}"#,
    );
    let config = GovernorConfig {
        mode: GovernorMode::Required,
        bin: bin.to_string_lossy().to_string(),
    };
    let outcome = gate(&config, &sample_request());
    assert!(!outcome.allowed);
    assert_eq!(outcome.block_code.as_deref(), Some("governor_blocked"));
}

#[test]
fn missing_binary_fails_closed_when_required() {
    let config = GovernorConfig {
        mode: GovernorMode::Required,
        bin: "/nonexistent/governor-bin-zzz".to_string(),
    };
    let outcome = gate(&config, &sample_request());
    assert!(!outcome.allowed, "missing governor must fail closed");
    assert_eq!(outcome.block_code.as_deref(), Some("governor_unavailable"));
}

#[test]
fn warn_mode_allows_even_on_deny() {
    let dir = tempfile::tempdir().unwrap();
    let bin = stub_governor(dir.path(), "governor-deny-warn", r#"{"decision":"deny"}"#);
    let config = GovernorConfig {
        mode: GovernorMode::Warn,
        bin: bin.to_string_lossy().to_string(),
    };
    let outcome = gate(&config, &sample_request());
    assert!(outcome.allowed, "warn mode never blocks");
    assert_eq!(outcome.decision, "deny");
}

/// Write a stub that records the exact argv it was invoked with to `argv_out`,
/// then emits an allow verdict. Lets us assert the blind-attribution invocation
/// shape without invoking the real Governor.
fn stub_governor_recording(
    dir: &std::path::Path,
    name: &str,
    argv_out: &std::path::Path,
) -> std::path::PathBuf {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "#!/bin/sh").unwrap();
    writeln!(f, "echo \"$@\" > \"{}\"", argv_out.display()).unwrap();
    writeln!(
        f,
        "cat <<'EOF'\n{{\"decision\":\"allow\",\"state\":\"allowed\",\"score\":0.2}}\nEOF"
    )
    .unwrap();
    drop(f);
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

#[test]
fn gate_invocation_passes_attribute_keys_and_never_pii() {
    let dir = tempfile::tempdir().unwrap();
    let argv_out = dir.path().join("argv.txt");
    let bin = stub_governor_recording(dir.path(), "governor-record", &argv_out);
    let config = GovernorConfig {
        mode: GovernorMode::Required,
        bin: bin.to_string_lossy().to_string(),
    };

    // A threaded send with a BCC to an external recipient. The subject and
    // addresses are PII that must NEVER reach the Governor invocation.
    let req = GovernorRequest::build(
        "acct-1",
        Some("martin.fm"),
        "Confidential Q3 numbers",
        "Alice <alice@secret-client.com>",
        None,
        Some("silent@watcher.example"),
        SendSurface::Scheduled,
        Some("draft-1"),
        &[],
        true,
    );
    let outcome = gate(&config, &req);
    assert!(outcome.allowed);

    let argv = std::fs::read_to_string(&argv_out).unwrap();

    // Blind-attribution invocation: the score verb, the envelope catalog, and the
    // declared attribute keys are present.
    assert!(argv.contains("score"), "argv: {argv}");
    assert!(argv.contains("--catalog envelope"), "argv: {argv}");
    assert!(argv.contains("--attr reply_to_thread"), "argv: {argv}");
    assert!(argv.contains("--attr has_bcc"), "argv: {argv}");

    // No subject text and no recipient addresses/domains ever reach Governor.
    for needle in [
        "Confidential Q3 numbers",
        "alice@secret-client.com",
        "secret-client.com",
        "silent@watcher.example",
        "watcher.example",
    ] {
        assert!(!argv.contains(needle), "invocation leaked {needle}: {argv}");
    }
}
