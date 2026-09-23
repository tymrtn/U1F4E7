// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! `envelope analytics`: what the local install has observed about a message.
//! Local reads only; no IMAP connection.

use anyhow::{Context, Result};
use envelope_email_store::Database;
use envelope_email_store::models::Event;
use serde_json::{Value, json};

use super::common::resolve_account;

/// Shared by the JSON note and the contract so the two never drift.
pub const SEEN_BY_NOTE: &str = "observed_at is when Envelope saw \\Seen set by another client; the read happened at or before that time, never after. It is not the read time.";

pub fn run_show(uid: u32, folder: &str, account: Option<&str>, json: bool) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let account = resolve_account(&db, account)?;
    let events = db
        .list_message_seen_events(&account.id, folder, uid)
        .context("failed to read message_seen events")?;

    if json {
        let output = show_json(&account.id, folder, uid, &events)?;
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    if events.is_empty() {
        println!("UID {uid} in {folder}: no read on another client observed");
        return Ok(());
    }
    for event in &events {
        let payload = payload(event)?;
        println!(
            "UID {uid} in {folder}: {} (observed via {})",
            seen_by_line(&event.created_at)?,
            payload["source"].as_str().unwrap_or("unknown")
        );
    }
    Ok(())
}

fn payload(event: &Event) -> Result<Value> {
    let raw = event
        .payload
        .as_deref()
        .with_context(|| format!("message_seen event {} has no payload", event.id))?;
    serde_json::from_str(raw)
        .with_context(|| format!("message_seen event {} payload is not JSON", event.id))
}

/// "seen by 2026-09-23 08:12", in local time.
fn seen_by_line(observed_at: &str) -> Result<String> {
    let at = chrono::DateTime::parse_from_rfc3339(observed_at)
        .with_context(|| format!("unreadable observed_at {observed_at:?}"))?
        .with_timezone(&chrono::Local);
    Ok(format!("seen by {}", at.format("%Y-%m-%d %H:%M")))
}

fn show_json(account_id: &str, folder: &str, uid: u32, events: &[Event]) -> Result<Value> {
    let seen = events
        .iter()
        .map(|event| {
            let payload = payload(event)?;
            Ok(json!({
                "event_id": event.id,
                "observed_at": event.created_at,
                "source": payload["source"],
                "message_id": event.message_id,
                "uidvalidity": payload["uidvalidity"],
                "label": seen_by_line(&event.created_at)?,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "account_id": account_id,
        "folder": folder,
        "uid": uid,
        "seen": seen,
        "note": SEEN_BY_NOTE,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use envelope_email_store::models::IndexedMessageInput;

    fn msg(uid: u32, flags: &[&str]) -> IndexedMessageInput {
        IndexedMessageInput {
            uid,
            message_id: Some(format!("<{uid}@x>")),
            from_addr: "p@example.test".into(),
            to_addr: "me@example.test".into(),
            subject: "s".into(),
            date: None,
            flags: flags.iter().map(|f| f.to_string()).collect(),
            size: 1,
            snippet: None,
            thread_id: None,
        }
    }

    #[test]
    fn show_json_reports_observation_time_with_seen_by_wording() {
        let db = Database::open_memory().unwrap();
        db.upsert_indexed_message_summaries("acc", "INBOX", 9, &[msg(3, &[])])
            .unwrap();
        db.upsert_indexed_message_summaries("acc", "INBOX", 9, &[msg(3, &["Seen"])])
            .unwrap();
        let events = db.list_message_seen_events("acc", "INBOX", 3).unwrap();

        let out = show_json("acc", "INBOX", 3, &events).unwrap();
        assert_eq!(out["uid"], 3);
        let seen = out["seen"].as_array().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["source"], "index_refresh");
        assert_eq!(seen[0]["message_id"], "<3@x>");
        assert_eq!(seen[0]["uidvalidity"], 9);
        assert_eq!(seen[0]["observed_at"], events[0].created_at.as_str());
        assert!(seen[0]["label"].as_str().unwrap().starts_with("seen by "));
        assert!(out["note"].as_str().unwrap().contains("not the read time"));
    }

    #[test]
    fn seen_by_line_rejects_garbage_loudly() {
        assert!(seen_by_line("yesterday-ish").is_err());
        assert!(
            seen_by_line("2026-09-23T08:12:00+00:00")
                .unwrap()
                .starts_with("seen by 2026-09-2")
        );
    }
}
