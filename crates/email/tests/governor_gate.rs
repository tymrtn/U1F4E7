// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! End-to-end tests for the Governor send gate against a stubbed Governor CLI.
//!
//! These tests never send real email and never invoke the real Governor
//! binary; they point the gate at a tiny shell-script stub so we can assert the
//! allow/deny/review wiring deterministically.

use envelope_email_transport::outbound::{
    GovernorConfig, GovernorMode, GovernorRequest, SendSurface, gate,
};

const STUB_SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/governor-stub.sh"
);

/// Create a Governor stub at `dir/name` that prints `body` on stdout, exits 0,
/// and records its arguments to `dir/name.argv`.
///
/// The stub is a symlink to a checked-in script, so no test ever writes the
/// file it executes. Writing a script and then running it races with sibling
/// tests: a process they fork in that window inherits the open write handle,
/// and running the script fails with ETXTBSY ("Text file busy"), which the gate
/// correctly reports as `governor_unavailable`.
fn stub_governor(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(dir.join(format!("{name}.verdict")), body).unwrap();
    std::os::unix::fs::symlink(STUB_SCRIPT, &path).unwrap();
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

/// The stub records the exact argv it was invoked with, which lets us assert
/// the blind-attribution invocation shape without invoking the real Governor.
#[test]
fn gate_invocation_passes_attribute_keys_and_never_pii() {
    let dir = tempfile::tempdir().unwrap();
    let bin = stub_governor(
        dir.path(),
        "governor-record",
        r#"{"decision":"allow","state":"allowed","score":0.2}"#,
    );
    let argv_out = dir.path().join("governor-record.argv");
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
