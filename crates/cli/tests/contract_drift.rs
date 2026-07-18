// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Schema pinning test: `envelope contract` output must equal
//! docs/schemas/envelope.agent_contract.v1.json exactly (parsed-JSON equality,
//! not string comparison — key order is not significant).
//!
//! If this test fails, either:
//!   a) A code change altered the contract — update the schema file and commit
//!      both together per CLAUDE.md's agent contract invariants, OR
//!   b) The schema file was edited without updating the code — regenerate with
//!      `envelope contract > docs/schemas/envelope.agent_contract.v1.json`
//!      after verifying the change is intentional.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn envelope_bin() -> &'static str {
    env!("CARGO_BIN_EXE_envelope")
}

/// Path to the canonical schema file relative to the workspace root.
/// CARGO_MANIFEST_DIR for the cli crate is crates/cli/; we go up two levels.
fn schema_path() -> std::path::PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Path::new(manifest_dir)
        .join("..") // crates/
        .join("..") // repo root
        .join("docs/schemas/envelope.agent_contract.v1.json")
}

#[test]
fn contract_output_matches_committed_schema() {
    let temp = tempfile::tempdir().expect("temp HOME");

    let output = Command::new(envelope_bin())
        .args(["contract"])
        .env("HOME", temp.path())
        .env("ENVELOPE_HOME", temp.path())
        .output()
        .expect("run envelope contract");

    assert!(
        output.status.success(),
        "`envelope contract` exited non-zero ({})\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let live: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "`envelope contract` stdout was not valid JSON ({e}):\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    });

    let schema_file = schema_path();
    let schema_bytes = std::fs::read(&schema_file).unwrap_or_else(|e| {
        panic!(
            "Could not read schema file at {}: {e}\n\
             Run `envelope contract > docs/schemas/envelope.agent_contract.v1.json` \
             to generate it.",
            schema_file.display()
        )
    });

    let committed: Value = serde_json::from_slice(&schema_bytes).unwrap_or_else(|e| {
        panic!(
            "Schema file {} is not valid JSON: {e}",
            schema_file.display()
        )
    });

    if live != committed {
        // Emit a diff-friendly report: each top-level key where the values diverge.
        let live_obj = live.as_object().expect("contract is a JSON object");
        let committed_obj = committed.as_object().expect("schema is a JSON object");

        let mut diffs: Vec<String> = Vec::new();

        // Keys present in live but missing or different in committed
        for (key, live_val) in live_obj {
            match committed_obj.get(key) {
                None => diffs.push(format!("  [+live only] {key}")),
                Some(committed_val) if live_val != committed_val => {
                    diffs.push(format!("  [changed]    {key}"));
                }
                _ => {}
            }
        }

        // Keys present in committed but gone from live
        for key in committed_obj.keys() {
            if !live_obj.contains_key(key) {
                diffs.push(format!("  [-live miss] {key}"));
            }
        }

        panic!(
            "Contract drift detected: `envelope contract` output does not match \
             docs/schemas/envelope.agent_contract.v1.json.\n\
             Diverging top-level keys:\n{}\n\n\
             If this change is intentional, regenerate the schema:\n  \
             envelope contract > docs/schemas/envelope.agent_contract.v1.json\n\
             and commit both files together.",
            diffs.join("\n")
        );
    }
}
