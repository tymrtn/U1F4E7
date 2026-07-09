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
    cmd.args(args).env("HOME", home);
    if let Some(t) = token {
        cmd.env("ENVELOPE_AGENT_TOKEN", t);
    }
    cmd.output().expect("run envelope cli")
}

/// Seed one offline account so send/reply draft paths can resolve credentials
/// without any network. Uses the insecure machine key (test-only).
fn seed_account(home: &std::path::Path) {
    let out = run_cli(
        home,
        &[
            "accounts",
            "add",
            "--email",
            "test@example.test",
            "--password",
            "pw",
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

/// Create a local draft via the CLI (offline: IMAP sync fails, draft is saved
/// locally) and return its id. Used to feed send_draft without any network.
fn create_local_draft(home: &std::path::Path, to: &str) -> String {
    let out = run_cli(
        home,
        &[
            "draft",
            "create",
            "--to",
            to,
            "--subject",
            "hi",
            "--body",
            "x",
            "--json",
        ],
        None,
    );
    assert!(out.status.success(), "draft create failed");
    let v: Value = serde_json::from_slice(&out.stdout).expect("draft create JSON");
    v["id"].as_str().expect("draft id").to_string()
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
    cmd.arg("mcp").env("HOME", home);
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

fn db_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join("Library/Application Support/envelope-email/envelope.db")
}

fn spawn_mcp(home: &std::path::Path) -> Child {
    Command::new(envelope_bin())
        .arg("mcp")
        .env("HOME", home)
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
    assert_eq!(tool_entries.len(), 16);
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
        .output()
        .expect("run contract");
    assert!(output.status.success());
    let contract: Value = serde_json::from_slice(&output.stdout).expect("contract JSON");

    // Contract stays v1 (additive change only).
    assert_eq!(contract["schema"], "envelope.agent_contract.v1");

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
fn mcp_anonymous_send_defaults_unchanged() {
    // With no ENVELOPE_AGENT_TOKEN the MCP send tool behaves exactly as before:
    // agent default is draft-only, so the result is a draft and no policy applies.
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    seed_account(home);

    let (payload, is_error) = tool_call(
        home,
        None,
        "send",
        json!({ "to": "a@b.test", "subject": "hi", "body": "x" }),
    );
    assert!(!is_error, "anonymous send must not be denied: {payload}");
    assert_eq!(payload["status"], "drafted");
    assert_eq!(payload["send_mode"], "draft-only");
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
    // normal Governor-gated dispatch. It may still be gated downstream, but it
    // must not return the ceiling-denial (status=drafted / send_mode=draft-only).
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
            "confirm_send": true,
            "send_now": true,
            "confirm_send_now": true
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
