// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result, bail};
use envelope_email_store::Database;
use envelope_email_store::action_log::EventActionLogInput;
use envelope_email_store::credential_store::CredentialBackend;
use envelope_email_transport::rule_exec::{
    self, ActionAttribution, ActionSource, ImapRuleMailbox, OfferReport,
};

use super::common::{resolve_account, setup_credentials};

pub fn run_tail(
    limit: u32,
    account: Option<&str>,
    agent: Option<&str>,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let actions = match (agent, account) {
        // An agent's trail should not depend on which mailbox it acted in: with
        // no --account, show everything attributed to it across accounts.
        (Some(agent_ref), None) => {
            let agent_id = resolve_agent_id(&db, agent_ref)?;
            db.list_actions_for_agent_any_account(&agent_id, limit)
                .context("failed to list actions")?
        }
        (Some(agent_ref), Some(_)) => {
            let acct = resolve_account(&db, account)?;
            let agent_id = resolve_agent_id(&db, agent_ref)?;
            db.list_actions_for_agent(&acct.id, &agent_id, limit)
                .context("failed to list actions")?
        }
        (None, _) => {
            let acct = resolve_account(&db, account)?;
            db.list_actions(&acct.id, limit)
                .context("failed to list actions")?
        }
    };

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

/// `envelope actions confirm <event_id>` — execute a pending offer's actions.
///
/// Idempotent: every action is keyed in the action log, so confirming twice
/// runs nothing twice. A dismissed offer is refused.
#[tokio::main]
pub async fn run_confirm(event_id: &str, json: bool, backend: CredentialBackend) -> Result<()> {
    let account_id = {
        let db = Database::open_default().context("failed to open database")?;
        db.get_event(event_id)
            .context("failed to load event")?
            .ok_or_else(|| anyhow::anyhow!("event not found: {event_id}"))?
            .account_id
    };
    let (db, creds) = setup_credentials(Some(&account_id), backend)?;
    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;
    let mut mbox = ImapRuleMailbox {
        client: &mut client,
        db: &db,
        account_id: &account_id,
    };
    let report = rule_exec::confirm_offer(
        &mut mbox,
        &db,
        event_id,
        &creds.account.username,
        &ActionAttribution::new(ActionSource::Cli),
    )
    .await?;
    print_offer_report(&report, json)
}

/// `envelope actions dismiss <event_id>` — close a pending offer unexecuted.
pub fn run_dismiss(event_id: &str, json: bool, _backend: CredentialBackend) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let report =
        rule_exec::dismiss_offer(&db, event_id, &ActionAttribution::new(ActionSource::Cli))?;
    print_offer_report(&report, json)
}

fn print_offer_report(report: &OfferReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }
    let verb = match report.resolution {
        rule_exec::OfferResolution::Confirmed => "Confirmed",
        rule_exec::OfferResolution::Dismissed => "Dismissed",
    };
    let note = if report.changed {
        ""
    } else {
        " (already done; nothing changed)"
    };
    println!("{verb} offer {}{note}", report.event_id);
    for action in &report.actions {
        println!(
            "  {} {} -> {}",
            action["status"].as_str().unwrap_or("?"),
            action["action"],
            action["result"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

/// Resolve an `--agent` reference (name or id) to an agent id. Accepts the id
/// directly when no name matches, so callers can filter by either.
fn resolve_agent_id(db: &Database, agent_ref: &str) -> Result<String> {
    if let Some(agent) = db
        .get_agent_by_name(agent_ref)
        .context("failed to resolve agent by name")?
    {
        return Ok(agent.id);
    }
    if db
        .get_agent_by_id(agent_ref)
        .context("failed to resolve agent by id")?
        .is_some()
    {
        return Ok(agent_ref.to_string());
    }
    bail!("agent not found: {agent_ref}")
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
