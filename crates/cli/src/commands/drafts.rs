// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{bail, Context, Result};
use envelope_email_store::crypto::get_or_create_passphrase;
use envelope_email_store::Database;
use envelope_email_transport::SmtpSender;

use super::common::resolve_account;

pub fn run_list(account: Option<&str>, json: bool) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let acct = resolve_account(&db, account)?;

    let drafts = db
        .list_drafts(&acct.id, Some("draft"), 100, 0)
        .context("failed to list drafts")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&drafts)?);
    } else {
        if drafts.is_empty() {
            println!("No drafts for {}", acct.username);
            return Ok(());
        }

        println!(
            "{:<36}  {:<30}  {:<40}  {}",
            "ID", "TO", "SUBJECT", "UPDATED"
        );
        println!("{}", "-".repeat(110));
        for d in &drafts {
            let subject = d.subject.as_deref().unwrap_or("-");
            let subject_display = if subject.len() > 38 {
                format!("{}...", &subject[..38])
            } else {
                subject.to_string()
            };
            let to_display = if d.to_addr.len() > 28 {
                format!("{}...", &d.to_addr[..28])
            } else {
                d.to_addr.clone()
            };
            println!(
                "{:<36}  {:<30}  {:<40}  {}",
                d.id, to_display, subject_display, d.updated_at,
            );
        }
        println!("\n{} draft(s)", drafts.len());
    }

    Ok(())
}

pub fn run_create(
    to: &str,
    subject: Option<&str>,
    body: Option<&str>,
    account: Option<&str>,
    json: bool,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let acct = resolve_account(&db, account)?;

    let draft = db
        .create_draft(
            &acct.id,
            to,
            subject,
            body,
            None, // html_content
            None, // in_reply_to
            None, // cc_addr
            None, // bcc_addr
            Some("cli"),
        )
        .context("failed to create draft")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&draft)?);
    } else {
        println!("Draft created: {}", draft.id);
        println!("  To:      {}", draft.to_addr);
        if let Some(ref s) = draft.subject {
            println!("  Subject: {s}");
        }
    }

    Ok(())
}

#[tokio::main]
pub async fn run_send(id: &str, account: Option<&str>, json: bool) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let passphrase = get_or_create_passphrase().context("keychain error")?;

    let draft = db
        .get_draft(id)
        .context("failed to get draft")?
        .ok_or_else(|| anyhow::anyhow!("draft not found: {id}"))?;

    // Resolve account from draft's account_id (override with --account if given)
    let acct = match account {
        Some(a) => resolve_account(&db, Some(a))?,
        None => db
            .get_account(&draft.account_id)
            .context("database error")?
            .ok_or_else(|| anyhow::anyhow!("account not found for draft: {}", draft.account_id))?,
    };

    let creds = db
        .get_account_with_credentials(&acct.id, &passphrase)
        .context("failed to decrypt credentials")?;

    let subject = draft.subject.as_deref().unwrap_or("");
    let message_id = SmtpSender::send(
        &creds,
        &draft.to_addr,
        subject,
        draft.text_content.as_deref(),
        draft.html_content.as_deref(),
        draft.cc_addr.as_deref(),
        draft.bcc_addr.as_deref(),
        draft.reply_to.as_deref(),
    )
    .await
    .context("failed to send draft")?;

    db.mark_draft_sent(id, Some(&message_id))
        .context("failed to mark draft as sent")?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": "sent",
                "draft_id": id,
                "to": draft.to_addr,
                "message_id": message_id,
            })
        );
    } else {
        println!("Draft {id} sent to {}", draft.to_addr);
        println!("Message-ID: {message_id}");
    }

    Ok(())
}

pub fn run_discard(id: &str, json: bool) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;

    let discarded = db.discard_draft(id).context("failed to discard draft")?;

    if !discarded {
        bail!("draft not found or not discardable: {id}");
    }

    if json {
        println!(
            "{}",
            serde_json::json!({ "action": "discard", "draft_id": id })
        );
    } else {
        println!("Draft {id} discarded");
    }

    Ok(())
}
