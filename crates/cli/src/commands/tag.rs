// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result, bail};
use envelope_email_store::credential_store::CredentialBackend;
use envelope_email_transport::imap;

use super::common::setup_credentials;

/// Parse a `key=value` score pair (e.g. `urgent=0.9`).
fn parse_score(s: &str) -> Result<(String, f64)> {
    let (key, val) = s
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("invalid score format '{s}' — expected key=value"))?;
    let value: f64 = val
        .parse()
        .with_context(|| format!("cannot parse score value '{val}' as a number"))?;
    Ok((key.to_string(), value))
}

/// `envelope tag set <uid>` — apply tags and scores to a message.
#[allow(clippy::too_many_arguments)]
#[tokio::main]
pub async fn run_set(
    uid: u32,
    folder: &str,
    scores: &[String],
    tags: &[String],
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    if scores.is_empty() && tags.is_empty() {
        bail!("provide at least one --score or --tag");
    }

    let parsed_scores: Vec<(String, f64)> = scores
        .iter()
        .map(|s| parse_score(s))
        .collect::<Result<Vec<_>>>()?;

    let (db, creds) = setup_credentials(account, backend)?;
    let account_id = &creds.account.id;

    // Fetch message to resolve UID -> Message-ID (stable key for tagging)
    let mut client = imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    let msg = imap::fetch_message(&mut client, folder, uid)
        .await
        .context("failed to fetch message")?
        .ok_or_else(|| anyhow::anyhow!("message UID {uid} not found in {folder}"))?;

    let message_id = msg
        .message_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("message UID {uid} has no Message-ID header"))?;

    // Apply tags
    for tag in tags {
        db.add_tag(account_id, message_id, tag, Some(uid as i64), Some(folder))
            .with_context(|| format!("failed to add tag '{tag}'"))?;
    }

    // Apply scores
    for (dim, val) in &parsed_scores {
        db.set_score(
            account_id,
            message_id,
            dim,
            *val,
            Some(uid as i64),
            Some(folder),
        )
        .with_context(|| format!("failed to set score '{dim}'"))?;
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "action": "tag_set",
                "uid": uid,
                "folder": folder,
                "message_id": message_id,
                "tags_added": tags,
                "scores_set": parsed_scores.iter()
                    .map(|(k, v)| serde_json::json!({"dimension": k, "value": v}))
                    .collect::<Vec<_>>(),
            })
        );
    } else {
        println!("Tagged UID {uid} ({folder})");
        println!("  Message-ID: {message_id}");
        for tag in tags {
            println!("  +tag: {tag}");
        }
        for (dim, val) in &parsed_scores {
            println!("  +score: {dim} = {val}");
        }
    }

    Ok(())
}

/// `envelope tag show <uid>` — show all tags and scores for a message.
#[tokio::main]
pub async fn run_show(
    uid: u32,
    folder: &str,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;
    let account_id = &creds.account.id;

    // Fetch message to get Message-ID
    let mut client = imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    let msg = imap::fetch_message(&mut client, folder, uid)
        .await
        .context("failed to fetch message")?
        .ok_or_else(|| anyhow::anyhow!("message UID {uid} not found in {folder}"))?;

    let message_id = msg
        .message_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("message UID {uid} has no Message-ID header"))?;

    let tags = db
        .get_tags(account_id, message_id)
        .context("failed to get tags")?;
    let scores = db
        .get_scores(account_id, message_id)
        .context("failed to get scores")?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "uid": uid,
                "folder": folder,
                "message_id": message_id,
                "subject": msg.subject,
                "from": msg.from_addr,
                "tags": tags.iter().map(|t| &t.tag).collect::<Vec<_>>(),
                "scores": scores.iter().map(|s| serde_json::json!({
                    "dimension": s.dimension,
                    "value": s.value,
                    "updated_at": s.updated_at,
                })).collect::<Vec<_>>(),
            }))?
        );
    } else {
        println!("UID {uid} ({folder})");
        println!("  From:       {}", msg.from_addr);
        println!("  Subject:    {}", msg.subject);
        println!("  Message-ID: {message_id}");
        if tags.is_empty() {
            println!("  Tags:       (none)");
        } else {
            let tag_names: Vec<&str> = tags.iter().map(|t| t.tag.as_str()).collect();
            println!("  Tags:       {}", tag_names.join(", "));
        }
        if scores.is_empty() {
            println!("  Scores:     (none)");
        } else {
            for s in &scores {
                println!("  Score:      {} = {:.2}", s.dimension, s.value);
            }
        }
    }

    Ok(())
}

/// `envelope tag list` — list messages matching a tag or minimum score filter.
#[allow(clippy::too_many_arguments)]
pub fn run_list(
    tag_filter: Option<&str>,
    min_scores: &[String],
    account: Option<&str>,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = envelope_email_store::Database::open_default().context("failed to open database")?;

    let acct = super::common::resolve_account(&db, account)?;
    let account_id = &acct.id;

    if tag_filter.is_none() && min_scores.is_empty() {
        bail!("provide --tag <name> or --min-score <dim>=<value>");
    }

    let mut results: Vec<serde_json::Value> = Vec::new();

    // Tag filter
    if let Some(tag) = tag_filter {
        let tagged = db
            .list_messages_with_tag(account_id, tag)
            .context("failed to list tagged messages")?;
        for t in &tagged {
            results.push(serde_json::json!({
                "message_id": t.message_id,
                "tag": t.tag,
                "uid": t.uid,
                "folder": t.folder,
                "match_type": "tag",
            }));
        }
    }

    // Score filters
    for score_spec in min_scores {
        let (dim, min_val) = parse_score(score_spec)?;
        let scored = db
            .list_messages_with_min_score(account_id, &dim, min_val)
            .with_context(|| format!("failed to list messages by score '{dim}'"))?;
        for s in &scored {
            results.push(serde_json::json!({
                "message_id": s.message_id,
                "dimension": s.dimension,
                "value": s.value,
                "uid": s.uid,
                "folder": s.folder,
                "match_type": "score",
            }));
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        if results.is_empty() {
            println!("No matching messages");
            return Ok(());
        }

        println!(
            "{:<40}  {:<12}  {:<8}  {:<10}  {}",
            "MESSAGE-ID", "MATCH", "UID", "FOLDER", "DETAIL"
        );
        println!("{}", "-".repeat(90));
        for r in &results {
            let mid = r["message_id"].as_str().unwrap_or("-");
            let mid_display = if mid.len() > 38 {
                format!("{}...", &mid[..35])
            } else {
                mid.to_string()
            };
            let match_type = r["match_type"].as_str().unwrap_or("-");
            let uid = r["uid"]
                .as_i64()
                .map(|u| u.to_string())
                .unwrap_or_else(|| "-".to_string());
            let folder = r["folder"].as_str().unwrap_or("-");
            let detail = if match_type == "tag" {
                r["tag"].as_str().unwrap_or("-").to_string()
            } else {
                format!(
                    "{} >= {:.2}",
                    r["dimension"].as_str().unwrap_or("?"),
                    r["value"].as_f64().unwrap_or(0.0)
                )
            };
            println!(
                "{:<40}  {:<12}  {:<8}  {:<10}  {}",
                mid_display, match_type, uid, folder, detail
            );
        }
        println!("\n{} result(s)", results.len());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_score_valid() {
        let (k, v) = parse_score("urgent=0.9").unwrap();
        assert_eq!(k, "urgent");
        assert!((v - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_score_integer() {
        let (k, v) = parse_score("priority=5").unwrap();
        assert_eq!(k, "priority");
        assert!((v - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_score_invalid_format() {
        assert!(parse_score("noequals").is_err());
    }

    #[test]
    fn parse_score_invalid_value() {
        assert!(parse_score("urgent=abc").is_err());
    }
}
