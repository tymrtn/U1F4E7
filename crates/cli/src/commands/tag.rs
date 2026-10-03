// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result, bail};
use envelope_email_store::Database;
use envelope_email_store::credential_store::CredentialBackend;
use envelope_email_store::models::{MessageScore, MessageTag};
use envelope_email_transport::imap;
use envelope_email_transport::threat::{self, persist};

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

    let message_id = set_tags(
        &db,
        account_id,
        folder,
        uid,
        msg.message_id.as_deref(),
        tags,
        &parsed_scores,
    )?;

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

    let (msg, raw) = imap::fetch_message_with_raw(&mut client, folder, uid)
        .await
        .context("failed to fetch message")?
        .ok_or_else(|| anyhow::anyhow!("message UID {uid} not found in {folder}"))?;

    let (message_id, tags, scores) = shown(
        &db,
        account_id,
        folder,
        uid,
        msg.message_id.as_deref(),
        raw.as_deref(),
    )?;

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

/// Apply `tags` and `scores` to the message at folder/UID under its tag key,
/// and return the key.
fn set_tags<'a>(
    db: &Database,
    account_id: &str,
    folder: &str,
    uid: u32,
    message_id: Option<&'a str>,
    tags: &[String],
    scores: &[(String, f64)],
) -> Result<&'a str> {
    let message_id = tag_key(message_id, folder, uid)?;
    for tag in tags {
        db.add_tag(account_id, message_id, tag, Some(uid as i64), Some(folder))
            .with_context(|| format!("failed to add tag '{tag}'"))?;
    }
    for (dim, val) in scores {
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
    Ok(message_id)
}

/// The key `tag set` and `tag show` use: the message's Message-ID, when tags
/// may be keyed by it ([`threat::usable_message_id`]).
fn tag_key<'a>(message_id: Option<&'a str>, folder: &str, uid: u32) -> Result<&'a str> {
    let message_id =
        message_id.ok_or_else(|| anyhow::anyhow!("message UID {uid} has no Message-ID header"))?;
    threat::usable_message_id(message_id).ok_or_else(|| {
        anyhow::anyhow!("message UID {uid} in {folder} has no usable Message-ID ({message_id})")
    })
}

/// The Message-ID `tag show` reads under, and the tags and scores it lists
/// for the message at folder/UID fetched as `raw`.
fn shown<'a>(
    db: &Database,
    account_id: &str,
    folder: &str,
    uid: u32,
    message_id: Option<&'a str>,
    raw: Option<&[u8]>,
) -> Result<(&'a str, Vec<MessageTag>, Vec<MessageScore>)> {
    let message_id = tag_key(message_id, folder, uid)?;
    let (tags, scores) =
        persist::shown_tags_and_scores(db, account_id, folder, uid, message_id, raw)
            .context("failed to get tags and scores")?;
    Ok((message_id, tags, scores))
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
pub(crate) mod tests {
    use super::*;
    use envelope_email_transport::threat::ThreatConfig;
    use envelope_email_transport::threat::persist::VerdictTarget;

    pub(crate) const ACCT: &str = "acct-1";

    /// Scans a message at INBOX UID 1 and marks it safe, then opens another
    /// message with its Message-ID (`twin@x`) at UID 2. Returns the second
    /// message's bytes.
    pub(crate) fn marked_original_and_twin(db: &Database) -> Vec<u8> {
        let message = |body: &str| {
            format!(
                "From: Alice <alice@partner.example>\r\nTo: me@example.org\r\n\
                 Subject: Lunch\r\nMessage-ID: <twin@x>\r\n\r\n{body}\r\n"
            )
            .into_bytes()
        };
        let (original, twin) = (message("Thursday?"), message("Friday?"));
        let config = ThreatConfig::default();
        for (uid, raw) in [(1, &original), (2, &twin)] {
            persist::verdict_on_open(db, ACCT, "me@example.org", "INBOX", uid, Some(raw), &config)
                .unwrap();
            if uid == 1 {
                let fingerprint = envelope_email_transport::threat::content_fingerprint(raw);
                persist::mark_safe(
                    db,
                    &VerdictTarget {
                        account_id: ACCT,
                        folder: "INBOX",
                        uid,
                        message_id: Some("twin@x"),
                        content_fingerprint: fingerprint.as_deref(),
                        observed_message_ids: &[],
                    },
                    "cli",
                    None,
                )
                .unwrap();
            }
        }
        twin
    }

    #[test]
    fn tag_show_does_not_give_a_twin_the_marked_original_s_threat_tags() {
        let db = Database::open_memory().unwrap();
        let twin = marked_original_and_twin(&db);
        let names = |uid: u32, raw: Option<&[u8]>| -> Vec<String> {
            let (_, tags, _) = shown(&db, ACCT, "INBOX", uid, Some("twin@x"), raw).unwrap();
            tags.into_iter().map(|t| t.tag).collect()
        };
        let false_positive = "threat:false_positive".to_string();
        assert!(!names(2, Some(&twin)).contains(&false_positive));
        assert!(!names(2, None).contains(&false_positive));
        assert!(names(1, None).contains(&false_positive));
    }

    /// Another message, which a reused UID 1 may now hold.
    pub(crate) const OTHER: &[u8] = b"From: Bob <bob@example.test>\r\nTo: me@example.org\r\n\
Subject: Hi\r\nMessage-ID: <other@x>\r\n\r\nYo\r\n";

    #[test]
    fn tag_show_gives_a_reused_uid_none_of_the_old_message_s_threat_tags() {
        let db = Database::open_memory().unwrap();
        let twin = marked_original_and_twin(&db);
        // UID 1 now holds another message; the verdict there is the marked
        // original's.
        let threat_tags = |message_id: &str, raw: Option<&[u8]>| -> Vec<String> {
            let (_, tags, _) = shown(&db, ACCT, "INBOX", 1, Some(message_id), raw).unwrap();
            let names = tags.into_iter().map(|t| t.tag);
            names.filter(|t| t.starts_with("threat:")).collect()
        };
        assert_eq!(threat_tags("twin@x", Some(&twin)), Vec::<String>::new());
        assert_eq!(threat_tags("other@x", Some(OTHER)), Vec::<String>::new());
        assert_eq!(threat_tags("other@x", None), Vec::<String>::new());
    }

    /// The `fp:` key some content's verdict would be stored under, with a
    /// tag on it. Returns the key.
    pub(crate) fn fingerprint_key_with_a_tag(db: &Database) -> String {
        let raw = b"From: a@example.test\r\nTo: me@example.org\r\nSubject: s\r\n\
                    Message-ID: <a@x>\r\n\r\nhi\r\n";
        let fingerprint = envelope_email_transport::threat::content_fingerprint(raw).unwrap();
        let key = format!("fp:{fingerprint}");
        db.add_tag(ACCT, &key, "vip", Some(1), Some("INBOX"))
            .unwrap();
        key
    }

    fn tag_names(db: &Database, key: &str) -> Vec<String> {
        let tags = db.get_tags(ACCT, key).unwrap();
        tags.into_iter().map(|t| t.tag).collect()
    }

    #[test]
    fn tag_set_never_writes_under_a_message_id_shaped_like_a_fingerprint_key() {
        let db = Database::open_memory().unwrap();
        let key = fingerprint_key_with_a_tag(&db);
        let set = set_tags(
            &db,
            ACCT,
            "INBOX",
            5,
            Some(&key),
            &["urgent".to_string()],
            &[("priority".to_string(), 1.0)],
        );
        assert!(set.is_err(), "{set:?}");
        assert_eq!(tag_names(&db, &key), ["vip"]);
        assert!(db.get_scores(ACCT, &key).unwrap().is_empty());
    }

    #[test]
    fn tag_show_never_reads_under_a_message_id_shaped_like_a_fingerprint_key() {
        let db = Database::open_memory().unwrap();
        let key = fingerprint_key_with_a_tag(&db);
        let shown = shown(&db, ACCT, "INBOX", 5, Some(&key), None);
        assert!(shown.is_err(), "{:?}", shown.map(|(_, tags, _)| tags));
    }

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
