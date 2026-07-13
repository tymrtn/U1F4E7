// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! `envelope license` command group: activate, show status, and deactivate
//! honor-system license keys.
//!
//! ## Key format
//!
//! License keys must match: `env-lic-<suffix>` where `<suffix>` is at least
//! 16 ASCII alphanumeric or hyphen characters. The total minimum key length
//! is therefore 24 characters (`env-lic-` = 8, suffix ≥ 16).
//!
//! Examples of valid keys:
//!   env-lic-abcdefghijklmnop
//!   env-lic-PROD-2026-xxxxxxxxxxxxxxx
//!
//! The full key is **never** echoed after storage. The output refers to the
//! key only by its prefix (first 12 characters of the full key: `env-lic-xxxx`).
//!
//! ## Stable error codes
//! - `license_key_invalid_format` — key does not pass the prefix/length check.

use anyhow::{Context, Result};
use envelope_email_store::Database;
use serde_json::json;

/// Stable error code for malformed license keys.
pub const LICENSE_KEY_INVALID_FORMAT_CODE: &str = "license_key_invalid_format";

/// Perpetual expiry sentinel stored when no expiry is specified.
/// The schema requires expires_at NOT NULL, so we use a far-future date.
const PERPETUAL_EXPIRES_AT: &str = "9999-12-31T23:59:59";

/// Required prefix for all license keys.
const KEY_PREFIX: &str = "env-lic-";

/// Minimum number of characters *after* the prefix (`env-lic-`).
const MIN_SUFFIX_LEN: usize = 16;

/// Characters allowed in the suffix: ASCII alphanumeric or `-`.
fn is_valid_suffix_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

/// Validate the license key format.
/// Returns `Ok(())` on success, `Err` with the stable error code message on failure.
fn validate_key_format(key: &str) -> std::result::Result<(), &'static str> {
    if !key.starts_with(KEY_PREFIX) {
        return Err(LICENSE_KEY_INVALID_FORMAT_CODE);
    }
    let suffix = &key[KEY_PREFIX.len()..];
    if suffix.len() < MIN_SUFFIX_LEN {
        return Err(LICENSE_KEY_INVALID_FORMAT_CODE);
    }
    if !suffix.chars().all(is_valid_suffix_char) {
        return Err(LICENSE_KEY_INVALID_FORMAT_CODE);
    }
    Ok(())
}

/// Return the display prefix for a key: the first 12 characters followed by `…`.
/// This is enough to identify `env-lic-xxxx` without leaking the secret suffix.
fn key_display_prefix(key: &str) -> String {
    let take = key.len().min(12);
    format!("{}…", &key[..take])
}

// ── activate ────────────────────────────────────────────────────────

pub fn run_activate(key: &str, json_mode: bool) -> Result<()> {
    // Validate format before opening the DB, so bad keys never touch storage.
    if let Err(code) = validate_key_format(key) {
        let prefix = key_display_prefix(key);
        if json_mode {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "status": "error",
                    "error": {
                        "code": code,
                        "reason": format!(
                            "license key must start with `{}` and have at least {} \
                             alphanumeric/hyphen characters after the prefix",
                            KEY_PREFIX, MIN_SUFFIX_LEN
                        ),
                    },
                    "key_prefix": prefix,
                }))?
            );
        } else {
            eprintln!(
                "Invalid license key format (code: {code}). \
                 Keys must start with `{KEY_PREFIX}` followed by at least \
                 {MIN_SUFFIX_LEN} alphanumeric or hyphen characters."
            );
        }
        std::process::exit(1);
    }

    let db = Database::open_default().context("failed to open database")?;
    let prefix = key_display_prefix(key);

    // Idempotent: if the same key is already active, return success without
    // re-storing (avoids resetting activated_at on re-runs).
    if let Some(existing) = db
        .get_active_license()
        .context("failed to read existing license")?
    {
        if existing.token == key {
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "status": "activated",
                        "key_prefix": prefix,
                        "activated_at": existing.activated_at,
                        "note": "already_active",
                    }))?
                );
            } else {
                println!("License already active (key_prefix: {prefix}).");
            }
            return Ok(());
        }
    }

    // Store the license. expires_at = perpetual sentinel (NULL not allowed by schema).
    // licensee defaults to "licensed" since honor-system has no identity service.
    // features defaults to empty (all features unlocked by presence of any valid license).
    db.store_license(key, "licensed", PERPETUAL_EXPIRES_AT, &[])
        .context("failed to store license")?;

    // Read back activated_at from the stored record for the response.
    let stored = db
        .get_active_license()
        .context("failed to read stored license")?
        .context("license store succeeded but get_active_license returned None")?;

    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "status": "activated",
                "key_prefix": prefix,
                "activated_at": stored.activated_at,
            }))?
        );
    } else {
        println!("License activated (key_prefix: {prefix}).");
        println!("  activated_at: {}", stored.activated_at);
        println!("Agent limit is now unlocked.");
    }

    Ok(())
}

// ── status ──────────────────────────────────────────────────────────

pub fn run_status(json_mode: bool) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let license = db.get_active_license().context("failed to read license")?;

    match license {
        Some(lic) => {
            let prefix = key_display_prefix(&lic.token);
            let expires = if lic.expires_at == PERPETUAL_EXPIRES_AT {
                None
            } else {
                Some(lic.expires_at.as_str())
            };

            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "licensed": true,
                        "key_prefix": prefix,
                        "activated_at": lic.activated_at,
                        "expires_at": expires,
                    }))?
                );
            } else {
                println!("License: active");
                println!("  key_prefix:   {prefix}");
                println!("  activated_at: {}", lic.activated_at);
                match expires {
                    None => println!("  expires_at:   never (perpetual)"),
                    Some(e) => println!("  expires_at:   {e}"),
                }
            }
        }
        None => {
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "licensed": false,
                        "key_prefix": null,
                        "activated_at": null,
                        "expires_at": null,
                    }))?
                );
            } else {
                println!("License: unlicensed (free tier — up to 2 active agents)");
                println!("  Run `envelope license activate` to unlock.");
            }
        }
    }

    Ok(())
}

// ── deactivate ──────────────────────────────────────────────────────

pub fn run_deactivate(json_mode: bool) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;

    // Check whether a license is present before deleting so we can report
    // accurately in both idempotent cases.
    let had_license = db
        .get_active_license()
        .context("failed to read license")?
        .is_some();

    db.delete_license().context("failed to delete license")?;

    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "status": "deactivated",
                "had_active_license": had_license,
            }))?
        );
    } else if had_license {
        println!("License deactivated. Reverted to free tier.");
    } else {
        println!("No active license to deactivate (already on free tier).");
    }

    Ok(())
}

// ── unit tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_key_passes_format_check() {
        assert!(validate_key_format("env-lic-abcdefghijklmnop").is_ok());
        assert!(validate_key_format("env-lic-PROD-2026-xxxxxxxxxxxx").is_ok());
        assert!(validate_key_format("env-lic-0123456789abcdef").is_ok());
    }

    #[test]
    fn wrong_prefix_fails_with_stable_code() {
        let err = validate_key_format("lic-env-abcdefghijklmnop").unwrap_err();
        assert_eq!(err, LICENSE_KEY_INVALID_FORMAT_CODE);
    }

    #[test]
    fn short_suffix_fails() {
        // Suffix is 15 chars (one short of minimum 16).
        let err = validate_key_format("env-lic-short15").unwrap_err();
        assert_eq!(err, LICENSE_KEY_INVALID_FORMAT_CODE);
    }

    #[test]
    fn special_chars_in_suffix_fail() {
        let err = validate_key_format("env-lic-abc@def!ghijklmno").unwrap_err();
        assert_eq!(err, LICENSE_KEY_INVALID_FORMAT_CODE);
    }

    #[test]
    fn key_display_prefix_never_exposes_full_key() {
        let key = "env-lic-supersecretlongkey";
        let disp = key_display_prefix(key);
        // Must be exactly 12 chars + "…".
        assert_eq!(disp, "env-lic-supe…");
        assert!(!disp.contains("supersecretlongkey"));
    }

    #[test]
    fn key_display_prefix_short_key_is_safe() {
        // A key shorter than 12 chars (e.g. during fuzzing) must not panic.
        let key = "env-lic-";
        let disp = key_display_prefix(key);
        assert!(disp.ends_with('…'));
    }
}
