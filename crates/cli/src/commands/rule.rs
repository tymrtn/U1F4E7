// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use envelope_email_store::credential_store::CredentialBackend;
use envelope_email_transport::imap;
use envelope_email_transport::rules::{self, Action, MessageContext};
use tracing::info;

use super::common::setup_credentials;

/// Parse a `key=value` score pair (e.g. `urgent=0.7`).
fn parse_score_filter(s: &str) -> Result<(String, f64)> {
    let (key, val) = s
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("invalid score format '{s}' — expected key=value"))?;
    let value: f64 = val
        .parse()
        .with_context(|| format!("cannot parse score value '{val}' as a number"))?;
    Ok((key.to_string(), value))
}

/// Parse a `type=arg` action pair (e.g. `move=Archive`, `flag=seen`, `delete`).
fn parse_action(s: &str) -> Result<Action> {
    if let Some((kind, arg)) = s.split_once('=') {
        match kind.to_lowercase().as_str() {
            "move" => Ok(Action::Move(arg.to_string())),
            "flag" => Ok(Action::Flag(arg.to_string())),
            "unflag" => Ok(Action::Unflag(arg.to_string())),
            "snooze" => Ok(Action::Snooze(arg.to_string())),
            "add_tag" | "addtag" | "tag" => Ok(Action::AddTag(arg.to_string())),
            _ => bail!("unknown action type '{kind}'. Use: move, flag, unflag, snooze, tag, delete, unsubscribe"),
        }
    } else {
        match s.to_lowercase().as_str() {
            "delete" => Ok(Action::Delete),
            "unsubscribe" => Ok(Action::Unsubscribe),
            _ => bail!("unknown action '{s}'. Use: move=<folder>, flag=<name>, delete, unsubscribe"),
        }
    }
}

/// Build a `MessageContext` from a fetched message + its tags/scores in the store.
fn build_message_context(
    msg: &envelope_email_store::Message,
    db: &envelope_email_store::Database,
    account_id: &str,
) -> Result<MessageContext> {
    let message_id = msg.message_id.as_deref().unwrap_or("");

    let tags: Vec<String> = if !message_id.is_empty() {
        db.get_tags(account_id, message_id)
            .context("failed to get tags")?
            .into_iter()
            .map(|t| t.tag)
            .collect()
    } else {
        vec![]
    };

    let scores: HashMap<String, f64> = if !message_id.is_empty() {
        db.get_scores(account_id, message_id)
            .context("failed to get scores")?
            .into_iter()
            .map(|s| (s.dimension, s.value))
            .collect()
    } else {
        HashMap::new()
    };

    Ok(MessageContext {
        from_addr: msg.from_addr.clone(),
        to_addr: msg.to_addr.clone(),
        subject: msg.subject.clone(),
        tags,
        scores,
    })
}

/// `envelope rule create` — create a new rule.
#[allow(clippy::too_many_arguments)]
pub fn run_create(
    name: &str,
    match_from: Option<&str>,
    match_to: Option<&str>,
    match_subject: Option<&str>,
    match_tags: &[String],
    match_score_above: &[String],
    match_score_below: &[String],
    action_str: &str,
    priority: i64,
    stop: bool,
    account: Option<&str>,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = envelope_email_store::Database::open_default().context("failed to open database")?;
    let acct = super::common::resolve_account(&db, account)?;
    let account_id = &acct.id;

    // Parse score filters
    let score_above: Vec<(String, f64)> = match_score_above
        .iter()
        .map(|s| parse_score_filter(s))
        .collect::<Result<Vec<_>>>()?;
    let score_below: Vec<(String, f64)> = match_score_below
        .iter()
        .map(|s| parse_score_filter(s))
        .collect::<Result<Vec<_>>>()?;

    // Build the match expression from CLI flags
    let match_expr = rules::build_match_expr(
        match_from,
        match_to,
        match_subject,
        match_tags,
        &score_above,
        &score_below,
    );
    let match_expr_json =
        serde_json::to_string(&match_expr).context("failed to serialize match expression")?;

    // Parse and serialize the action
    let action = parse_action(action_str)?;
    let action_json = serde_json::to_string(&action).context("failed to serialize action")?;

    // Check for duplicate name
    if db
        .find_rule_by_name(account_id, name)
        .context("database error")?
        .is_some()
    {
        bail!("a rule named '{name}' already exists for this account");
    }

    let rule = db
        .create_rule(account_id, name, &match_expr_json, &action_json, priority, stop)
        .context("failed to create rule")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&rule)?);
    } else {
        println!("Created rule: {}", rule.name);
        println!("  ID:       {}", rule.id);
        println!("  Priority: {}", rule.priority);
        println!("  Stop:     {}", rule.stop);
        println!("  Sieve:    {}", if rule.sieve_exportable { "yes" } else { "no" });
        println!("  Match:    {match_expr_json}");
        println!("  Action:   {action_json}");
    }

    Ok(())
}

/// `envelope rule list` — list all rules for an account.
pub fn run_list(
    account: Option<&str>,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = envelope_email_store::Database::open_default().context("failed to open database")?;
    let acct = super::common::resolve_account(&db, account)?;

    let rules = db
        .list_rules(&acct.id)
        .context("failed to list rules")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&rules)?);
    } else {
        if rules.is_empty() {
            println!("No rules configured");
            return Ok(());
        }

        println!(
            "{:<8}  {:<3}  {:<5}  {:<4}  {:<6}  {:<30}  {}",
            "PRI", "ON", "STOP", "HITS", "SIEVE", "NAME", "ACTION"
        );
        println!("{}", "-".repeat(90));
        for r in &rules {
            let enabled_mark = if r.enabled { "yes" } else { "no" };
            let stop_mark = if r.stop { "yes" } else { "no" };
            let sieve_mark = if r.sieve_exportable { "yes" } else { "no" };
            let name_display = if r.name.len() > 28 {
                format!("{}...", &r.name[..25])
            } else {
                r.name.clone()
            };
            println!(
                "{:<8}  {:<3}  {:<5}  {:<4}  {:<6}  {:<30}  {}",
                r.priority, enabled_mark, stop_mark, r.hit_count, sieve_mark, name_display, r.action,
            );
        }
        println!("\n{} rule(s)", rules.len());
    }

    Ok(())
}

/// `envelope rule test <uid>` — dry-run all rules against a single message.
#[tokio::main]
pub async fn run_test(
    uid: u32,
    folder: &str,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;
    let account_id = creds.account.id.clone();

    let mut client = imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    let msg = imap::fetch_message(&mut client, folder, uid)
        .await
        .context("failed to fetch message")?
        .ok_or_else(|| anyhow::anyhow!("message UID {uid} not found in {folder}"))?;

    let ctx = build_message_context(&msg, &db, &account_id)?;

    let enabled_rules = db
        .list_enabled_rules(&account_id)
        .context("failed to list enabled rules")?;

    let mut matches: Vec<serde_json::Value> = Vec::new();

    for rule in &enabled_rules {
        let match_expr: rules::MatchExpr = serde_json::from_str(&rule.match_expr)
            .with_context(|| format!("invalid match_expr in rule '{}'", rule.name))?;

        let matched = rules::evaluate(&match_expr, &ctx);
        if matched {
            matches.push(serde_json::json!({
                "rule_id": rule.id,
                "rule_name": rule.name,
                "priority": rule.priority,
                "action": rule.action,
                "stop": rule.stop,
            }));

            if rule.stop {
                break;
            }
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "uid": uid,
                "folder": folder,
                "subject": msg.subject,
                "from": msg.from_addr,
                "tags": ctx.tags,
                "scores": ctx.scores,
                "rules_evaluated": enabled_rules.len(),
                "matches": matches,
            }))?
        );
    } else {
        println!("Testing UID {uid} ({folder})");
        println!("  From:    {}", msg.from_addr);
        println!("  Subject: {}", msg.subject);
        println!("  Tags:    {}", if ctx.tags.is_empty() { "(none)".to_string() } else { ctx.tags.join(", ") });
        println!(
            "  Scores:  {}",
            if ctx.scores.is_empty() {
                "(none)".to_string()
            } else {
                ctx.scores
                    .iter()
                    .map(|(k, v)| format!("{k}={v:.2}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
        println!();

        if matches.is_empty() {
            println!("No rules matched ({} evaluated)", enabled_rules.len());
        } else {
            println!("{} rule(s) matched:", matches.len());
            for m in &matches {
                let name = m["rule_name"].as_str().unwrap_or("?");
                let action = m["action"].as_str().unwrap_or("?");
                let stop = m["stop"].as_bool().unwrap_or(false);
                let stop_marker = if stop { " [STOP]" } else { "" };
                println!("  - {name} -> {action}{stop_marker}");
            }
        }
    }

    Ok(())
}

/// `envelope rule run` — batch apply rules to messages in a folder.
#[allow(clippy::too_many_arguments)]
#[tokio::main]
pub async fn run_apply(
    folder: &str,
    account: Option<&str>,
    limit: u32,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;
    let account_id = creds.account.id.clone();

    let mut client = imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    // Fetch inbox messages
    let summaries = imap::fetch_inbox(&mut client, folder, limit)
        .await
        .context("failed to fetch messages")?;

    let enabled_rules = db
        .list_enabled_rules(&account_id)
        .context("failed to list enabled rules")?;

    if enabled_rules.is_empty() {
        if json {
            println!("{}", serde_json::json!({"processed": 0, "actions": 0, "message": "no enabled rules"}));
        } else {
            println!("No enabled rules — nothing to do");
        }
        return Ok(());
    }

    let total = summaries.len();
    let mut actions_taken = 0u32;
    let mut action_log: Vec<serde_json::Value> = Vec::new();

    // We need to process messages one at a time because actions (move/delete)
    // change UIDs. Collect UIDs first, then fetch full messages individually.
    let uids: Vec<u32> = summaries.iter().map(|s| s.uid).collect();

    for (i, &uid) in uids.iter().enumerate() {
        // Fetch the full message for rule evaluation
        let msg = match imap::fetch_message(&mut client, folder, uid).await {
            Ok(Some(m)) => m,
            Ok(None) => continue, // message may have been moved/deleted by a prior action
            Err(_) => continue,
        };

        let ctx = build_message_context(&msg, &db, &account_id)?;

        // Evaluate all enabled rules (in priority order, stop on first stop rule)
        for rule in &enabled_rules {
            let match_expr: rules::MatchExpr = match serde_json::from_str(&rule.match_expr) {
                Ok(e) => e,
                Err(_) => continue,
            };

            if !rules::evaluate(&match_expr, &ctx) {
                continue;
            }

            let action: Action = match serde_json::from_str(&rule.action) {
                Ok(a) => a,
                Err(_) => continue,
            };

            // Execute the action
            let action_result = execute_action(&mut client, &action, uid, folder).await;

            match &action_result {
                Ok(desc) => {
                    info!("rule '{}' fired on UID {uid}: {desc}", rule.name);
                    db.increment_rule_hit(&rule.id).ok();
                    actions_taken += 1;

                    action_log.push(serde_json::json!({
                        "uid": uid,
                        "rule": rule.name,
                        "action": desc,
                        "status": "ok",
                    }));
                }
                Err(e) => {
                    action_log.push(serde_json::json!({
                        "uid": uid,
                        "rule": rule.name,
                        "error": format!("{e}"),
                        "status": "error",
                    }));
                }
            }

            // If the action moved/deleted the message, skip remaining rules
            if matches!(action, Action::Move(_) | Action::Delete) {
                break;
            }

            if rule.stop {
                break;
            }
        }

        // Progress output (non-JSON only, every 50 messages)
        if !json && (i + 1) % 50 == 0 {
            eprintln!(
                "processed {}/{total}, {actions_taken} actions taken",
                i + 1,
            );
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "processed": total,
                "actions": actions_taken,
                "log": action_log,
            }))?
        );
    } else {
        println!(
            "processed {total}/{total}, {actions_taken} action(s) taken"
        );
    }

    Ok(())
}

/// Execute a single rule action against a message.
async fn execute_action(
    client: &mut imap::ImapClient,
    action: &Action,
    uid: u32,
    folder: &str,
) -> Result<String> {
    match action {
        Action::Move(dest) => {
            imap::move_message(client, uid, folder, dest)
                .await
                .with_context(|| format!("failed to move UID {uid} to {dest}"))?;
            Ok(format!("moved to {dest}"))
        }
        Action::Flag(flag) => {
            imap::set_flag(client, folder, uid, flag)
                .await
                .with_context(|| format!("failed to set flag '{flag}' on UID {uid}"))?;
            Ok(format!("flagged {flag}"))
        }
        Action::Unflag(flag) => {
            imap::remove_flag(client, folder, uid, flag)
                .await
                .with_context(|| format!("failed to remove flag '{flag}' from UID {uid}"))?;
            Ok(format!("unflagged {flag}"))
        }
        Action::Delete => {
            imap::delete_message(client, folder, uid)
                .await
                .with_context(|| format!("failed to delete UID {uid}"))?;
            Ok("deleted".to_string())
        }
        Action::AddTag(_tag) => {
            // Tag actions are metadata-only; they don't touch IMAP.
            // The tag was already set during context building in a production
            // pipeline, but in batch mode we skip this for now.
            Ok(format!("add_tag:{_tag} (metadata-only, skipped in batch)"))
        }
        Action::Snooze(_until) => {
            // Snooze requires full snooze machinery; log as unsupported in batch.
            Ok(format!("snooze:{_until} (use 'envelope snooze set' instead)"))
        }
        Action::Unsubscribe => {
            // Unsubscribe requires HTTP/SMTP; log as unsupported in batch.
            Ok("unsubscribe (use 'envelope unsubscribe' instead)".to_string())
        }
    }
}

/// `envelope rule enable <name>` — enable a rule by name.
pub fn run_enable(
    name: &str,
    account: Option<&str>,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = envelope_email_store::Database::open_default().context("failed to open database")?;
    let acct = super::common::resolve_account(&db, account)?;

    let rule = db
        .find_rule_by_name(&acct.id, name)
        .context("database error")?
        .ok_or_else(|| anyhow::anyhow!("rule '{name}' not found"))?;

    db.enable_rule(&rule.id).context("failed to enable rule")?;

    if json {
        println!("{}", serde_json::json!({"action": "enable", "name": name, "id": rule.id}));
    } else {
        println!("Enabled rule: {name}");
    }

    Ok(())
}

/// `envelope rule disable <name>` — disable a rule by name.
pub fn run_disable(
    name: &str,
    account: Option<&str>,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = envelope_email_store::Database::open_default().context("failed to open database")?;
    let acct = super::common::resolve_account(&db, account)?;

    let rule = db
        .find_rule_by_name(&acct.id, name)
        .context("database error")?
        .ok_or_else(|| anyhow::anyhow!("rule '{name}' not found"))?;

    db.disable_rule(&rule.id)
        .context("failed to disable rule")?;

    if json {
        println!("{}", serde_json::json!({"action": "disable", "name": name, "id": rule.id}));
    } else {
        println!("Disabled rule: {name}");
    }

    Ok(())
}

/// `envelope rule delete <name>` — delete a rule by name.
pub fn run_delete(
    name: &str,
    account: Option<&str>,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = envelope_email_store::Database::open_default().context("failed to open database")?;
    let acct = super::common::resolve_account(&db, account)?;

    let rule = db
        .find_rule_by_name(&acct.id, name)
        .context("database error")?
        .ok_or_else(|| anyhow::anyhow!("rule '{name}' not found"))?;

    db.delete_rule(&rule.id).context("failed to delete rule")?;

    if json {
        println!("{}", serde_json::json!({"action": "delete", "name": name, "id": rule.id}));
    } else {
        println!("Deleted rule: {name}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_action_move() {
        let a = parse_action("move=Archive").unwrap();
        assert_eq!(a, Action::Move("Archive".to_string()));
    }

    #[test]
    fn parse_action_flag() {
        let a = parse_action("flag=seen").unwrap();
        assert_eq!(a, Action::Flag("seen".to_string()));
    }

    #[test]
    fn parse_action_delete() {
        let a = parse_action("delete").unwrap();
        assert_eq!(a, Action::Delete);
    }

    #[test]
    fn parse_action_unsubscribe() {
        let a = parse_action("unsubscribe").unwrap();
        assert_eq!(a, Action::Unsubscribe);
    }

    #[test]
    fn parse_action_tag() {
        let a = parse_action("tag=processed").unwrap();
        assert_eq!(a, Action::AddTag("processed".to_string()));
    }

    #[test]
    fn parse_action_unknown() {
        assert!(parse_action("banana=split").is_err());
        assert!(parse_action("banana").is_err());
    }

    #[test]
    fn parse_score_filter_valid() {
        let (k, v) = parse_score_filter("urgent=0.7").unwrap();
        assert_eq!(k, "urgent");
        assert!((v - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_score_filter_invalid() {
        assert!(parse_score_filter("nope").is_err());
        assert!(parse_score_filter("bad=xyz").is_err());
    }
}

/// Export rules as a Sieve script.
pub fn run_export(
    account: Option<&str>,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = envelope_email_store::Database::open_default()
        .context("failed to open database")?;
    let acct = super::common::resolve_account(&db, account)?;
    let account_email = acct.username;

    let rules = db
        .list_enabled_rules(&account_email)
        .context("failed to list rules")?;

    let (script, skipped) = envelope_email_transport::sieve::export_sieve(&rules);

    if json {
        println!(
            "{}",
            serde_json::json!({
                "script": script,
                "skipped": skipped,
                "exported_count": rules.len() - skipped.len(),
            })
        );
    } else {
        if !skipped.is_empty() {
            eprintln!(
                "Skipped {} rule(s) (local-only, not Sieve-exportable): {}",
                skipped.len(),
                skipped.join(", ")
            );
        }
        print!("{script}");
    }

    Ok(())
}
