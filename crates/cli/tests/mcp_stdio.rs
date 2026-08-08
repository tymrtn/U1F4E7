// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn envelope_bin() -> &'static str {
    env!("CARGO_BIN_EXE_envelope")
}

// ── Per-agent identity helpers ──────────────────────────────────────

fn run_cli(home: &std::path::Path, args: &[&str], token: Option<&str>) -> std::process::Output {
    let mut cmd = Command::new(envelope_bin());
    cmd.args(args).env("HOME", home).env("ENVELOPE_HOME", home);
    if let Some(t) = token {
        cmd.env("ENVELOPE_AGENT_TOKEN", t);
    }
    cmd.output().expect("run envelope cli")
}

fn run_cli_with_stdin(
    home: &std::path::Path,
    args: &[&str],
    token: Option<&str>,
    input: &str,
) -> std::process::Output {
    let mut cmd = Command::new(envelope_bin());
    cmd.args(args)
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
        .stdin(Stdio::piped());
    if let Some(t) = token {
        cmd.env("ENVELOPE_AGENT_TOKEN", t);
    }
    let mut child = cmd.spawn().expect("spawn envelope cli");
    child
        .stdin
        .as_mut()
        .expect("envelope stdin")
        .write_all(format!("{input}\n").as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait for envelope cli")
}

/// Seed one offline account so send/reply draft paths can resolve credentials
/// without any network. Uses the insecure machine key (test-only).
fn seed_account(home: &std::path::Path) {
    let out = run_cli_with_stdin(
        home,
        &[
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
        ],
        None,
        "pw",
    );
    assert!(out.status.success(), "seed account failed");
}

/// Create an agent and return (token, agent_id).
fn create_agent(home: &std::path::Path, name: &str) -> (String, String) {
    let out = run_cli(home, &["--json", "agent", "create", name], None);
    assert!(out.status.success(), "agent create failed");
    let v: Value = serde_json::from_slice(&out.stdout).expect("agent create JSON");
    (
        v["token"].as_str().unwrap().to_string(),
        v["id"].as_str().unwrap().to_string(),
    )
}

fn set_policy(home: &std::path::Path, name: &str, actions: &str, ceiling: &str) {
    let out = run_cli(
        home,
        &[
            "agent",
            "policy",
            "set",
            name,
            "--allow-accounts",
            "*",
            "--allow-folders",
            "*",
            "--allow-actions",
            actions,
            "--send-mode-ceiling",
            ceiling,
        ],
        None,
    );
    assert!(out.status.success(), "policy set failed");
}

/// Seed a draft record directly in the store and return its id, feeding
/// send_draft without any network. `draft create` now requires a live IMAP
/// APPEND (drafts must land in the real Drafts folder), and the seed account
/// points at an unreachable host — but the send-ceiling behavior under test is
/// independent of the IMAP transport, so we insert the draft row directly.
fn create_local_draft(home: &std::path::Path, to: &str) -> String {
    let db = envelope_email_store::Database::open(&db_path(home)).expect("open db");
    let account_id: String = db
        .conn()
        .query_row("SELECT id FROM accounts LIMIT 1", [], |r| r.get(0))
        .expect("seed account id");
    let draft = db
        .create_draft(
            &account_id,
            to,
            Some("hi"),
            Some("x"),
            None,
            None,
            None,
            None,
            Some("cli"),
        )
        .expect("create draft record");
    draft.id
}

/// Send one framed tools/call and return the parsed tool-result text as JSON,
/// plus whether the MCP layer marked it an error.
fn tool_call(
    home: &std::path::Path,
    token: Option<&str>,
    name: &str,
    arguments: Value,
) -> (Value, bool) {
    let mut cmd = Command::new(envelope_bin());
    cmd.arg("mcp").env("HOME", home).env("ENVELOPE_HOME", home);
    if let Some(t) = token {
        cmd.env("ENVELOPE_AGENT_TOKEN", t);
    }
    let mut child = cmd
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

/// Like [`tool_call`] but injects extra environment variables on the MCP server
/// process (e.g. a mock `ENVELOPE_GOVERNOR_BIN` / `ENVELOPE_GOVERNOR_MODE`).
fn tool_call_env(
    home: &std::path::Path,
    token: Option<&str>,
    name: &str,
    arguments: Value,
    extra_env: &[(&str, &str)],
) -> (Value, bool) {
    let mut cmd = Command::new(envelope_bin());
    cmd.arg("mcp").env("HOME", home).env("ENVELOPE_HOME", home);
    if let Some(t) = token {
        cmd.env("ENVELOPE_AGENT_TOKEN", t);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd
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
    let json_text = text.strip_prefix("Error: ").unwrap_or(text);
    let parsed = serde_json::from_str(json_text).unwrap_or_else(|_| json!({ "_raw": text }));
    (parsed, is_error)
}

/// Write an executable mock Governor binary that prints a fixed verdict and exits
/// 0. Returns its path. Unix-only (the dev/CI target).
#[cfg(unix)]
fn write_mock_governor(dir: &std::path::Path, decision: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("mock-governor.sh");
    let script = format!(
        "#!/bin/sh\nprintf '{{\"decision\": \"{decision}\", \"state\": \"review_required\"}}'\nexit 0\n"
    );
    std::fs::write(&path, script).expect("write mock governor");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

fn db_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join("envelope-email/envelope.db")
}

fn spawn_mcp(home: &std::path::Path) -> Child {
    Command::new(envelope_bin())
        .arg("mcp")
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn envelope mcp")
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
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = Some(value.trim().parse::<usize>().expect("valid Content-Length"));
            }
        }
    }

    let len = content_length.expect("Content-Length header");
    let mut body = vec![0; len];
    stdout.read_exact(&mut body).expect("read response body");
    serde_json::from_slice(&body).expect("parse response JSON")
}

#[test]
fn mcp_stdio_accepts_content_length_framed_initialize_and_tools_list() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let mut child = spawn_mcp(temp.path());
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_framed(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "envelope-test", "version": "0" }
            }
        }),
    );
    let init = read_framed(&mut stdout);
    assert_eq!(init["jsonrpc"], "2.0");
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "envelope");

    write_framed(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    );
    let tools = read_framed(&mut stdout);
    let tool_entries = tools["result"]["tools"].as_array().expect("tools array");
    // 22 mailbox tools + the read-only governor_catalog discovery tool (v2).
    assert_eq!(tool_entries.len(), 23);
    for name in [
        "bulk",
        "thread",
        "rules_preview",
        "rules_run",
        "watch_status",
        "snooze",
        "governor_catalog",
    ] {
        assert!(
            tool_entries.iter().any(|tool| tool["name"] == name),
            "tool {name} must be advertised"
        );
    }
    assert!(tool_entries.iter().any(|tool| tool["name"] == "send"));
    assert!(
        tool_entries
            .iter()
            .any(|tool| tool["name"] == "create_reply_draft")
    );
    assert!(
        tool_entries
            .iter()
            .any(|tool| tool["name"] == "modify_draft")
    );
    assert!(tool_entries.iter().any(|tool| tool["name"] == "send_draft"));
    assert_eq!(
        tool_entries
            .iter()
            .find(|tool| tool["name"] == "send")
            .expect("send tool")["inputSchema"]["properties"]["send_mode"]["default"],
        "draft-only"
    );

    drop(stdin);
    let status = child.wait().expect("wait for mcp process");
    assert!(status.success());
}

#[test]
fn mcp_content_tools_advertise_untrusted_trust_boundary() {
    // The content-returning MCP tools (read, inbox, search) must document that
    // their results are wrapped in the untrusted-content trust envelope, and
    // tools that do not return external email content must NOT. This asserts the
    // advertised contract via tools/list without touching any mailbox.
    let temp = tempfile::tempdir().expect("temp HOME");
    let mut child = spawn_mcp(temp.path());
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_framed(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "envelope-test", "version": "0" }
            }
        }),
    );
    let _init = read_framed(&mut stdout);

    write_framed(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    );
    let tools = read_framed(&mut stdout);
    let entries = tools["result"]["tools"].as_array().expect("tools array");

    let description_of = |name: &str| -> String {
        entries
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("tool {name} must exist"))["description"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };

    for wrapped in ["read", "inbox", "search"] {
        let desc = description_of(wrapped);
        assert!(
            desc.contains("UNTRUSTED") && desc.contains("_envelope_trust"),
            "wrapped tool {wrapped} description must document the untrusted trust envelope, got: {desc}"
        );
    }
    for unwrapped in ["folders", "accounts"] {
        let desc = description_of(unwrapped);
        assert!(
            !desc.contains("_envelope_trust"),
            "unwrapped tool {unwrapped} description must not advertise the trust envelope, got: {desc}"
        );
    }

    drop(stdin);
    child.wait().expect("wait for mcp process");
}

#[test]
fn contract_export_declares_untrusted_trust_model() {
    // The additive trust_model block must describe the wrapper for MCP consumers.
    let temp = tempfile::tempdir().expect("temp HOME");
    let output = Command::new(envelope_bin())
        .arg("contract")
        .env("HOME", temp.path())
        .env("ENVELOPE_HOME", temp.path())
        .output()
        .expect("run contract");
    assert!(output.status.success());
    let contract: Value = serde_json::from_slice(&output.stdout).expect("contract JSON");

    // Contract stays v1 (additive change only).
    assert_eq!(contract["schema"], "envelope.agent_contract.v2");

    let untrusted = &contract["trust_model"]["untrusted_content"];
    assert_eq!(untrusted["marker_key"], "_envelope_trust");
    assert_eq!(untrusted["marker_value"], "untrusted-content");
    assert_eq!(untrusted["warning_key"], "_warning");
    assert_eq!(untrusted["content_key"], "content");
    let wrapped = untrusted["wrapped_tools"]
        .as_array()
        .expect("wrapped_tools array");
    for name in ["inbox", "read", "search"] {
        assert!(
            wrapped.iter().any(|t| t == name),
            "trust_model must list {name} as wrapped"
        );
    }
    assert!(
        untrusted["applies_to"]
            .as_array()
            .expect("applies_to array")
            .iter()
            .any(|c| c == "mcp"),
        "trust_model must apply to mcp"
    );
}

#[test]
fn mcp_config_includes_runtime_snippets_and_draft_only_safety() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let output = Command::new(envelope_bin())
        .arg("mcp")
        .arg("--config")
        .env("HOME", temp.path())
        .env("ENVELOPE_HOME", temp.path())
        .output()
        .expect("run mcp --config");

    assert!(output.status.success());
    let config: Value = serde_json::from_slice(&output.stdout).expect("config JSON");
    let server = &config["mcpServers"]["envelope"];
    assert!(
        server["command"]
            .as_str()
            .unwrap_or_default()
            .ends_with("envelope")
    );
    assert_eq!(server["args"], json!(["mcp"]));
    assert_eq!(server["env"]["HOME"], temp.path().display().to_string());

    let setup = &config["envelopeAgentSetup"];
    assert!(
        setup["sendSafety"]
            .as_str()
            .unwrap_or_default()
            .contains("draft-only")
    );
    for runtime in ["claudeCode", "codex", "hermes"] {
        let runtime_setup = &setup[runtime];
        assert!(
            runtime_setup["target"]
                .as_str()
                .unwrap_or_default()
                .contains("config")
        );
        assert_eq!(runtime_setup["commandPath"], server["command"]);
        assert_eq!(runtime_setup["env"], server["env"]);
        assert!(
            runtime_setup["draftOnlySafety"]
                .as_str()
                .unwrap_or_default()
                .contains("draft-only")
        );
        let snippet = runtime_setup["snippet"].as_str().expect("runtime snippet");
        assert!(snippet.contains(server["command"].as_str().expect("command path")));
        assert!(snippet.contains("HOME"));
    }
}

// ── Per-agent identity: MCP enforcement ─────────────────────────────

#[test]
fn mcp_startup_fails_loud_on_unknown_token() {
    // A set-but-unknown ENVELOPE_AGENT_TOKEN must fail startup and never fall
    // back to anonymous. We feed a valid initialize request; the process should
    // exit non-zero without ever answering it.
    let temp = tempfile::tempdir().expect("temp HOME");
    let mut child = Command::new(envelope_bin())
        .arg("mcp")
        .env("HOME", temp.path())
        .env("ENVELOPE_HOME", temp.path())
        .env(
            "ENVELOPE_AGENT_TOKEN",
            "envtok_deadbeefdeadbeefdeadbeefdeadbeef",
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcp");
    // Close stdin immediately; startup resolution happens before the read loop.
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("wait mcp");
    assert!(
        !out.status.success(),
        "unknown agent token must fail MCP startup"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ENVELOPE_AGENT_TOKEN") && stderr.contains("refusing to start"),
        "startup error must name the env var and refuse; got: {stderr}"
    );
}

#[test]
fn mcp_startup_fails_loud_on_revoked_token() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    let (token, _id) = create_agent(home, "skippy");
    let revoked = run_cli(home, &["agent", "revoke", "skippy"], None);
    assert!(revoked.status.success());

    let mut child = Command::new(envelope_bin())
        .arg("mcp")
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
        .env("ENVELOPE_AGENT_TOKEN", &token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcp");
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("wait mcp");
    assert!(
        !out.status.success(),
        "revoked agent token must fail MCP startup"
    );
}

#[test]
fn mcp_restrictive_policy_denies_tool_with_stable_code() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);
    let (token, _id) = create_agent(home, "skippy");
    // Only inbox.read is allowed; a send tool call must be denied before dispatch.
    set_policy(home, "skippy", "inbox.read", "draft-only");

    let (payload, is_error) = tool_call(
        home,
        Some(&token),
        "send",
        json!({ "to": "a@b.test", "subject": "hi", "body": "x" }),
    );
    assert!(is_error, "denied tool must be reported as an MCP error");
    assert_eq!(payload["code"], "agent_policy_denied_action");
    // No recipient address may leak into the denial.
    assert!(!payload.to_string().contains("a@b.test"));
}

#[test]
fn mcp_allowed_send_clamps_to_ceiling_and_attributes_agent() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);
    let (token, agent_id) = create_agent(home, "skippy");
    // send allowed, but ceiling is draft-only: an autonomous request must clamp.
    set_policy(home, "skippy", "send", "draft-only");

    let (payload, is_error) = tool_call(
        home,
        Some(&token),
        "send",
        json!({
            "to": "a@b.test",
            "subject": "hi",
            "body": "x",
            "attributes": ["informational"],
            "send_mode": "autonomous-send"
        }),
    );
    assert!(!is_error, "allowed send must pass authorization: {payload}");
    // Clamped down: draft-only ceiling forces a draft even for an autonomous request.
    assert_eq!(payload["status"], "drafted");
    assert_eq!(payload["send_mode"], "draft-only");
    assert_eq!(payload["sent"], false);

    // The send-policy audit event is attributed to the acting agent id.
    let db = envelope_email_store::Database::open(&db_path(home)).expect("open db");
    let attributed: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE agent_id = ?1 AND event_type LIKE 'send_policy.%'",
            [&agent_id],
            |row| row.get(0),
        )
        .expect("count attributed events");
    assert!(
        attributed >= 1,
        "the mutating send must record a send-policy event attributed to the agent"
    );
}

#[test]
fn mcp_anonymous_send_default_mode_is_draft_only() {
    // With no ENVELOPE_AGENT_TOKEN the MCP send tool defaults send_mode to
    // draft-only. A valid `attributes` declaration is required (v2), and the
    // outcome is a draft — no policy applies.
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);

    let (payload, is_error) = tool_call(
        home,
        None,
        "send",
        json!({ "to": "a@b.test", "subject": "hi", "body": "x", "attributes": ["informational"] }),
    );
    assert!(!is_error, "anonymous send must not be denied: {payload}");
    assert_eq!(payload["status"], "drafted");
    assert_eq!(payload["send_mode"], "draft-only");
}

#[test]
fn mcp_draft_only_send_without_attributes_is_attributes_required() {
    // v2: the mandatory-attributes rule is enforced at the handler boundary even
    // when the policy outcome would be draft-only — a missing declaration returns
    // structured attributes_required, not a silently-created draft.
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);

    let (payload, is_error) = tool_call(
        home,
        None,
        "send",
        json!({ "to": "a@b.test", "subject": "hi", "body": "x" }),
    );
    assert!(is_error, "missing attributes must be refused: {payload}");
    assert_eq!(payload["error"]["code"], "attributes_required");
    // Nothing was created — the draft-only path never ran.
    let db = envelope_email_store::Database::open(&db_path(home)).expect("open db");
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM drafts", [], |r| r.get(0))
        .expect("count drafts");
    assert_eq!(count, 0, "no draft created when attributes are missing");
}

#[test]
fn mcp_send_draft_under_draft_only_ceiling_never_sends() {
    // Regression: send_draft dispatched straight to the shared send primitive
    // (Governor gate only), bypassing the per-agent send-mode ceiling. An agent
    // with a draft-only ceiling — even with the `send` action allowed and all
    // three confirmation flags set — must never reach SMTP.
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);
    let (token, agent_id) = create_agent(home, "skippy");
    set_policy(home, "skippy", "send", "draft-only");

    let draft_id = create_local_draft(home, "a@b.test");

    let (payload, is_error) = tool_call(
        home,
        Some(&token),
        "send_draft",
        json!({
            "draft_id": draft_id,
            "attributes": ["informational"],
            "confirm_send": true,
            "send_now": true,
            "confirm_send_now": true
        }),
    );

    // Ceiling wins over every confirmation flag: no SMTP path is reached.
    assert!(
        !is_error,
        "ceiling block is a non-sent drafted outcome, not an error: {payload}"
    );
    assert_eq!(payload["status"], "drafted");
    assert_eq!(payload["send_mode"], "draft-only");
    assert_eq!(payload["sent"], false);
    assert_eq!(payload["draft_id"], draft_id);

    // The draft still exists (it was never consumed by a send).
    let out = run_cli(home, &["draft", "show", &draft_id, "--json"], Some(&token));
    assert!(
        out.status.success(),
        "draft must still exist after blocked send"
    );

    // The ceiling decision is recorded as a send-policy event attributed to the agent.
    let db = envelope_email_store::Database::open(&db_path(home)).expect("open db");
    let attributed: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE agent_id = ?1 AND event_type LIKE 'send_policy.%'",
            [&agent_id],
            |row| row.get(0),
        )
        .expect("count attributed events");
    assert!(
        attributed >= 1,
        "the blocked send_draft must record a send-policy event attributed to the agent"
    );
}

#[test]
fn mcp_send_draft_confirm_send_ceiling_passes_ceiling_check() {
    // A confirm-send ceiling (with confirm_send=true) must NOT be blocked by the
    // ceiling logic itself: the send clears the ceiling and proceeds to the
    // normal dispatch (here it queues into the outbox). It must not return the
    // ceiling-denial (status=drafted / send_mode=draft-only). A valid `attributes`
    // declaration is supplied (v2 requires it) and the default cooldown queue path
    // keeps this deterministic — no Governor spawn, no SMTP.
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);
    let (token, _agent_id) = create_agent(home, "skippy");
    set_policy(home, "skippy", "send", "confirm-send");

    let draft_id = create_local_draft(home, "a@b.test");

    let (payload, _is_error) = tool_call(
        home,
        Some(&token),
        "send_draft",
        json!({
            "draft_id": draft_id,
            "attributes": ["informational"],
            "confirm_send": true
        }),
    );

    // The ceiling check passed: the outcome is NOT the ceiling-denial shape.
    assert_ne!(
        payload["status"], "drafted",
        "confirm-send ceiling must not be forced to a draft: {payload}"
    );
    assert_ne!(
        payload["send_mode"], "draft-only",
        "confirm-send ceiling must not clamp to draft-only: {payload}"
    );
}

// ── Wave 3 tools: bulk / thread / rules / watch / snooze ────────────

/// Set a policy with an explicit allow-actions list (comma-separated).
fn set_policy_actions(home: &std::path::Path, name: &str, actions: &str) {
    set_policy(home, name, actions, "draft-only");
}

#[test]
fn mcp_bulk_denied_when_policy_lacks_underlying_action() {
    // The bulk tool requires BOTH the coarse `bulk` action AND the underlying
    // single action. An agent with `bulk` but not `delete` must be denied a bulk
    // delete before any IMAP work, with a stable denial code.
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);
    let (token, _id) = create_agent(home, "skippy");
    set_policy_actions(home, "skippy", "bulk"); // no `delete`

    let (payload, is_error) = tool_call(
        home,
        Some(&token),
        "bulk",
        json!({ "op": "delete", "uids": [1, 2], "folder": "INBOX", "confirm": true }),
    );
    assert!(is_error, "bulk missing underlying action must be denied");
    assert_eq!(payload["code"], "agent_policy_denied_action");
}

#[test]
fn mcp_bulk_allowed_with_both_actions_reaches_execution() {
    // With both `bulk` and `delete` allowed, the two-action gate passes; the call
    // proceeds past authorization (and then fails at the offline IMAP connect —
    // proving it cleared policy rather than being denied).
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);
    let (token, _id) = create_agent(home, "skippy");
    set_policy_actions(home, "skippy", "bulk,delete");

    let (payload, _is_error) = tool_call(
        home,
        Some(&token),
        "bulk",
        json!({ "op": "delete", "uids": [1], "folder": "INBOX", "confirm": true }),
    );
    // It must NOT be a policy denial (it cleared the two-action gate).
    assert_ne!(
        payload["code"], "agent_policy_denied_action",
        "bulk with both actions must clear the gate: {payload}"
    );
}

#[test]
fn mcp_rules_run_default_dry_run_authorizes_under_rules_read() {
    // rules_run defaults dry_run=true and must authorize under rules.read. An
    // agent holding only rules.read must NOT be denied (it clears policy, then
    // fails at the offline IMAP connect).
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);
    let (token, _id) = create_agent(home, "skippy");
    set_policy_actions(home, "skippy", "rules.read");

    let (payload, _is_error) = tool_call(home, Some(&token), "rules_run", json!({}));
    assert_ne!(
        payload["code"], "agent_policy_denied_action",
        "default dry-run rules_run must authorize under rules.read: {payload}"
    );
}

#[test]
fn mcp_rules_run_real_run_requires_rules_run_action() {
    // A real run (dry_run:false) escalates to the rules.run action. An agent with
    // only rules.read must be denied.
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);
    let (token, _id) = create_agent(home, "skippy");
    set_policy_actions(home, "skippy", "rules.read");

    let (payload, is_error) =
        tool_call(home, Some(&token), "rules_run", json!({ "dry_run": false }));
    assert!(is_error, "real rules_run without rules.run must be denied");
    assert_eq!(payload["code"], "agent_policy_denied_action");
}

#[test]
fn mcp_watch_status_happy_path_returns_delivery_counts() {
    // watch_status is read-only (no IMAP): with the watch.read action it returns
    // a structured summary with delivery counts even on an empty DB.
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);
    let (token, _id) = create_agent(home, "skippy");
    set_policy_actions(home, "skippy", "watch.read");

    let (payload, is_error) = tool_call(home, Some(&token), "watch_status", json!({}));
    assert!(
        !is_error,
        "watch_status happy path must not error: {payload}"
    );
    assert!(payload["watches"].is_array(), "watches array: {payload}");
    assert!(
        payload["deliveries"]["delivered"].is_number(),
        "delivery counts present: {payload}"
    );
    assert!(payload["deliveries"]["dead_letter"].is_number());
}

#[test]
fn mcp_snooze_list_happy_path_returns_array() {
    // snooze list is read-only (no IMAP): with the snooze action it returns the
    // (empty) snoozed list without denial or error.
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);
    let (token, _id) = create_agent(home, "skippy");
    set_policy_actions(home, "skippy", "snooze");

    let (payload, is_error) = tool_call(home, Some(&token), "snooze", json!({ "action": "list" }));
    assert!(
        !is_error,
        "snooze list happy path must not error: {payload}"
    );
    assert!(
        payload.is_array(),
        "snooze list must be an array: {payload}"
    );
}

#[test]
fn mcp_thread_list_happy_path_returns_wrapped_array() {
    // thread list is DB-only (no IMAP). With inbox.read it returns the untrusted
    // trust envelope wrapping a (possibly empty) thread array.
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);
    let (token, _id) = create_agent(home, "skippy");
    set_policy_actions(home, "skippy", "inbox.read");

    let (payload, is_error) = tool_call(home, Some(&token), "thread", json!({}));
    assert!(
        !is_error,
        "thread list happy path must not error: {payload}"
    );
    assert_eq!(payload["_envelope_trust"], "untrusted-content");
    assert!(
        payload["content"].is_array(),
        "wrapped thread list must be an array under content: {payload}"
    );
}

#[test]
fn contract_export_declares_wave3_tools_and_gates() {
    // The contract export must additively declare the 5 new tools, the bulk
    // two-action gate, the delete-confirm gate, and the revoked-token note (F4).
    let temp = tempfile::tempdir().expect("temp HOME");
    let output = Command::new(envelope_bin())
        .arg("contract")
        .env("HOME", temp.path())
        .env("ENVELOPE_HOME", temp.path())
        .output()
        .expect("run contract");
    assert!(output.status.success());
    let contract: Value = serde_json::from_slice(&output.stdout).expect("contract JSON");
    assert_eq!(contract["schema"], "envelope.agent_contract.v2");

    let map = &contract["agent_identity"]["tool_action_map"];
    assert_eq!(map["bulk"], "bulk");
    assert_eq!(map["thread"], "inbox.read");
    assert_eq!(map["rules_preview"], "rules.read");
    assert_eq!(map["rules_run"], "rules.run");
    assert_eq!(map["watch_status"], "watch.read");
    assert_eq!(map["snooze"], "snooze");

    let ai = &contract["agent_identity"];
    assert!(ai["bulk_two_action_gate"].is_string());
    assert!(
        ai["bulk_delete_confirmation"]
            .as_str()
            .unwrap_or_default()
            .contains("confirm")
    );
    assert!(
        ai["rules_run_dry_run_default"]
            .as_str()
            .unwrap_or_default()
            .contains("dry_run")
    );
    assert!(
        ai["revoked_token_session_persistence"]
            .as_str()
            .unwrap_or_default()
            .contains("next session start"),
        "F4 revoked-token note must document next-session-start semantics"
    );

    // All 5 new tools appear in the mcp_tools list too.
    let tools = contract["mcp_tools"].as_array().expect("mcp_tools array");
    for name in [
        "bulk",
        "thread",
        "rules_preview",
        "rules_run",
        "watch_status",
        "snooze",
    ] {
        assert!(
            tools.iter().any(|t| t["name"] == name),
            "mcp_tools must declare {name}"
        );
    }
}

// ── Attribution protocol journeys (envelope.attribution.v1) ──────────────
//
// These never send real email or spawn Governor: an unattributed/invalid
// request is refused before any side effect, and an attributed request reaches
// the local outbox queue (no SMTP). Anonymous MCP authorizes every tool; an
// autonomous-send request reaches the actual-send attribution precheck.

const RISK_KEYS: [&str; 5] = [
    "financial_content",
    "legal_content",
    "commitment_language",
    "has_pii",
    "uncited_claims",
];

#[test]
fn mcp_send_without_attributes_is_attributes_required_then_recovers() {
    let temp = tempfile::tempdir().expect("temp HOME");
    seed_account(temp.path());

    // Attempt 1: autonomous send, no attributes -> attributes_required.
    let (resp, is_error) = tool_call(
        temp.path(),
        None,
        "send",
        json!({
            "to": "stranger@acme.example",
            "subject": "Hello",
            "body": "hi there",
            "send_mode": "autonomous-send"
        }),
    );
    assert!(is_error, "unattributed send must be an error: {resp}");
    assert_eq!(resp["status"], "invalid");
    assert_eq!(resp["error"]["code"], "attributes_required");

    // Governor was never spawned (this is not governor_unavailable).
    let reason = resp["error"]["reason"].as_str().expect("reason string");
    assert!(
        reason.contains("attributes"),
        "names the parameter: {reason}"
    );
    assert!(
        reason.contains("governor_catalog"),
        "names the catalog: {reason}"
    );
    assert!(
        reason.contains("financial_content") || reason.contains("informational"),
        "names a concrete key: {reason}"
    );

    // The error carries a self-contained `--help`-quality `help` object across the
    // real MCP boundary: definition, both declaration syntaxes, contextual
    // examples, and every catalog-discovery pointer.
    let help = &resp["error"]["help"];
    assert!(
        help["what_are_attributes"].as_str().unwrap_or("").len() > 60,
        "plain-language definition of attributes"
    );
    assert!(
        help["syntax"]["cli"]
            .as_str()
            .unwrap_or("")
            .contains("--attr"),
        "CLI declaration syntax"
    );
    assert_eq!(
        help["syntax"]["mcp"]["field"], "attributes",
        "MCP field name"
    );
    assert_eq!(help["list_attributes"]["mcp_tool"], "governor_catalog");
    assert_eq!(
        help["list_attributes"]["cli"],
        "envelope governor catalog --json"
    );
    assert_eq!(
        help["list_attributes"]["skill"],
        "envelope-governor-attribution"
    );
    assert!(
        help["rules"]
            .as_array()
            .map(|r| r.len() >= 3)
            .unwrap_or(false)
    );

    // Examples: >=3 contextual suggestions, each with key/description/when, and at
    // least one risk key.
    let examples = help["examples"].as_array().expect("help.examples");
    assert!(examples.len() >= 3, "at least three contextual examples");
    for ex in examples {
        assert!(ex["key"].is_string());
        assert!(ex["description"].is_string());
        assert!(ex["when"].is_string());
    }
    assert!(
        examples
            .iter()
            .any(|s| RISK_KEYS.contains(&s["key"].as_str().unwrap_or(""))),
        "at least one risk key"
    );

    // The declared/rejected INPUT sets are echoed under error.attributes.
    assert!(resp["error"]["attributes"]["declared"].is_array());
    assert!(resp["error"]["attributes"]["rejected"].is_array());

    // No score/weight/threshold leaks anywhere in the payload.
    let whole = serde_json::to_string(&resp).unwrap();
    for banned in ["\"score\"", "weight", "threshold"] {
        assert!(!whole.contains(banned), "payload leaked {banned}");
    }

    // Attempt 2: declare a true fact -> reaches the outbox queue (no SMTP).
    let (resp2, is_error2) = tool_call(
        temp.path(),
        None,
        "send",
        json!({
            "to": "stranger@acme.example",
            "subject": "Hello",
            "body": "hi there",
            "attributes": ["informational"],
            "send_mode": "autonomous-send"
        }),
    );
    assert!(!is_error2, "attributed send should proceed: {resp2}");
    assert_eq!(resp2["status"], "queued");
    assert!(resp2["draft_id"].is_string());

    // Gap 3: a SUCCESSFUL (queued) result carries the additive `attribution`
    // block. The real Governor decision runs later at the sweep, so governor is
    // null and governor_decision_pending marks the deferral. No score ever.
    let attribution = &resp2["attribution"];
    assert_eq!(attribution["attribution_state"], "attributed");
    assert_eq!(attribution["protocol"], "envelope.attribution.v1");
    assert_eq!(attribution["catalog"], "envelope");
    assert!(
        attribution["declared_attrs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "informational")
    );
    assert!(attribution["governor_attrs"].is_array());
    assert!(attribution.get("accepted_redundant").is_some());
    assert!(attribution.get("rejected_attrs").is_some());
    assert_eq!(
        attribution["governor"],
        Value::Null,
        "verdict deferred to sweep"
    );
    assert!(attribution.get("governor_decision_pending").is_some());
    assert!(
        !serde_json::to_string(attribution)
            .unwrap()
            .contains("\"score\"")
    );
}

#[test]
fn mcp_stateless_attribution_failure_never_claims_a_draft_was_parked() {
    // Gap 2: a direct/stateless send that fails attribution created no draft, so
    // its response must be idempotent to retry and must NOT claim any parking.
    let temp = tempfile::tempdir().expect("temp HOME");
    seed_account(temp.path());

    let (resp, is_error) = tool_call(
        temp.path(),
        None,
        "send",
        json!({
            "to": "stranger@acme.example",
            "subject": "Hello",
            "body": "hi there",
            "send_mode": "autonomous-send"
        }),
    );
    assert!(is_error);
    assert_eq!(resp["error"]["code"], "attributes_required");
    // Idempotent, and honest: no draft, nothing sent, nothing parked.
    assert_eq!(resp["error"]["recovery"]["retry"]["idempotent"], true);
    let note = resp["error"]["recovery"]["retry"]["note"]
        .as_str()
        .unwrap_or_default();
    assert!(
        note.contains("nothing was sent or created"),
        "note must affirm nothing was created: {note}"
    );
    let whole = serde_json::to_string(&resp).unwrap();
    assert!(
        !whole.contains("attribution_exhausted") && !whole.contains("pending_review"),
        "a stateless failure must not claim a draft was parked: {whole}"
    );
}

#[cfg(unix)]
#[test]
fn mcp_stateless_immediate_review_never_claims_a_draft_was_parked() {
    // Block 4: a direct/stateless immediate send (send_now + confirm_send_now,
    // no draft) that Governor routes to REVIEW created and parked nothing. Its
    // recovery must say so and must NOT link a nonexistent pending_review draft.
    let temp = tempfile::tempdir().expect("temp HOME");
    seed_account(temp.path());
    let gov = write_mock_governor(temp.path(), "review");

    let (resp, is_error) = tool_call_env(
        temp.path(),
        None,
        "send",
        json!({
            "to": "stranger@acme.example",
            "subject": "Hello",
            "body": "hi there",
            "attributes": ["informational"],
            "send_mode": "autonomous-send",
            "send_now": true,
            "confirm_send_now": true
        }),
        &[
            ("ENVELOPE_GOVERNOR_MODE", "required"),
            ("ENVELOPE_GOVERNOR_BIN", gov.to_str().unwrap()),
        ],
    );
    assert!(is_error, "a review verdict blocks the send: {resp}");
    assert_eq!(resp["status"], "blocked");
    assert_eq!(resp["error"]["route"], "review");
    let reason = resp["error"]["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("Nothing was sent, created, or parked"),
        "stateless review must be honest about parking: {reason}"
    );
    let whole = serde_json::to_string(&resp).unwrap();
    assert!(
        !whole.contains("pending_review"),
        "must not link a nonexistent parked draft: {whole}"
    );
    // Nothing was created: no draft rows exist.
    let db = envelope_email_store::Database::open(&db_path(temp.path())).expect("open db");
    let draft_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM drafts", [], |r| r.get(0))
        .expect("count drafts");
    assert_eq!(draft_count, 0, "a stateless review must create no draft");
}

#[cfg(unix)]
#[test]
fn mcp_draft_backed_immediate_review_does_not_falsely_park() {
    // Block 4: a draft-backed immediate send (send_draft) that Governor reviews
    // releases the draft back to `draft` (the claim guard), it is NOT parked. The
    // recovery must not falsely claim a pending_review park.
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);
    let (token, _agent_id) = create_agent(home, "skippy");
    set_policy(home, "skippy", "send", "autonomous-send");
    let draft_id = create_local_draft(home, "stranger@acme.example");
    let gov = write_mock_governor(home, "review");

    let (resp, is_error) = tool_call_env(
        home,
        Some(&token),
        "send_draft",
        json!({
            "draft_id": draft_id,
            "confirm_send": true,
            "send_now": true,
            "confirm_send_now": true,
            "attributes": ["informational"]
        }),
        &[
            ("ENVELOPE_GOVERNOR_MODE", "required"),
            ("ENVELOPE_GOVERNOR_BIN", gov.to_str().unwrap()),
        ],
    );
    assert!(is_error, "review blocks the send: {resp}");
    assert_eq!(resp["error"]["route"], "review");
    let whole = serde_json::to_string(&resp).unwrap();
    assert!(
        !whole.contains("pending_review"),
        "an unparked draft-backed review must not claim a pending_review park: {whole}"
    );
    // The draft is back at `draft` status (released, not parked/consumed).
    let db = envelope_email_store::Database::open(&db_path(home)).expect("open db");
    let status: String = db
        .conn()
        .query_row(
            "SELECT status FROM drafts WHERE id = ?1",
            [&draft_id],
            |r| r.get(0),
        )
        .expect("draft status");
    assert_eq!(
        status, "draft",
        "reviewed draft returns to draft, not parked"
    );
}

#[test]
fn mcp_send_typo_is_attributes_invalid_then_corrected() {
    let temp = tempfile::tempdir().expect("temp HOME");
    seed_account(temp.path());

    let (resp, is_error) = tool_call(
        temp.path(),
        None,
        "send",
        json!({
            "to": "stranger@acme.example",
            "subject": "Hello",
            "body": "hi",
            "attributes": ["informationl"],
            "send_mode": "autonomous-send"
        }),
    );
    assert!(is_error);
    assert_eq!(resp["error"]["code"], "attributes_invalid");
    // Rejected keys + per-key reason + nearest suggestion live in the obvious
    // error.attributes.rejected structure.
    let rejected = resp["error"]["attributes"]["rejected"]
        .as_array()
        .expect("attributes.rejected");
    assert!(rejected.iter().any(|r| {
        r["key"] == "informationl"
            && r["code"] == "unknown_attribute"
            && r["did_you_mean"]
                .as_array()
                .map(|d| d.iter().any(|k| k == "informational"))
                .unwrap_or(false)
    }));
    // The caller's declared input is echoed, and the same help affordances apply.
    assert!(
        resp["error"]["attributes"]["declared"]
            .as_array()
            .map(|d| d.iter().any(|k| k == "informationl"))
            .unwrap_or(false)
    );
    assert!(
        !resp["error"]["help"]["examples"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Corrected retry queues.
    let (resp2, is_error2) = tool_call(
        temp.path(),
        None,
        "send",
        json!({
            "to": "stranger@acme.example",
            "subject": "Hello",
            "body": "hi",
            "attributes": ["informational"],
            "send_mode": "autonomous-send"
        }),
    );
    assert!(!is_error2, "corrected retry should queue: {resp2}");
    assert_eq!(resp2["status"], "queued");
}

#[test]
fn mcp_self_asserted_tyler_approved_is_attestation_required() {
    let temp = tempfile::tempdir().expect("temp HOME");
    seed_account(temp.path());

    let (resp, is_error) = tool_call(
        temp.path(),
        None,
        "send",
        json!({
            "to": "stranger@acme.example",
            "subject": "Hello",
            "body": "hi",
            "attributes": ["tyler_approved", "informational"],
            "send_mode": "autonomous-send"
        }),
    );
    assert!(is_error);
    assert_eq!(resp["error"]["code"], "attributes_invalid");
    let rejected = resp["error"]["attributes"]["rejected"].as_array().unwrap();
    assert!(
        rejected
            .iter()
            .any(|r| r["key"] == "tyler_approved" && r["code"] == "attestation_required"),
        "self-asserted attestation must be rejected: {resp}"
    );
    // The attestation key never reaches Governor: the whole request is invalid and
    // Governor is never spawned, so no Governor decision block is emitted.
    assert_eq!(resp["status"], "invalid");
    assert!(
        resp["error"].get("governor").is_none(),
        "attestation must never reach Governor: {resp}"
    );
}

#[test]
fn mcp_governor_catalog_is_always_allowed_and_weight_free() {
    let temp = tempfile::tempdir().expect("temp HOME");
    seed_account(temp.path());

    // A restricted agent (no send, read-only) can still discover the catalog.
    let (token, _id) = create_agent(temp.path(), "reader");
    set_policy(temp.path(), "reader", "inbox.read", "draft-only");

    let (resp, is_error) = tool_call(temp.path(), Some(&token), "governor_catalog", json!({}));
    assert!(!is_error, "governor_catalog must be always allowed: {resp}");
    assert_eq!(resp["protocol"], "envelope.attribution.v1");
    assert_eq!(resp["catalog_version"], 1);
    assert_eq!(resp["attributes"].as_array().unwrap().len(), 30);

    // No weights/scores anywhere; provenance present; tyler_approved is attestation.
    let text = serde_json::to_string(&resp).unwrap();
    assert!(!text.contains("weight"));
    assert!(!text.contains("\"score\""));
    assert!(!text.contains("threshold"));
    let attrs = resp["attributes"].as_array().unwrap();
    assert!(attrs.iter().any(|a| a["provenance"] == "declarable"));
    assert!(
        attrs
            .iter()
            .any(|a| a["key"] == "tyler_approved" && a["provenance"] == "requires_attestation")
    );
}

#[test]
fn mcp_send_schema_attributes_enum_excludes_attestation_keys() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let mut child = spawn_mcp(temp.path());
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_framed(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}),
    );
    let _ = read_framed(&mut stdout);
    write_framed(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    let tools = read_framed(&mut stdout);
    drop(stdin);
    child.wait().expect("wait mcp");

    let entries = tools["result"]["tools"].as_array().unwrap();
    let send = entries.iter().find(|t| t["name"] == "send").unwrap();
    let enum_vals = send["inputSchema"]["properties"]["attributes"]["items"]["enum"]
        .as_array()
        .expect("attributes enum");
    assert!(
        enum_vals.iter().any(|k| k == "financial_content"),
        "declarable keys present"
    );
    assert!(
        !enum_vals.iter().any(|k| k == "tyler_approved"),
        "attestation keys must be unrepresentable in the schema enum"
    );
    // The attributes schema itself carries no weight/score data.
    let attrs_schema =
        serde_json::to_string(&send["inputSchema"]["properties"]["attributes"]).unwrap();
    assert!(!attrs_schema.contains("\"score\""));
    assert!(!attrs_schema.contains("weight"));
}

#[test]
fn cli_send_without_attr_is_canonical_error_with_no_side_effect() {
    let temp = tempfile::tempdir().expect("temp HOME");
    seed_account(temp.path());

    // No --attr: attributes_required, nonzero exit, NO draft/SMTP/Governor side effect.
    let out = run_cli(
        temp.path(),
        &[
            "--json",
            "send",
            "--to",
            "stranger@acme.example",
            "--subject",
            "Hi",
            "--body",
            "hi",
        ],
        None,
    );
    assert!(
        !out.status.success(),
        "unattributed CLI send must exit nonzero"
    );
    let v: Value = serde_json::from_slice(&out.stdout).expect("json on stdout");
    assert_eq!(v["status"], "invalid");
    assert_eq!(v["error"]["code"], "attributes_required");
    assert!(!serde_json::to_string(&v).unwrap().contains("\"score\""));

    // No draft row was created (no side effect before the refusal).
    let db = envelope_email_store::Database::open(&db_path(temp.path())).expect("open db");
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM drafts", [], |r| r.get(0))
        .expect("count drafts");
    assert_eq!(
        count, 0,
        "no draft may be created on an attribution refusal"
    );

    // Retry with --attr: queues into the outbox (creates a draft, no SMTP).
    let out2 = run_cli(
        temp.path(),
        &[
            "--json",
            "send",
            "--to",
            "stranger@acme.example",
            "--subject",
            "Hi",
            "--body",
            "hi",
            "--attr",
            "informational",
        ],
        None,
    );
    assert!(
        out2.status.success(),
        "attributed CLI send should queue: {}",
        String::from_utf8_lossy(&out2.stdout)
    );
    let v2: Value = serde_json::from_slice(&out2.stdout).expect("json");
    assert_eq!(v2["status"], "queued");
}

#[test]
fn cli_send_without_attr_fails_closed_in_warn_mode_too() {
    // Warn mode softens only a Governor VERDICT; it never waives the attribution
    // precondition. An unattributed CLI send under ENVELOPE_GOVERNOR_MODE=warn
    // must still be refused with attributes_required and create no draft — the
    // exact "warn must not send unattributed bot mail" invariant.
    let temp = tempfile::tempdir().expect("temp HOME");
    seed_account(temp.path());

    let out = Command::new(envelope_bin())
        .args([
            "--json",
            "send",
            "--to",
            "stranger@acme.example",
            "--subject",
            "Hi",
            "--body",
            "hi",
        ])
        .env("HOME", temp.path())
        .env("ENVELOPE_HOME", temp.path())
        .env("ENVELOPE_GOVERNOR_MODE", "warn")
        .output()
        .expect("run envelope cli");

    assert!(
        !out.status.success(),
        "warn mode must still fail closed on a missing declaration: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let v: Value = serde_json::from_slice(&out.stdout).expect("json on stdout");
    assert_eq!(v["status"], "invalid");
    assert_eq!(v["error"]["code"], "attributes_required");

    // No draft may be created — warn must not silently queue an unattributed send.
    let db = envelope_email_store::Database::open(&db_path(temp.path())).expect("open db");
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM drafts", [], |r| r.get(0))
        .expect("count drafts");
    assert_eq!(
        count, 0,
        "warn must not create a draft for an unattributed send"
    );
}

#[test]
fn cli_governor_catalog_prints_weight_free_projection() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let out = run_cli(temp.path(), &["--json", "governor", "catalog"], None);
    assert!(out.status.success(), "governor catalog --json must succeed");
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["protocol"], "envelope.attribution.v1");
    assert_eq!(v["attributes"].as_array().unwrap().len(), 30);
    let text = serde_json::to_string(&v).unwrap();
    assert!(!text.contains("weight"));
    assert!(!text.contains("\"score\""));
}

// ── Contract parity: REAL handler output vs the PUBLISHED output schema ──
//
// The strongest anti-drift check: capture a real MCP `send` response and
// validate it against the output schema the running binary publishes via
// `envelope contract send`. Both sides are the real binary, so the schema can
// never silently diverge from what the handler actually emits.

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn schema_type_admits(schema_type: &Value, actual: &str) -> bool {
    match schema_type {
        Value::String(s) => s == actual || (s == "number" && actual == "integer"),
        Value::Array(types) => types.iter().any(|t| schema_type_admits(t, actual)),
        _ => true,
    }
}

/// Every key of `value` must be declared in the schema (contract objects are
/// additionalProperties:false), and each present value's JSON type admitted by
/// the property `type`. Returns violations (empty = conforms).
fn schema_conformance_violations(schema: &Value, value: &Value) -> Vec<String> {
    let props = &schema["properties"];
    let mut errs = Vec::new();
    for (k, v) in value.as_object().expect("response must be a JSON object") {
        match props.get(k) {
            None => errs.push(format!("undeclared key `{k}`")),
            Some(prop) => {
                let actual = json_type_name(v);
                if prop.get("type").is_some() && !schema_type_admits(&prop["type"], actual) {
                    errs.push(format!(
                        "key `{k}` is {actual} but schema type is {}",
                        prop["type"]
                    ));
                }
            }
        }
    }
    errs
}

fn published_output_schema(home: &std::path::Path, surface: &str) -> Value {
    let out = run_cli(home, &["contract", "--surface", surface], None);
    assert!(
        out.status.success(),
        "envelope contract --surface {surface} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).expect("contract surface JSON");
    v["output_schema"].clone()
}

#[test]
fn real_send_responses_conform_to_published_contract_schema() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);
    let (token, _agent_id) = create_agent(home, "skippy");
    // Autonomous ceiling so an attributed autonomous send reaches the outbox
    // queue (rather than being clamped to a draft).
    set_policy(home, "skippy", "send", "autonomous-send");

    let send_schema = published_output_schema(home, "send");

    // Real queued acceptance — the full success envelope, straight from the handler.
    let (queued, is_error) = tool_call(
        home,
        Some(&token),
        "send",
        json!({
            "to": "stranger@acme.example",
            "subject": "Hello",
            "body": "hi there",
            "attributes": ["informational"],
            "send_mode": "autonomous-send"
        }),
    );
    assert!(!is_error, "attributed send should proceed: {queued}");
    assert_eq!(queued["status"], "queued", "{queued}");
    let errs = schema_conformance_violations(&send_schema, &queued);
    assert!(
        errs.is_empty(),
        "real queued `send` response drifted from the published schema: {errs:?}\n{queued}"
    );

    // Real attribution refusal — the `{status, error}` envelope, straight from
    // the handler (unknown key → attributes_invalid).
    let (refusal, _is_error) = tool_call(
        home,
        Some(&token),
        "send",
        json!({
            "to": "stranger@acme.example",
            "subject": "Hello",
            "body": "hi there",
            "attributes": ["not_a_real_key"],
            "send_mode": "autonomous-send"
        }),
    );
    assert_eq!(refusal["status"], "invalid", "{refusal}");
    let errs = schema_conformance_violations(&send_schema, &refusal);
    assert!(
        errs.is_empty(),
        "real refusal `send` response drifted from the published schema: {errs:?}\n{refusal}"
    );
}
