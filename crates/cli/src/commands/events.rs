// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result, bail};
use envelope_email_store::Database;
use envelope_email_store::credential_store::CredentialBackend;
use envelope_email_store::models::Event;
use envelope_email_transport::code_extractor::redact_codes;

use super::common::resolve_account;

pub fn run_list(
    account: Option<&str>,
    limit: usize,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let account_id = match account {
        Some(id_or_email) => Some(resolve_account(&db, Some(id_or_email))?.id),
        None => None,
    };

    let events = db
        .list_events(account_id.as_deref(), limit)
        .context("failed to list events")?
        .into_iter()
        .map(redact_event_for_output)
        .collect::<Vec<_>>();

    if json {
        println!("{}", serde_json::to_string_pretty(&events)?);
        return Ok(());
    }

    if events.is_empty() {
        println!("No events found");
        return Ok(());
    }

    println!(
        "{:<19}  {:<14}  {:<10}  {:<8}  {:<6}  {}",
        "CREATED", "TYPE", "ACCOUNT", "FOLDER", "ACKED", "SUBJECT"
    );
    println!("{}", "-".repeat(96));
    for event in &events {
        let created_at = truncate(&event.created_at, 19);
        let account = truncate(&event.account_id, 10);
        let folder = truncate(&event.folder, 8);
        let acked = if event.acked_at.is_some() {
            "yes"
        } else {
            "no"
        };
        let subject = event
            .subject
            .as_deref()
            .or(event.snippet.as_deref())
            .unwrap_or("-");
        println!(
            "{:<19}  {:<14}  {:<10}  {:<8}  {:<6}  {}",
            created_at,
            truncate(&event.event_type, 14),
            account,
            folder,
            acked,
            truncate(subject, 80)
        );
    }
    println!("\n{} event(s)", events.len());

    Ok(())
}

pub fn run_ack(
    event_id: &str,
    _actor: Option<&str>,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;

    if !db
        .mark_acked(event_id)
        .context("failed to mark event acked")?
    {
        bail!("event not found: {event_id}");
    }

    let event = db
        .get_event(event_id)
        .context("failed to reload event")?
        .ok_or_else(|| anyhow::anyhow!("event not found after ack: {event_id}"))?;
    let event = redact_event_for_output(event);

    if json {
        println!("{}", serde_json::to_string_pretty(&event)?);
    } else {
        println!("Acked event {}", event.id);
        println!("  Type:    {}", event.event_type);
        println!("  Account: {}", event.account_id);
        println!("  Folder:  {}", event.folder);
        println!(
            "  Acked:   {}",
            event.acked_at.as_deref().unwrap_or("(unknown)")
        );
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

fn redact_event_for_output(mut event: Event) -> Event {
    event.subject = event.subject.as_deref().map(redact_codes);
    event.snippet = event.snippet.as_deref().map(redact_codes);
    event.payload = event.payload.as_deref().map(redact_codes);
    event
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_redaction_scrubs_legacy_unredacted_event_rows() {
        let event = Event {
            id: "evt-legacy".to_string(),
            account_id: "acc-1".to_string(),
            event_type: "otp_detected".to_string(),
            folder: "INBOX".to_string(),
            uid: Some(42),
            message_id: Some("<msg@example.com>".to_string()),
            from_addr: Some("noreply@example.com".to_string()),
            subject: Some("Your code is 482910".to_string()),
            snippet: Some("Use 482910 or 482-910 to sign in".to_string()),
            payload: Some(r#"{"debug":"code 482910"}"#.to_string()),
            idempotency_key: Some("same-key".to_string()),
            secure_pending: true,
            acked_at: None,
            created_at: "2026-04-25T12:00:00".to_string(),
        };

        let redacted = redact_event_for_output(event);
        let serialized = serde_json::to_string(&redacted).unwrap();
        assert!(!serialized.contains("482910"));
        assert!(!serialized.contains("482-910"));
        assert_eq!(redacted.subject.as_deref(), Some("Your code is ***"));
    }
}
