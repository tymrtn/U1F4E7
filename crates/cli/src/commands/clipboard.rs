// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Secure local clipboard handoff for secrets.
//!
//! This module copies a secret value directly into the OS clipboard for a
//! keyboard-present local operator. The secret is written only to the clipboard
//! tool's stdin — it is never printed to stdout/stderr, returned to callers, or
//! written to a file. Callers print metadata only.
//!
//! This is transient local convenience, not secure storage. It is intentionally
//! unavailable for any remote/headless delivery path.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::process::{Command, Stdio};

/// A resolved clipboard backend: the command plus its arguments.
struct ClipboardTool {
    program: &'static str,
    args: &'static [&'static str],
}

/// Detect an available clipboard tool for the current platform.
///
/// macOS uses `pbcopy`. Linux prefers Wayland's `wl-copy`, then X11's `xclip`
/// or `xsel`. Returns an explicit error (never a silent no-op) when no tool is
/// found, so the operator knows the secret was not copied.
fn detect_tool() -> Result<ClipboardTool> {
    #[cfg(target_os = "macos")]
    {
        if which("pbcopy") {
            return Ok(ClipboardTool {
                program: "pbcopy",
                args: &[],
            });
        }
        bail!("clipboard tool `pbcopy` not found on PATH");
    }

    #[cfg(not(target_os = "macos"))]
    {
        if which("wl-copy") {
            return Ok(ClipboardTool {
                program: "wl-copy",
                args: &[],
            });
        }
        if which("xclip") {
            return Ok(ClipboardTool {
                program: "xclip",
                args: &["-selection", "clipboard"],
            });
        }
        if which("xsel") {
            return Ok(ClipboardTool {
                program: "xsel",
                args: &["--clipboard", "--input"],
            });
        }
        bail!(
            "no clipboard tool found on PATH (looked for wl-copy, xclip, xsel); \
             install one to use clipboard handoff"
        );
    }
}

/// Return true if `program` resolves on PATH.
fn which(program: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {program}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Write `secret` to the OS clipboard. The secret is passed only via the child
/// process stdin and is never logged or returned.
pub fn copy_secret(secret: &str) -> Result<&'static str> {
    let tool = detect_tool()?;
    let mut child = Command::new(tool.program)
        .args(tool.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to launch clipboard tool `{}`", tool.program))?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .context("failed to open clipboard tool stdin")?;
        stdin
            .write_all(secret.as_bytes())
            .context("failed to write secret to clipboard tool")?;
    }

    let status = child
        .wait()
        .with_context(|| format!("clipboard tool `{}` did not exit cleanly", tool.program))?;
    if !status.success() {
        bail!("clipboard tool `{}` exited with failure", tool.program);
    }
    Ok(tool.program)
}

/// Spawn a detached process that clears the clipboard after `ttl_secs`.
///
/// The clearing command writes an empty string to the same clipboard backend.
/// No secret is involved in this path. Best-effort: if spawning fails the caller
/// should still warn the operator to clear the clipboard manually.
pub fn schedule_clear(ttl_secs: u64) -> Result<()> {
    let tool = detect_tool()?;
    let clear_cmd = if tool.args.is_empty() {
        format!("sleep {ttl_secs}; printf '' | {}", tool.program)
    } else {
        format!(
            "sleep {ttl_secs}; printf '' | {} {}",
            tool.program,
            tool.args.join(" ")
        )
    };
    Command::new("sh")
        .arg("-c")
        .arg(clear_cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to schedule clipboard clear")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_a_real_program() {
        // `sh` is present on every supported platform.
        assert!(which("sh"));
    }

    #[test]
    fn which_rejects_missing_program() {
        assert!(!which("envelope-definitely-not-a-real-binary-xyz"));
    }
}
