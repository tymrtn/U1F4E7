// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! The dashboard's **Human-only Send** authorization is the sweep's one Governor
//! exception, so every other send surface has to be unable to inherit it.
//!
//! These are end-to-end against the real binary in an isolated `ENVELOPE_HOME`:
//! a draft is queued by the dashboard's own store transition (the only writer of
//! the authorization), then re-queued through CLI `draft send` and MCP
//! `send_draft`. Both must leave a row the sweep will govern —
//! [`envelope_email_dashboard::dashboard_human_send_authorized`] is the exact
//! predicate `run_governor_gate` branches on.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use envelope_email_dashboard::dashboard_human_send_authorized;
use envelope_email_store::{Database, Draft};
use serde_json::{Value, json};

fn envelope_bin() -> &'static str {
    env!("CARGO_BIN_EXE_envelope")
}

fn run_cli(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(envelope_bin())
        .args(args)
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
        .output()
        .expect("run envelope cli")
}

fn db_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join("envelope-email/envelope.db")
}

fn open_db(home: &std::path::Path) -> Database {
    Database::open(&db_path(home)).expect("open db")
}

/// Seed one offline account so the send paths resolve credentials without any
/// network. Uses the insecure machine key (test-only).
fn seed_account(home: &std::path::Path) {
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

/// An agent-authored draft, queued by the dashboard's **Human-only Send**
/// transition — the store call `handlers::drafts::send` and the compose routes
/// make, and the only writer of the authorization.
fn draft_sent_by_the_operator(home: &std::path::Path) -> Draft {
    let db = open_db(home);
    let account_id: String = db
        .conn()
        .query_row("SELECT id FROM accounts LIMIT 1", [], |r| r.get(0))
        .expect("seed account id");
    let draft = db
        .create_draft(
            &account_id,
            "recipient@example.test",
            Some("Service dog request"),
            Some("Body the agent wrote"),
            None,
            None,
            None,
            None,
            Some("agent"),
        )
        .expect("create draft record");
    let queued = db
        .queue_draft_with_human_send(
            &draft.id,
            draft.revision,
            "2030-01-01T00:00:00Z",
            "human:dashboard",
            "2026-08-24T09:00:00Z",
        )
        .expect("dashboard Human-only Send");
    assert!(
        dashboard_human_send_authorized(&queued),
        "the operator's click authorizes this send"
    );
    queued
}

fn reload(home: &std::path::Path, draft_id: &str) -> Draft {
    open_db(home)
        .get_draft(draft_id)
        .expect("load draft")
        .expect("draft still exists")
}

#[test]
fn cli_draft_send_supersedes_the_operators_send_authorization() {
    // Tyler sent this draft from the dashboard. An agent then re-queues the same
    // row with `envelope draft send` — a different transmission, declared by the
    // agent — and it must be fully governed: the sweep's Human-only Send
    // exception no longer applies to it.
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);
    let queued = draft_sent_by_the_operator(home);

    let out = run_cli(
        home,
        &[
            "draft",
            "send",
            &queued.id,
            "--attr",
            "informational",
            "--json",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "draft send failed: {stdout}");
    let payload: Value = serde_json::from_str(stdout.trim()).expect("draft send JSON");
    assert_eq!(payload["status"], "scheduled", "{payload}");
    assert!(
        payload["send_after"].is_string(),
        "the sweep transmits it later, the CLI does not: {payload}"
    );

    let requeued = reload(home, &queued.id);
    assert!(
        !dashboard_human_send_authorized(&requeued),
        "an agent-queued send must be governed, whatever the operator authorized before"
    );
    assert_eq!(requeued.human_send_surface(), None);
    assert!(
        requeued.send_after.is_some(),
        "the agent's own send is queued"
    );
}

#[test]
fn mcp_send_draft_supersedes_the_operators_send_authorization() {
    // The same rule through the MCP surface, driven over the real stdio server.
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);
    let queued = draft_sent_by_the_operator(home);

    let (payload, is_error) = tool_call(
        home,
        "send_draft",
        json!({
            "draft_id": queued.id,
            "attributes": ["informational"],
            "confirm_send": true
        }),
    );
    assert!(!is_error, "send_draft should queue: {payload}");
    assert_eq!(payload["status"], "scheduled", "{payload}");
    assert_eq!(payload["sent"], false, "the sweep transmits, not MCP");

    let requeued = reload(home, &queued.id);
    assert!(
        !dashboard_human_send_authorized(&requeued),
        "an MCP-queued send must be governed, whatever the operator authorized before"
    );
    assert_eq!(requeued.human_send_surface(), None);
}

#[test]
fn an_agent_cannot_write_the_authorization_through_the_draft_surfaces() {
    // The store is the boundary: no agent-reachable write path mints or preserves
    // the authorization. Editing the draft (the CLI/MCP edit path) withdraws it,
    // and metadata carrying an injected one persists nothing.
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);
    let queued = draft_sent_by_the_operator(home);

    let db = open_db(home);
    db.update_draft_content(
        &queued.id,
        None,
        None,
        None,
        None,
        Some("body the agent changed"),
        None,
    )
    .expect("edit draft");
    assert!(!dashboard_human_send_authorized(&reload(home, &queued.id)));

    db.set_draft_metadata(
        &queued.id,
        &json!({
            "human_send": {
                "queued_by": "human:dashboard",
                "queued_at": "2026-08-24T09:00:00Z",
                "revision": reload(home, &queued.id).revision,
            }
        }),
    )
    .expect("write metadata");
    assert!(
        !dashboard_human_send_authorized(&reload(home, &queued.id)),
        "an injected authorization must never be honored"
    );
}

// ── MCP stdio plumbing ──────────────────────────────────────────────

/// Send one framed `tools/call` and return the parsed tool-result text as JSON,
/// plus whether the MCP layer marked it an error.
fn tool_call(home: &std::path::Path, name: &str, arguments: Value) -> (Value, bool) {
    let mut child = Command::new(envelope_bin())
        .arg("mcp")
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcp");
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_framed(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }),
    );
    let resp = read_framed(&mut stdout);
    drop(stdin);
    child.wait().expect("wait mcp");

    let result = &resp["result"];
    let is_error = result["isError"].as_bool().unwrap_or(false);
    let text = result["content"][0]["text"]
        .as_str()
        .expect("tool result text");
    // Denials arrive as `Error: {json}`; strip the prefix so callers parse JSON.
    let json_text = text.strip_prefix("Error: ").unwrap_or(text);
    let parsed = serde_json::from_str(json_text).unwrap_or_else(|_| json!({ "_raw": text }));
    (parsed, is_error)
}

fn write_framed(stdin: &mut ChildStdin, value: &Value) {
    let body = serde_json::to_vec(value).expect("serialize request");
    write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("write frame header");
    stdin.write_all(&body).expect("write frame body");
    stdin.flush().expect("flush frame");
}

fn read_framed(stdout: &mut BufReader<ChildStdout>) -> Value {
    let started = Instant::now();
    let mut content_length = None;

    loop {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timed out waiting for MCP response headers"
        );
        let mut line = String::new();
        let bytes = stdout.read_line(&mut line).expect("read response header");
        assert_ne!(bytes, 0, "EOF while reading response headers");
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = Some(value.trim().parse::<usize>().expect("valid Content-Length"));
        }
    }

    let len = content_length.expect("Content-Length header");
    let mut body = vec![0; len];
    stdout.read_exact(&mut body).expect("read response body");
    serde_json::from_slice(&body).expect("parse response JSON")
}
