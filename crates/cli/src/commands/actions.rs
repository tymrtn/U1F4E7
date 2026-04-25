// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result, bail};
use envelope_email_store::Database;
use envelope_email_store::action_log::EventActionLogInput;
use envelope_email_store::credential_store::CredentialBackend;

use super::common::resolve_account;

pub fn run_tail(
    limit: u32,
    account: Option<&str>,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let acct = resolve_account(&db, account)?;
    let actions = db
        .list_actions(&acct.id, limit)
        .context("failed to list actions")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&actions)?);
        return Ok(());
    }

    if actions.is_empty() {
        println!("No actions found");
        return Ok(());
    }

    println!(
        "{:<19}  {:<14}  {:<12}  {:<10}  {}",
        "CREATED", "TYPE", "STATUS", "EVENT", "ACTION"
    );
    println!("{}", "-".repeat(96));
    for action in &actions {
        println!(
            "{:<19}  {:<14}  {:<12}  {:<10}  {}",
            truncate(&action.created_at, 19),
            truncate(&action.action_type, 14),
            truncate(&action.action_status, 12),
            truncate(action.event_id.as_deref().unwrap_or("-"), 10),
            truncate(&action.action_taken, 80)
        );
    }
    println!("\n{} action(s)", actions.len());

    Ok(())
}

pub fn run_exec_mark_handled(
    event_id: &str,
    actor: &str,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let event = db
        .get_event(event_id)
        .context("failed to load event")?
        .ok_or_else(|| anyhow::anyhow!("event not found: {event_id}"))?;

    if actor.trim().is_empty() {
        bail!("actor is required");
    }

    let action_taken = serde_json::json!({
        "kind": "mark_handled",
        "actor": actor,
        "mode": "local_audit_only",
    })
    .to_string();

    let action = db
        .log_action_for_event(EventActionLogInput {
            account_id: &event.account_id,
            event_id: &event.id,
            action_type: "mark_handled",
            confidence: 1.0,
            justification: "mark-handled executed locally; no mailbox mutation",
            action_taken: &action_taken,
            action_status: "completed",
            message_id: event.message_id.as_deref(),
            draft_id: None,
        })
        .context("failed to record action")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&action)?);
    } else {
        println!("Recorded action {}", action.id);
        println!("  Event:   {}", event.id);
        println!("  Actor:   {actor}");
        println!("  Type:    {}", action.action_type);
        println!("  Status:  {}", action.action_status);
    }

    Ok(())
}

fn truncate(value: &str, max_len: usize) -> String {
    if value.chars().count() <= max_len {
        return value.to_string();
    }
    value
        .chars()
        .take(max_len.saturating_sub(3))
        .collect::<String>()
        + "..."
}
