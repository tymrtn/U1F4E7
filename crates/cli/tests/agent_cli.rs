// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! CLI integration tests for the `envelope agent` command group and the
//! per-agent identity contract. Each test runs the built binary against an
//! isolated `HOME` so no real mailbox or DB is touched.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

fn envelope_bin() -> &'static str {
    env!("CARGO_BIN_EXE_envelope")
}

fn run(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(envelope_bin())
        .args(args)
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
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

/// Spec framing (MCP stdio): one compact JSON-RPC object per line, `\n`-terminated.
fn write_line(stdin: &mut ChildStdin, value: &Value) {
    let mut body = serde_json::to_vec(value).expect("serialize request");
    body.push(b'\n');
    stdin.write_all(&body).expect("write line");
    stdin.flush().expect("flush line");
}

/// Read one server message: exactly one `\n`-terminated JSON line, no headers.
fn read_message(stdout: &mut BufReader<ChildStdout>) -> Value {
    let mut line = String::new();
    let bytes = stdout.read_line(&mut line).expect("read response line");
    assert_ne!(bytes, 0, "EOF while waiting for MCP response");
    assert!(
        line.starts_with('{') && line.ends_with('\n'),
        "response must be a bare newline-terminated JSON line, got: {line:?}"
    );
    serde_json::from_str(line.trim_end_matches(['\r', '\n'])).expect("parse response JSON")
}

fn mcp_tool_call(home: &Path, token: &str, name: &str, arguments: Value) -> (Value, bool) {
    let mut child = Command::new(envelope_bin())
        .arg("mcp")
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
        .env("ENVELOPE_AGENT_TOKEN", token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn MCP server");
    let mut stdin = child.stdin.take().expect("MCP stdin");
    let stdout = child.stdout.take().expect("MCP stdout");
    let mut stdout = BufReader::new(stdout);
    write_line(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }),
    );
    let response = read_message(&mut stdout);
    drop(stdin);
    child.wait().expect("wait for MCP server");

    let result = &response["result"];
    let is_error = result["isError"].as_bool().unwrap_or(false);
    let text = result["content"][0]["text"]
        .as_str()
        .expect("tool result text");
    let json_text = text.strip_prefix("Error: ").unwrap_or(text);
    (
        serde_json::from_str(json_text).unwrap_or_else(|_| json!({ "_raw": text })),
        is_error,
    )
}

#[test]
fn agent_create_list_revoke_roundtrip_via_json() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();

    // create prints the raw token exactly once.
    let created_output = run(home, &["--json", "agent", "create", "skippy"]);
    assert!(created_output.status.success());
    let created_raw = String::from_utf8_lossy(&created_output.stdout);
    let created = json_stdout(&created_output);
    assert_eq!(created["status"], "created");
    assert_eq!(created["name"], "skippy");
    let token = created["token"].as_str().expect("token string");
    assert!(token.starts_with("envtok_"));
    assert_eq!(
        created["token_prefix"].as_str().unwrap(),
        &token[..15],
        "token_prefix must be the first 15 chars of the token"
    );
    assert_eq!(
        created_raw.matches(token).count(),
        1,
        "the raw agent token must appear exactly once in its creation response"
    );

    // list shows the agent, active, and NEVER a token or hash.
    let listed = run(home, &["--json", "agent", "list"]);
    assert!(listed.status.success());
    let listed = json_stdout(&listed);
    let rows = listed.as_array().expect("list is an array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "skippy");
    assert_eq!(rows[0]["status"], "active");
    let listed_str = listed.to_string();
    assert!(
        !listed_str.contains(token),
        "list output must never contain the raw token"
    );
    assert!(
        !listed_str.contains("token_hash"),
        "list output must never expose a token hash"
    );

    // revoke flips status to revoked.
    let revoked = run(home, &["--json", "agent", "revoke", "skippy"]);
    assert!(revoked.status.success());
    assert_eq!(json_stdout(&revoked)["status"], "revoked");
    let after = json_stdout(&run(home, &["--json", "agent", "show", "skippy"]));
    assert_eq!(after["status"], "revoked");
}

#[test]
fn agent_token_enforces_policy_and_revocation_at_mcp_startup() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();

    let created = json_stdout(&run(home, &["--json", "agent", "create", "policy-agent"]));
    let token = created["token"].as_str().expect("agent token");

    let configured = run(
        home,
        &[
            "agent",
            "policy",
            "set",
            "policy-agent",
            "--allow-accounts",
            "*",
            "--allow-folders",
            "*",
            "--allow-actions",
            "accounts.list",
            "--send-mode-ceiling",
            "draft-only",
        ],
    );
    assert!(configured.status.success(), "policy setup failed");

    let (allowed, is_error) = mcp_tool_call(home, token, "accounts", json!({}));
    assert!(!is_error, "allowed MCP tool must succeed: {allowed}");
    assert!(
        allowed.is_array(),
        "accounts tool returns an array: {allowed}"
    );

    let (denied, is_error) = mcp_tool_call(home, token, "inbox", json!({ "account": "any" }));
    assert!(is_error, "disallowed MCP tool must be rejected: {denied}");
    assert_eq!(denied["code"], "agent_policy_denied_action");

    assert!(
        run(home, &["agent", "revoke", "policy-agent"])
            .status
            .success()
    );
    let revoked = Command::new(envelope_bin())
        .arg("mcp")
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
        .env("ENVELOPE_AGENT_TOKEN", token)
        .stdin(Stdio::null())
        .output()
        .expect("run revoked MCP server");
    assert!(
        !revoked.status.success(),
        "revoked token must fail MCP startup"
    );
    assert!(
        !String::from_utf8_lossy(&revoked.stderr).contains(token),
        "revocation failure must not echo the token"
    );
}

#[test]
fn third_create_without_license_returns_agent_limit_code() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();

    assert!(run(home, &["agent", "create", "one"]).status.success());
    assert!(run(home, &["agent", "create", "two"]).status.success());

    let third = run(home, &["--json", "agent", "create", "three"]);
    assert!(
        !third.status.success(),
        "3rd active agent without a license must be denied"
    );
    let payload = json_stdout(&third);
    assert_eq!(payload["status"], "denied");
    assert_eq!(payload["error"]["code"], "agent_limit_license_required");
    // The friendly message must name the license activation command.
    assert!(
        payload["error"]["reason"]
            .as_str()
            .unwrap()
            .contains("license activate"),
        "denial reason must point to `envelope license activate`"
    );
    assert_eq!(payload["free_tier_limit"], 2);

    // Revoking one frees a slot: creation is allowed again.
    assert!(run(home, &["agent", "revoke", "two"]).status.success());
    assert!(
        run(home, &["agent", "create", "three"]).status.success(),
        "revoking an agent must free a free-tier slot"
    );
}

#[test]
fn policy_set_show_roundtrip_and_ceiling_validation() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    run(home, &["agent", "create", "skippy"]);

    let set = run(
        home,
        &[
            "--json",
            "agent",
            "policy",
            "set",
            "skippy",
            "--allow-accounts",
            "acc-1,acc-2",
            "--allow-folders",
            "*",
            "--allow-actions",
            "inbox.read,send",
            "--send-mode-ceiling",
            "confirm-send",
            "--allow-recipients",
            "ops@corp.test,@safe.test",
        ],
    );
    assert!(set.status.success());
    let set = json_stdout(&set);
    assert_eq!(set["send_mode_ceiling"], "confirm-send");
    assert_eq!(
        set["allowed_accounts"],
        serde_json::json!(["acc-1", "acc-2"])
    );
    assert_eq!(set["allowed_folders"], serde_json::json!("*"));

    let shown = json_stdout(&run(home, &["--json", "agent", "policy", "show", "skippy"]));
    assert_eq!(shown["send_mode_ceiling"], "confirm-send");
    assert_eq!(
        shown["allowed_actions"],
        serde_json::json!(["inbox.read", "send"])
    );
    assert_eq!(
        shown["allow_recipients"],
        serde_json::json!(["ops@corp.test", "@safe.test"])
    );

    // An invalid ceiling name is rejected against the four stable names.
    let bad = run(
        home,
        &[
            "agent",
            "policy",
            "set",
            "skippy",
            "--send-mode-ceiling",
            "bogus",
        ],
    );
    assert!(
        !bad.status.success(),
        "invalid ceiling name must be rejected"
    );
    let stderr = String::from_utf8_lossy(&bad.stderr);
    assert!(
        stderr.contains("send-mode-ceiling") || stderr.contains("send_mode_ceiling"),
        "error should name the ceiling flag, got: {stderr}"
    );
}

#[test]
fn contract_export_declares_agent_identity_block() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let output = run(temp.path(), &["contract"]);
    assert!(output.status.success());
    let contract: Value = serde_json::from_slice(&output.stdout).expect("contract JSON");

    // Stays v1 (additive).
    assert_eq!(contract["schema"], "envelope.agent_contract.v2");

    let block = &contract["agent_identity"];
    assert_eq!(block["env"], "ENVELOPE_AGENT_TOKEN");
    assert_eq!(block["free_tier"]["max_active_agents"], 2);
    assert_eq!(
        block["free_tier"]["over_limit_code"],
        "agent_limit_license_required"
    );
    // Tool->action map and denial codes must be advertised.
    assert_eq!(block["tool_action_map"]["send"], "send");
    assert_eq!(block["tool_action_map"]["inbox"], "inbox.read");
    let codes = block["policy_enforcement"]["denial_codes"]
        .as_array()
        .expect("denial_codes array");
    for code in [
        "agent_policy_denied_action",
        "agent_policy_denied_account",
        "agent_policy_denied_folder",
    ] {
        assert!(
            codes.iter().any(|c| c == code),
            "denial_codes must include {code}"
        );
    }
}
