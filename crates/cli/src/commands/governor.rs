// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! `envelope-email governor` subcommand — status and testing.

use anyhow::Result;

use crate::governor::{self, GovernorAttrs};

/// Show governor status: enabled/disabled, binary path, reachability.
pub fn run_status(json: bool) -> Result<()> {
    let status = governor::test_connectivity()?;

    if json {
        println!("{}", serde_json::to_string_pretty(&status.to_json())?);
    } else {
        println!("Governor integration");
        println!("  Enabled:   {}", if status.enabled { "yes" } else { "no" });
        println!("  Binary:    {}", status.binary_path);
        println!(
            "  Reachable: {}",
            if status.reachable {
                "yes"
            } else {
                "NO — governor binary not found or not responding"
            }
        );
        if let Some(ref v) = status.version {
            println!("  Version:   {v}");
        }
        println!();
        if !status.enabled {
            println!("To enable: export ENVELOPE_GOVERNOR=true");
        }
        if !status.reachable {
            println!(
                "Governor binary not found at '{}'. Install it or set GOVERNOR_PATH.",
                status.binary_path
            );
        }
    }

    Ok(())
}

/// Dry-run a send through governor scoring without actually sending.
pub fn run_test_send(to: &str, subject: &str, json: bool) -> Result<()> {
    let attrs = GovernorAttrs::email_send(to, subject);

    // Check if governor is even reachable
    let status = governor::test_connectivity()?;
    if !status.reachable {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "status": "error",
                    "reason": "governor binary not found",
                    "binary_path": status.binary_path,
                })
            );
        } else {
            println!(
                "Governor binary not found at '{}'. Install it or set GOVERNOR_PATH.",
                status.binary_path
            );
        }
        return Ok(());
    }

    match governor::check_verbose(&attrs) {
        Ok(result) => {
            let allowed = result.result == "execute";
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": if allowed { "allowed" } else { "denied" },
                        "to": to,
                        "subject": subject,
                        "attrs": attrs.attrs,
                        "score": result.score,
                        "result": result.result,
                        "breakdown": result.breakdown,
                    })
                );
            } else if allowed {
                println!("✓ Governor would ALLOW sending to {to}");
                println!("  Subject:   \"{subject}\"");
                println!("  Attrs:     {}", attrs.attrs.join(", "));
                println!("  Score:     {:.4}", result.score);
                println!("  Breakdown: {}", result.breakdown);
            } else {
                println!("✗ Governor would DENY sending to {to}");
                println!("  Subject:   \"{subject}\"");
                println!("  Attrs:     {}", attrs.attrs.join(", "));
                println!("  Score:     {:.4}", result.score);
                println!("  Breakdown: {}", result.breakdown);
                println!();
                println!("Tip: Add context attributes to increase score:");
                println!("  user_requested, known_contact, reply_to_known, etc.");
            }
        }
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "error",
                        "to": to,
                        "subject": subject,
                        "attrs": attrs.attrs,
                        "error": format!("{e:#}"),
                    })
                );
            } else {
                println!("✗ Governor error for send to {to}");
                println!("  {e:#}");
            }
        }
    }

    Ok(())
}

/// Dry-run a destructive operation through governor scoring.
pub fn run_test_delete(action: &str, json: bool) -> Result<()> {
    let attrs = GovernorAttrs::destructive(action);

    let status = governor::test_connectivity()?;
    if !status.reachable {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "status": "error",
                    "reason": "governor binary not found",
                    "binary_path": status.binary_path,
                })
            );
        } else {
            println!(
                "Governor binary not found at '{}'. Install it or set GOVERNOR_PATH.",
                status.binary_path
            );
        }
        return Ok(());
    }

    match governor::check_verbose(&attrs) {
        Ok(result) => {
            let allowed = result.result == "execute";
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": if allowed { "allowed" } else { "denied" },
                        "action": action,
                        "attrs": attrs.attrs,
                        "score": result.score,
                        "result": result.result,
                        "breakdown": result.breakdown,
                    })
                );
            } else if allowed {
                println!("✓ Governor would ALLOW: {action}");
                println!("  Attrs:     {}", attrs.attrs.join(", "));
                println!("  Score:     {:.4}", result.score);
                println!("  Breakdown: {}", result.breakdown);
            } else {
                println!("✗ Governor would DENY: {action}");
                println!("  Attrs:     {}", attrs.attrs.join(", "));
                println!("  Score:     {:.4}", result.score);
                println!("  Breakdown: {}", result.breakdown);
                println!();
                println!("Tip: Add context attributes to increase score:");
                println!("  user_requested, reversible, low_stakes, etc.");
            }
        }
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "error",
                        "action": action,
                        "attrs": attrs.attrs,
                        "error": format!("{e:#}"),
                    })
                );
            } else {
                println!("✗ Governor error for: {action}");
                println!("  {e:#}");
            }
        }
    }

    Ok(())
}
