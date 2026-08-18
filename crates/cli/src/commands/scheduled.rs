// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result, bail};
use envelope_email_store::Database;
use envelope_email_store::credential_store::CredentialBackend;

use super::common::resolve_account;

/// Build a non-secret summary (filename, content_type, size) of stored draft
/// attachments. Never includes `data_base64`, so scheduled-send listings cannot
/// leak snapshotted attachment bytes.
fn attachment_summaries(attachments: &[serde_json::Value]) -> Vec<serde_json::Value> {
    attachments
        .iter()
        .map(|a| {
            serde_json::json!({
                "filename": a.get("filename").cloned().unwrap_or(serde_json::Value::Null),
                "content_type": a.get("content_type").cloned().unwrap_or(serde_json::Value::Null),
                "size": a.get("size").cloned().unwrap_or(serde_json::Value::Null),
            })
        })
        .collect()
}

/// List scheduled messages (drafts with send_after set, still in draft status).
pub fn run_list(account: Option<&str>, json: bool, _backend: CredentialBackend) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;

    // Resolve account if provided
    let account_id = match account {
        Some(a) => {
            let acct = resolve_account(&db, Some(a))?;
            Some(acct.id)
        }
        None => None,
    };

    // Query drafts with send_after set and status = 'draft'
    let drafts = if let Some(ref acct_id) = account_id {
        db.list_drafts(acct_id, Some("draft"), 100, 0)
            .context("failed to list drafts")?
            .into_iter()
            .filter(|d| d.send_after.is_some())
            .collect::<Vec<_>>()
    } else {
        // List all accounts and aggregate
        let accounts = db.list_accounts().context("failed to list accounts")?;
        let mut all = Vec::new();
        for acct in &accounts {
            let mut drafts = db
                .list_drafts(&acct.id, Some("draft"), 100, 0)
                .context("failed to list drafts")?
                .into_iter()
                .filter(|d| d.send_after.is_some())
                .collect::<Vec<_>>();
            all.append(&mut drafts);
        }
        all
    };

    if json {
        let items: Vec<serde_json::Value> = drafts
            .iter()
            .map(|d| {
                serde_json::json!({
                    "draft_id": d.id,
                    "account_id": d.account_id,
                    "to": d.to_addr,
                    "subject": d.subject,
                    "send_after": d.send_after,
                    "created_at": d.created_at,
                    "attachments": attachment_summaries(&d.attachments),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else {
        if drafts.is_empty() {
            println!("No scheduled messages");
            return Ok(());
        }

        println!(
            "{:<36}  {:<28}  {:<22}  {}",
            "DRAFT ID", "TO", "SEND AT", "SUBJECT"
        );
        println!("{}", "-".repeat(110));
        for d in &drafts {
            let subject = d.subject.as_deref().unwrap_or("-");
            let subject_display = if subject.len() > 30 {
                format!("{}...", &subject[..27])
            } else {
                subject.to_string()
            };
            let to_display = if d.to_addr.len() > 26 {
                format!("{}...", &d.to_addr[..23])
            } else {
                d.to_addr.clone()
            };
            let send_at = d.send_after.as_deref().unwrap_or("-");
            println!(
                "{:<36}  {:<28}  {:<22}  {}",
                d.id, to_display, send_at, subject_display,
            );
            if !d.attachments.is_empty() {
                for a in attachment_summaries(&d.attachments) {
                    println!(
                        "  attachment: {} ({} bytes, {})",
                        a["filename"].as_str().unwrap_or("attachment"),
                        a["size"].as_u64().unwrap_or(0),
                        a["content_type"]
                            .as_str()
                            .unwrap_or("application/octet-stream"),
                    );
                }
            }
        }
        println!("\n{} scheduled message(s)", drafts.len());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_summaries_exclude_bytes() {
        let attachments = vec![serde_json::json!({
            "filename": "packet.txt",
            "content_type": "text/plain",
            "size": 5,
            "data_base64": "aGVsbG8=",
        })];
        let summaries = attachment_summaries(&attachments);
        let serialized = serde_json::to_string(&summaries).unwrap();
        assert!(!serialized.contains("data_base64"));
        assert!(!serialized.contains("aGVsbG8="));
        assert!(serialized.contains("packet.txt"));
        assert_eq!(summaries[0]["size"], 5);
    }

    // ── hold: unqueue without discarding ──────────────────────────────

    fn seeded_db() -> Database {
        let db = Database::open_memory().unwrap();
        db.conn().execute("INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port, encrypted_password) VALUES ('acc1', 'Test', 'op@example.com', 'example.com', 'smtp.example.com', 587, 'imap.example.com', 993, 'x')", []).unwrap();
        db
    }

    fn scheduled_draft(db: &Database, send_after: &str) -> String {
        let draft = db
            .create_draft(
                "acc1",
                "buyer@example.com",
                Some("Quarterly update"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&draft.id, send_after).unwrap();
        draft.id
    }

    #[test]
    fn hold_clears_send_after_and_keeps_the_draft() {
        let db = seeded_db();
        // Hours out, not a 60s cooldown: this is the schedule discard ruins.
        let id = scheduled_draft(&db, "2030-01-01T09:00:00");

        let result = hold_scheduled(&db, &id, None).unwrap();

        assert_eq!(result["action"], "hold");
        assert_eq!(result["status"], "draft");
        assert_eq!(result["send_after"], serde_json::Value::Null);
        assert_eq!(result["was_scheduled_for"], "2030-01-01T09:00:00");
        assert_eq!(result["discarded"], false);

        let after = db.get_draft(&id).unwrap().unwrap();
        assert!(after.send_after.is_none());
        assert_eq!(after.status.as_str(), "draft");
        assert_eq!(after.subject.as_deref(), Some("Quarterly update"));
        assert_eq!(after.text_content.as_deref(), Some("Body"));
    }

    #[test]
    fn a_held_draft_is_no_longer_listed_as_scheduled() {
        let db = seeded_db();
        let id = scheduled_draft(&db, "2000-01-01T00:00:00");
        assert_eq!(db.list_drafts_due_for_send().unwrap().len(), 1);

        hold_scheduled(&db, &id, None).unwrap();

        assert!(db.list_drafts_due_for_send().unwrap().is_empty());
        // `scheduled list` filters on send_after, so the held draft leaves that
        // listing too — while still being there as a normal draft.
        let listed = db.list_drafts("acc1", Some("draft"), 100, 0).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].send_after.is_none());
    }

    /// The whole reason `hold` exists: `cancel` destroys the message.
    #[test]
    fn hold_keeps_the_draft_where_cancel_would_discard_it() {
        let db = seeded_db();
        let held_id = scheduled_draft(&db, "2030-01-01T09:00:00");
        let cancelled_id = scheduled_draft(&db, "2030-01-01T09:00:00");

        hold_scheduled(&db, &held_id, None).unwrap();
        db.discard_draft(&cancelled_id).unwrap();

        assert_eq!(
            db.get_draft(&held_id).unwrap().unwrap().status.as_str(),
            "draft"
        );
        assert_eq!(
            db.get_draft(&cancelled_id)
                .unwrap()
                .unwrap()
                .status
                .as_str(),
            "discarded"
        );
    }

    #[test]
    fn hold_refuses_a_draft_that_was_never_scheduled() {
        let db = seeded_db();
        let draft = db
            .create_draft(
                "acc1",
                "buyer@example.com",
                Some("Unscheduled"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();

        let err = hold_scheduled(&db, &draft.id, None).unwrap_err();

        assert!(
            err.to_string().contains("failed to hold scheduled draft"),
            "{err:#}"
        );
        assert!(
            format!("{err:#}").contains("not scheduled"),
            "the refusal must say why: {err:#}"
        );
    }

    #[test]
    fn hold_refuses_a_draft_the_send_sweep_already_claimed() {
        let db = seeded_db();
        let id = scheduled_draft(&db, "2000-01-01T00:00:00");
        let rev = db.get_draft(&id).unwrap().unwrap().revision;
        assert!(db.claim_draft_for_sending(&id, rev).unwrap().is_some());

        let err = hold_scheduled(&db, &id, None).unwrap_err();

        assert!(
            format!("{err:#}").contains("not editable"),
            "a claimed send must not be yanked back: {err:#}"
        );
        assert_eq!(
            db.get_draft(&id).unwrap().unwrap().status.as_str(),
            "sending"
        );
    }

    #[test]
    fn hold_refuses_a_missing_draft() {
        let db = seeded_db();
        let err = hold_scheduled(&db, "no-such-draft", None).unwrap_err();
        assert!(err.to_string().contains("draft not found"), "{err:#}");
    }

    #[test]
    fn hold_enforces_the_account_scope_when_one_is_given() {
        let db = seeded_db();
        db.conn().execute("INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port, encrypted_password) VALUES ('acc2', 'Other', 'other@example.com', 'example.com', 'smtp.example.com', 587, 'imap.example.com', 993, 'x')", []).unwrap();
        let id = scheduled_draft(&db, "2030-01-01T09:00:00");

        let err = hold_scheduled(&db, &id, Some("other@example.com")).unwrap_err();

        assert!(err.to_string().contains("does not belong to"), "{err:#}");
        assert!(
            db.get_draft(&id).unwrap().unwrap().send_after.is_some(),
            "a cross-account hold must not clear the schedule"
        );

        // The owning account still holds it.
        hold_scheduled(&db, &id, Some("op@example.com")).unwrap();
        assert!(db.get_draft(&id).unwrap().unwrap().send_after.is_none());
    }
}

/// Take a scheduled message back out of the outbox WITHOUT discarding it.
///
/// The non-destructive counterpart to [`run_cancel`]: `send_after` is cleared
/// so the scheduled-send sweep can never pick the draft up, and the row stays
/// in `draft` status so it can be edited and re-queued later. Reach for this
/// whenever the message is still wanted and only the timing is wrong — cancel
/// throws the message away.
///
/// Unlike [`run_cancel`], `--account` is enforced when supplied: an id that
/// belongs to another account is refused rather than silently held.
fn hold_scheduled(db: &Database, id: &str, account: Option<&str>) -> Result<serde_json::Value> {
    let draft = db
        .get_draft(id)
        .context("failed to get draft")?
        .ok_or_else(|| anyhow::anyhow!("draft not found: {id}"))?;
    let was_scheduled_for = draft.send_after.clone();

    if let Some(a) = account {
        let acct = resolve_account(db, Some(a))?;
        if draft.account_id != acct.id {
            bail!("draft {id} does not belong to account {}", acct.username);
        }
    }

    let held = db
        .hold_scheduled_draft(id)
        .with_context(|| format!("failed to hold scheduled draft {id}"))?;

    Ok(serde_json::json!({
        "action": "hold",
        "draft_id": held.id,
        "account_id": held.account_id,
        "status": held.status.as_str(),
        "to": held.to_addr,
        "subject": held.subject,
        "send_after": held.send_after,
        "was_scheduled_for": was_scheduled_for,
        "discarded": false,
    }))
}

/// CLI entry point for `envelope scheduled hold <id>`.
pub fn run_hold(id: &str, account: Option<&str>, json: bool) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let result = hold_scheduled(&db, id, account)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Held scheduled message: {id}");
        println!("  To:       {}", result["to"].as_str().unwrap_or("-"));
        if let Some(s) = result["subject"].as_str() {
            println!("  Subject:  {s}");
        }
        if let Some(sa) = result["was_scheduled_for"].as_str() {
            println!("  Was scheduled for: {sa}");
        }
        println!("  Status:   draft (editable — re-send when you are ready)");
    }

    Ok(())
}

/// Cancel a scheduled message by discarding the draft.
///
/// Destructive: the draft is gone afterwards. [`run_hold`] is the verb for
/// stopping the clock while keeping the message.
pub fn run_cancel(id: &str, _account: Option<&str>, json: bool) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;

    // Verify the draft exists and has send_after
    let draft = db
        .get_draft(id)
        .context("failed to get draft")?
        .ok_or_else(|| anyhow::anyhow!("draft not found: {id}"))?;

    if draft.send_after.is_none() {
        bail!("draft {id} is not a scheduled message (no send_after set)");
    }

    let discarded = db.discard_draft(id).context("failed to discard draft")?;
    if !discarded {
        bail!(
            "could not cancel draft {id} (status: {})",
            draft.status.as_str()
        );
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "action": "cancel",
                "draft_id": id,
                "to": draft.to_addr,
                "subject": draft.subject,
                "was_scheduled_for": draft.send_after,
            })
        );
    } else {
        println!("Cancelled scheduled message: {id}");
        println!("  To:       {}", draft.to_addr);
        if let Some(ref s) = draft.subject {
            println!("  Subject:  {s}");
        }
        if let Some(ref sa) = draft.send_after {
            println!("  Was scheduled for: {sa}");
        }
    }

    Ok(())
}
