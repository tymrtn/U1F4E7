// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! `envelope bulk <move|copy|flag|delete|tag>` — bulk message operations.
//!
//! Resolves a set of UIDs (from `--uids 1,2,9:14` or `--query <imap-query>`),
//! applies one operation across all of them with partial-failure semantics, and
//! action-logs the result. See [`envelope_email_transport::bulk`] for the engine.

use anyhow::{Context, Result, bail};
use envelope_email_store::CredentialBackend;
use envelope_email_transport::bulk::{BULK_UID_LIMIT, BulkOp, BulkRequest, BulkResult, BulkTarget};

use super::common::setup_credentials;

/// Parse a UID spec like `1,2,9:14` into an explicit UID list. Ranges are
/// inclusive; `a:b` with `a > b` is rejected. A single range whose span exceeds
/// [`BULK_UID_LIMIT`] is rejected at parse time so `--uids 1:4000000000` never
/// materializes gigabytes of UIDs before the per-call cap check fires.
pub fn parse_uid_spec(spec: &str) -> Result<Vec<u32>> {
    let mut out: Vec<u32> = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((lo, hi)) = part.split_once(':') {
            let lo: u32 = lo
                .trim()
                .parse()
                .with_context(|| format!("invalid UID range start '{lo}'"))?;
            let hi: u32 = hi
                .trim()
                .parse()
                .with_context(|| format!("invalid UID range end '{hi}'"))?;
            if lo > hi {
                bail!("invalid UID range '{part}': start {lo} > end {hi}");
            }
            // Reject an oversized span before allocating it. `hi - lo + 1` is the
            // inclusive count; compare against the transport cap without
            // overflowing (span is computed in u64).
            let span = u64::from(hi - lo) + 1;
            if span > BULK_UID_LIMIT as u64 {
                bail!(
                    "UID range '{part}' spans {span} UIDs, exceeding the bulk limit of \
                     {BULK_UID_LIMIT}; narrow the range"
                );
            }
            out.extend(lo..=hi);
        } else {
            let uid: u32 = part
                .parse()
                .with_context(|| format!("invalid UID '{part}'"))?;
            out.push(uid);
        }
    }
    if out.is_empty() {
        bail!("no UIDs parsed from '{spec}'");
    }
    Ok(out)
}

/// Build a [`BulkTarget`] from mutually-exclusive `--uids` / `--query`.
pub fn build_target(uids: Option<&str>, query: Option<&str>) -> Result<BulkTarget> {
    match (uids, query) {
        (Some(_), Some(_)) => bail!("provide only one of --uids or --query, not both"),
        (Some(spec), None) => Ok(BulkTarget::Uids(parse_uid_spec(spec)?)),
        (None, Some(q)) => Ok(BulkTarget::Search(q.to_string())),
        (None, None) => bail!("provide --uids <spec> or --query <imap-query>"),
    }
}

/// Whether a delete request should run as a dry run. Per approved design: bulk
/// delete requires `--confirm`; if neither `--confirm` nor `--dry-run` is given,
/// it runs as a dry run (and says so) rather than deleting.
pub fn delete_effective_dry_run(dry_run: bool, confirm: bool) -> bool {
    dry_run || !confirm
}

#[allow(clippy::too_many_arguments)]
#[tokio::main]
pub async fn run(
    op: BulkOp,
    target: BulkTarget,
    folder: &str,
    dry_run: bool,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;
    let account_id = creds.account.id.clone();

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    let action_type = op.action_type();
    let req = BulkRequest {
        target,
        op,
        folder: folder.to_string(),
        dry_run,
    };

    let result =
        match envelope_email_transport::bulk::execute(&mut client, &db, &account_id, &req).await {
            Ok(r) => r,
            Err(e) => {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "action": action_type,
                            "ok": false,
                            "code": e.code(),
                            "error": e.to_string(),
                        })
                    );
                } else {
                    eprintln!("bulk {action_type} failed [{}]: {e}", e.code());
                }
                // Surface the failure to the process exit code too.
                return Err(anyhow::anyhow!("{e}"));
            }
        };

    // Action-log a single entry per bulk op (count + uids summary), matching the
    // batched granularity approved for bulk. Skip on dry run (no mutation).
    if !result.dry_run && !result.succeeded.is_empty() {
        let uids_summary = summarize_uids(&result.succeeded);
        let justification = format!(
            "bulk {action_type}: {} succeeded, {} failed",
            result.succeeded.len(),
            result.failed.len()
        );
        let _ = db.log_action(
            &account_id,
            action_type,
            1.0,
            &justification,
            &uids_summary,
            None,
            None,
        );
    }

    emit(&result, action_type, folder, json);
    Ok(())
}

/// Summarize a UID list for the action log, capping the inline list so a huge
/// bulk op doesn't write an unbounded string. Notes truncation when capped.
fn summarize_uids(uids: &[u32]) -> String {
    const MAX_INLINE: usize = 50;
    if uids.len() <= MAX_INLINE {
        uids.iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",")
    } else {
        let head = uids[..MAX_INLINE]
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");
        format!("{head},…(+{} more)", uids.len() - MAX_INLINE)
    }
}

fn emit(result: &BulkResult, action_type: &str, folder: &str, json: bool) {
    if json {
        let mut v = serde_json::to_value(result).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(obj) = v.as_object_mut() {
            obj.insert("action".to_string(), serde_json::json!(action_type));
            obj.insert("folder".to_string(), serde_json::json!(folder));
            obj.insert("ok".to_string(), serde_json::json!(true));
        }
        println!("{v}");
        return;
    }

    if result.dry_run {
        println!(
            "DRY RUN — would {} {} message(s) in {folder} (no changes made):",
            action_type.trim_start_matches("bulk_"),
            result.requested
        );
        println!("  UIDs: {}", summarize_uids(&result.resolved_uids));
        return;
    }

    println!(
        "bulk {}: {} succeeded, {} failed ({} requested) in {folder}",
        action_type.trim_start_matches("bulk_"),
        result.succeeded.len(),
        result.failed.len(),
        result.requested
    );
    if !result.failed.is_empty() {
        for f in &result.failed {
            println!("  FAILED uid {} [{}]: {}", f.uid, f.code, f.reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_uids() {
        assert_eq!(parse_uid_spec("1,2,3").unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn parse_range() {
        assert_eq!(parse_uid_spec("9:14").unwrap(), vec![9, 10, 11, 12, 13, 14]);
    }

    #[test]
    fn parse_mixed_spec() {
        assert_eq!(
            parse_uid_spec("1,2,9:14").unwrap(),
            vec![1, 2, 9, 10, 11, 12, 13, 14]
        );
    }

    #[test]
    fn parse_rejects_reversed_range() {
        assert!(parse_uid_spec("14:9").is_err());
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_uid_spec("abc").is_err());
    }

    #[test]
    fn parse_rejects_oversized_range_span() {
        // A span far larger than the bulk limit must be rejected at parse time,
        // before allocating gigabytes of UIDs.
        let err = parse_uid_spec("1:4000000000").unwrap_err();
        assert!(
            err.to_string().contains(&BULK_UID_LIMIT.to_string()),
            "error should name the bulk limit: {err}"
        );
    }

    #[test]
    fn parse_accepts_span_at_limit_and_rejects_one_over() {
        // Exactly BULK_UID_LIMIT wide is allowed (the per-call cap check owns the
        // final decision); one UID wider is rejected by the span guard.
        assert!(parse_uid_spec(&format!("1:{BULK_UID_LIMIT}")).is_ok());
        assert!(parse_uid_spec(&format!("1:{}", BULK_UID_LIMIT + 1)).is_err());
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse_uid_spec("").is_err());
        assert!(parse_uid_spec("  ").is_err());
    }

    #[test]
    fn parse_tolerates_whitespace() {
        assert_eq!(parse_uid_spec(" 1 , 2 ").unwrap(), vec![1, 2]);
    }

    #[test]
    fn build_target_rejects_both() {
        assert!(build_target(Some("1"), Some("q")).is_err());
    }

    #[test]
    fn build_target_rejects_neither() {
        assert!(build_target(None, None).is_err());
    }

    #[test]
    fn build_target_uids() {
        match build_target(Some("1,2"), None).unwrap() {
            BulkTarget::Uids(u) => assert_eq!(u, vec![1, 2]),
            _ => panic!("expected Uids"),
        }
    }

    #[test]
    fn build_target_search() {
        match build_target(None, Some("FROM bob")).unwrap() {
            BulkTarget::Search(q) => assert_eq!(q, "FROM bob"),
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn delete_defaults_to_dry_run_without_confirm() {
        // Neither --confirm nor --dry-run: must run as dry run.
        assert!(delete_effective_dry_run(false, false));
    }

    #[test]
    fn delete_confirm_actually_deletes() {
        // --confirm and not --dry-run: real delete.
        assert!(!delete_effective_dry_run(false, true));
    }

    #[test]
    fn delete_dry_run_flag_wins_over_confirm() {
        // Explicit --dry-run stays a dry run even with --confirm.
        assert!(delete_effective_dry_run(true, true));
    }

    #[test]
    fn summarize_uids_caps_long_lists() {
        let uids: Vec<u32> = (1..=100).collect();
        let s = summarize_uids(&uids);
        assert!(s.contains("+50 more"));
    }

    #[test]
    fn summarize_uids_short_list_inline() {
        assert_eq!(summarize_uids(&[1, 2, 3]), "1,2,3");
    }
}
