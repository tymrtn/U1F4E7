// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Governor integration — optional safety layer for destructive/outbound commands.
//!
//! When enabled, Envelope shells out to the `governor` CLI before executing
//! governed operations (send, delete, move-to-trash). If Governor denies the
//! action, Envelope returns an error instead of executing.
//!
//! Configuration priority (highest wins):
//! 1. `--no-governor` CLI flag → always bypass
//! 2. `ENVELOPE_GOVERNOR` env var → "true"/"1" enables, "false"/"0" disables
//! 3. Default: disabled
//!
//! Governor CLI interface:
//!   `governor admin score --attr <key> [--attr <key>...] --json`
//! Returns JSON with `{ "result": "execute"|"deny", "score": f64, "breakdown": {...} }`

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::process::Command;

/// Attributes that describe what kind of operation is being governed.
#[derive(Debug, Clone)]
pub struct GovernorAttrs {
    pub attrs: Vec<String>,
    pub description: String,
}

impl GovernorAttrs {
    /// Email send operation — outbound, external side effects.
    pub fn email_send(to: &str, subject: &str) -> Self {
        Self {
            attrs: vec!["outbound".into(), "email_send".into()],
            description: format!("send email to {to} subject \"{subject}\""),
        }
    }

    /// Destructive operation — delete, move to Trash/Junk.
    pub fn destructive(action: &str) -> Self {
        Self {
            attrs: vec!["destructive".into()],
            description: action.to_string(),
        }
    }
}

/// Result from `governor admin score --json`.
#[derive(Debug, Deserialize)]
pub struct GovernorScoreResult {
    pub result: String,
    pub score: f64,
    #[serde(default)]
    pub breakdown: serde_json::Value,
}

/// Resolve the governor binary path from `GOVERNOR_PATH` env var or default.
pub fn governor_path() -> String {
    std::env::var("GOVERNOR_PATH").unwrap_or_else(|_| "governor".to_string())
}

/// Check whether governor is enabled.
///
/// - If `no_governor` is true (CLI flag), always returns false.
/// - Otherwise checks `ENVELOPE_GOVERNOR` env var.
/// - Default: false (disabled).
pub fn is_enabled(no_governor: bool) -> bool {
    if no_governor {
        return false;
    }
    match std::env::var("ENVELOPE_GOVERNOR") {
        Ok(val) => matches!(val.to_lowercase().as_str(), "true" | "1" | "yes"),
        Err(_) => false,
    }
}

/// Run a governor score check. Returns Ok(()) if allowed, Err if denied or governor fails.
///
/// Uses `governor admin score --attr <key> --json` to evaluate the attributes.
pub fn check(attrs: &GovernorAttrs) -> Result<()> {
    let bin = governor_path();

    let mut cmd = Command::new(&bin);
    cmd.arg("admin");
    cmd.arg("score");

    for attr in &attrs.attrs {
        cmd.arg("--attr");
        cmd.arg(attr);
    }

    cmd.arg("--json");

    let output = cmd
        .output()
        .with_context(|| format!(
            "failed to execute governor at '{bin}'. Is it installed and in PATH? \
             Set GOVERNOR_PATH to override."
        ))?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "governor failed (exit code {}): {stderr}\n\
             Use --no-governor to bypass in emergencies.",
            output.status.code().unwrap_or(-1),
        );
    }

    let result: GovernorScoreResult = serde_json::from_str(&stdout)
        .with_context(|| format!("failed to parse governor JSON output: {stdout}"))?;

    if result.result != "execute" {
        bail!(
            "governor denied: score {:.4} ({}) — attrs: [{}]\n\
             Action: {}\n\
             Use --no-governor to bypass in emergencies.",
            result.score,
            result.result,
            attrs.attrs.join(", "),
            attrs.description,
        );
    }

    Ok(())
}

/// Run a governor score check and return the full result for display.
pub fn check_verbose(attrs: &GovernorAttrs) -> Result<GovernorScoreResult> {
    let bin = governor_path();

    let mut cmd = Command::new(&bin);
    cmd.arg("admin");
    cmd.arg("score");

    for attr in &attrs.attrs {
        cmd.arg("--attr");
        cmd.arg(attr);
    }

    cmd.arg("--json");

    let output = cmd
        .output()
        .with_context(|| format!(
            "failed to execute governor at '{bin}'. Is it installed and in PATH? \
             Set GOVERNOR_PATH to override."
        ))?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("governor failed (exit code {}): {stderr}", output.status.code().unwrap_or(-1));
    }

    let result: GovernorScoreResult = serde_json::from_str(&stdout)
        .with_context(|| format!("failed to parse governor JSON output: {stdout}"))?;

    Ok(result)
}

/// Test whether the governor binary is reachable and responding.
pub fn test_connectivity() -> Result<GovernorStatus> {
    let bin = governor_path();
    let enabled = is_enabled(false);

    let version_output = Command::new(&bin)
        .arg("--version")
        .output();

    let (reachable, version) = match version_output {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
            (true, Some(v))
        }
        _ => (false, None),
    };

    Ok(GovernorStatus {
        enabled,
        binary_path: bin,
        reachable,
        version,
    })
}

/// Status information for the `governor status` subcommand.
#[derive(Debug)]
pub struct GovernorStatus {
    pub enabled: bool,
    pub binary_path: String,
    pub reachable: bool,
    pub version: Option<String>,
}

impl GovernorStatus {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "enabled": self.enabled,
            "binary_path": self.binary_path,
            "reachable": self.reachable,
            "version": self.version,
        })
    }
}

/// Check if a move destination is a destructive folder (Trash, Junk, etc.)
pub fn is_destructive_folder(folder: &str) -> bool {
    let lower = folder.to_lowercase();
    lower.contains("trash")
        || lower.contains("junk")
        || lower.contains("spam")
        || lower.contains("deleted")
        || lower == "[gmail]/bin"
        || lower == "[gmail]/papelera"
}
