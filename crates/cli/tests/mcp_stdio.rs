// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn envelope_bin() -> &'static str {
    env!("CARGO_BIN_EXE_envelope")
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
