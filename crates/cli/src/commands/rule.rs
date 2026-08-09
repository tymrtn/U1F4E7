// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use envelope_email_store::credential_store::CredentialBackend;
use envelope_email_transport::imap;
use envelope_email_transport::rules::{self, Action, MessageContext};
use tracing::info;

use super::common::setup_credentials;
use super::ui;

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
///
/// `reject=<reason>` and `ereject=<reason>` produce server-side Sieve
/// actions that Envelope only emits via `envelope rule export`. They are
/// not executed locally — Envelope never fabricates a bounce against
/// already-delivered mail. Prefer `ereject` where the server supports it
/// (RFC 5429): it refuses the message at SMTP time and avoids generating
/// a backscatter MDN.
fn parse_action(s: &str) -> Result<Action> {
    if let Some((kind, arg)) = s.split_once('=') {
        match kind.to_lowercase().as_str() {
            "move" => Ok(Action::Move(arg.to_string())),
            "flag" => Ok(Action::Flag(arg.to_string())),
            "unflag" => Ok(Action::Unflag(arg.to_string())),
            "snooze" => Ok(Action::Snooze(arg.to_string())),
            "add_tag" | "addtag" | "tag" => Ok(Action::AddTag(arg.to_string())),
            "webhook" => {
                // SSRF guard: reject private/reserved/loopback/link-local
                // webhook targets before the URL is ever persisted.
                envelope_email_transport::url_guard::check_public_url(arg)
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .context("webhook action URL rejected")?;
                Ok(Action::Webhook(arg.to_string()))
            }
            "reject" => Ok(Action::Reject(arg.to_string())),
            "ereject" => Ok(Action::Ereject(arg.to_string())),
            _ => bail!(
                "unknown action type '{kind}'. Use: move, flag, unflag, snooze, tag, webhook, delete, unsubscribe, reject (server-side Sieve), ereject (server-side Sieve)"
            ),
        }
    } else {
        match s.to_lowercase().as_str() {
            "delete" => Ok(Action::Delete),
            "unsubscribe" => Ok(Action::Unsubscribe),
            "reject" | "ereject" => bail!(
                "action '{s}' requires a reason; use {s}=<reason> (server-side Sieve action; prefer ereject to avoid backscatter)"
            ),
            _ => {
                bail!(
                    "unknown action '{s}'. Use: move=<folder>, flag=<name>, delete, unsubscribe, reject=<reason>, ereject=<reason>"
                )
            }
        }
    }
}

fn sanitize_action_display(action: &str) -> String {
    match serde_json::from_str::<Action>(action) {
        Ok(Action::Webhook(_)) => serde_json::to_string(&Action::Webhook("[redacted]".to_string()))
            .unwrap_or_else(|_| "{\"webhook\":\"[redacted]\"}".to_string()),
        Ok(parsed) => {
            serde_json::to_string(&parsed).unwrap_or_else(|_| "[invalid action]".to_string())
        }
        Err(_) => "[invalid action]".to_string(),
    }
}

/// Build a `MessageContext` from a fetched message + its tags/scores in the store.
fn build_message_context(
    msg: &envelope_email_store::Message,
    db: &envelope_email_store::Database,
    account_id: &str,
) -> Result<MessageContext> {
    // Canonicalize so summary/full/persistence keys agree (IMAP ENVELOPE ids
    // arrive bracketed; persisted scores/tags use the bare id).
    let message_id =
        envelope_email_store::canonical_message_id(msg.message_id.as_deref().unwrap_or(""));

    let tags: Vec<String> = if !message_id.is_empty() {
        db.get_tags(account_id, message_id)
            .context("failed to get tags")?
            .into_iter()
            .map(|t| t.tag)
            .collect()
    } else {
        vec![]
    };

    let mut scores: HashMap<String, f64> = if !message_id.is_empty() {
        db.get_scores(account_id, message_id)
            .context("failed to get scores")?
            .into_iter()
            .map(|s| (s.dimension, s.value))
            .collect()
    } else {
        HashMap::new()
    };
    // Seed the header-derived provider_spam signal; a persisted score wins.
    rules::merge_provider_spam(&mut scores, msg.provider_spam);

    let contact_tags = db
        .get_contact_tags(account_id, &msg.from_addr)
        .context("failed to get contact tags")?;

    Ok(MessageContext {
        from_addr: msg.from_addr.clone(),
        to_addr: msg.to_addr.clone(),
        subject: msg.subject.clone(),
        tags,
        scores,
        contact_tags,
    })
}

/// Build a `MessageContext` from a fetched message summary + its tags/scores in the store.
///
/// This avoids downloading full RFC822 bodies during batch rule preview/run. Header-only
/// rules only need fields already present in `MessageSummary` from the initial batch FETCH.
fn build_message_context_from_summary(
    summary: &envelope_email_store::MessageSummary,
    db: &envelope_email_store::Database,
    account_id: &str,
) -> Result<MessageContext> {
    // Canonicalize so summary/full/persistence keys agree (IMAP ENVELOPE ids
    // arrive bracketed; persisted scores/tags use the bare id).
    let message_id =
        envelope_email_store::canonical_message_id(summary.message_id.as_deref().unwrap_or(""));

    let tags: Vec<String> = if !message_id.is_empty() {
        db.get_tags(account_id, message_id)
            .context("failed to get tags")?
            .into_iter()
            .map(|t| t.tag)
            .collect()
    } else {
        vec![]
    };

    let mut scores: HashMap<String, f64> = if !message_id.is_empty() {
        db.get_scores(account_id, message_id)
            .context("failed to get scores")?
            .into_iter()
            .map(|s| (s.dimension, s.value))
            .collect()
    } else {
        HashMap::new()
    };
    // Seed the header-derived provider_spam signal; a persisted score wins.
    rules::merge_provider_spam(&mut scores, summary.provider_spam);

    let contact_tags = db
        .get_contact_tags(account_id, &summary.from_addr)
        .context("failed to get contact tags")?;

    Ok(MessageContext {
        from_addr: summary.from_addr.clone(),
        to_addr: summary.to_addr.clone(),
        subject: summary.subject.clone(),
        tags,
        scores,
        contact_tags,
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
    match_contact_tags: &[String],
    action_str: &str,
    priority: i64,
    stop: bool,
    enabled: bool,
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
        match_contact_tags,
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
        .create_rule_with_enabled(
            account_id,
            name,
            &match_expr_json,
            &action_json,
            priority,
            stop,
            enabled,
        )
        .context("failed to create rule")?;

    if json {
        let value = ui::with_ui(&rule, ui::rules_ui(account_id));
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("Created rule: {}", rule.name);
        println!("  ID:       {}", rule.id);
        println!("  Priority: {}", rule.priority);
        println!("  Enabled:  {}", rule.enabled);
        println!("  Stop:     {}", rule.stop);
        println!(
            "  Sieve:    {}",
            if rule.sieve_exportable { "yes" } else { "no" }
        );
        println!("  Match:    {match_expr_json}");
        println!("  Action:   {action_json}");
    }

    Ok(())
}

/// `envelope rule list` — list all rules for an account.
pub fn run_list(account: Option<&str>, json: bool, _backend: CredentialBackend) -> Result<()> {
    let db = envelope_email_store::Database::open_default().context("failed to open database")?;
    let acct = super::common::resolve_account(&db, account)?;

    let rules = db.list_rules(&acct.id).context("failed to list rules")?;

    if json {
        let rules_ui_meta = ui::rules_ui(&acct.id);
        let safe_rules: Vec<_> = rules
            .iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.id,
                    "account_id": r.account_id,
                    "name": r.name,
                    "match_expr": r.match_expr,
                    "action": sanitize_action_display(&r.action),
                    "enabled": r.enabled,
                    "priority": r.priority,
                    "stop": r.stop,
                    "sieve_exportable": r.sieve_exportable,
                    "hit_count": r.hit_count,
                    "last_hit_at": r.last_hit_at,
                    "created_at": r.created_at,
                    "updated_at": r.updated_at,
                    "ui": rules_ui_meta,
                })
            })
            .collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&safe_rules)?);
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
                r.priority,
                enabled_mark,
                stop_mark,
                r.hit_count,
                sieve_mark,
                name_display,
                sanitize_action_display(&r.action),
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
                "action": sanitize_action_display(&rule.action),
                "enabled": rule.enabled,
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
                "ui": ui::message_ui(&account_id, uid, folder),
            }))?
        );
    } else {
        println!("Testing UID {uid} ({folder})");
        println!("  From:    {}", msg.from_addr);
        println!("  Subject: {}", msg.subject);
        println!(
            "  Tags:    {}",
            if ctx.tags.is_empty() {
                "(none)".to_string()
            } else {
                ctx.tags.join(", ")
            }
        );
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

/// Reusable rule-preview core: resolve rules against fetched summaries and
/// return the structured `{mode, folder, processed, matches, mutated}` Value
/// with no mailbox mutation. Shared by the CLI `run_preview` wrapper and the
/// MCP `rules_preview` tool so both advertise identical semantics.
pub async fn preview_core(
    client: &mut imap::ImapClient,
    db: &envelope_email_store::Database,
    account_id: &str,
    folder: &str,
    limit: u32,
) -> Result<serde_json::Value> {
    let summaries = imap::fetch_inbox(client, folder, limit)
        .await
        .context("failed to fetch messages")?;
    let preview_rules = db.list_rules(account_id).context("failed to list rules")?;

    let total = summaries.len();
    let mut matches: Vec<serde_json::Value> = Vec::new();
    for summary in &summaries {
        let ctx = build_message_context_from_summary(summary, db, account_id)?;
        for rule in &preview_rules {
            let match_expr: rules::MatchExpr = match serde_json::from_str(&rule.match_expr) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !rules::evaluate(&match_expr, &ctx) {
                continue;
            }
            matches.push(serde_json::json!({
                "uid": summary.uid,
                "from": summary.from_addr,
                "subject": summary.subject,
                "rule": rule.name,
                "action": sanitize_action_display(&rule.action),
                "enabled": rule.enabled,
                "stop": rule.stop,
            }));
            if rule.stop && rule.enabled {
                break;
            }
        }
    }

    Ok(serde_json::json!({
        "mode": "preview",
        "folder": folder,
        "processed": total,
        "matches": matches,
        "mutated": false,
        "ui": ui::rules_ui(account_id),
    }))
}

/// Reusable rule-run core: apply enabled rules against fetched summaries,
/// mutating the mailbox, and return the structured `{processed, actions, log}`
/// Value. Shared by the CLI `run_apply` wrapper and the MCP `rules_run` tool.
/// The caller is responsible for the confirm/dry-run gate — this always mutates.
pub async fn apply_core(
    client: &mut imap::ImapClient,
    db: &envelope_email_store::Database,
    account_id: &str,
    folder: &str,
    limit: u32,
) -> Result<serde_json::Value> {
    let summaries = imap::fetch_inbox(client, folder, limit)
        .await
        .context("failed to fetch messages")?;

    let enabled_rules = db
        .list_enabled_rules(account_id)
        .context("failed to list enabled rules")?;

    if enabled_rules.is_empty() {
        return Ok(serde_json::json!({
            "processed": 0,
            "actions": 0,
            "log": [],
            "message": "no enabled rules",
            "ui": ui::rules_ui(account_id),
        }));
    }

    let total = summaries.len();
    let mut actions_taken = 0u32;
    let mut action_log: Vec<serde_json::Value> = Vec::new();

    for summary in summaries.iter() {
        let uid = summary.uid;
        let ctx = build_message_context_from_summary(summary, db, account_id)?;

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
            let action_result = execute_action(
                client,
                db,
                account_id,
                &action,
                uid,
                folder,
                Some(&rule.name),
                Some(&ctx),
            )
            .await;
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
            if matches!(action, Action::Move(_) | Action::Delete) {
                break;
            }
            if rule.stop {
                break;
            }
        }
    }

    Ok(serde_json::json!({
        "processed": total,
        "actions": actions_taken,
        "log": action_log,
        "ui": ui::rules_ui(account_id),
    }))
}

/// `envelope rule preview` — batch preview rules without mailbox mutation.
#[allow(clippy::too_many_arguments)]
#[tokio::main]
pub async fn run_preview(
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
    let result = preview_core(&mut client, &db, &account_id, folder, limit).await?;
    let matches = result["matches"].as_array().cloned().unwrap_or_default();
    let total = result["processed"].as_u64().unwrap_or(0) as usize;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if matches.is_empty() {
        println!(
            "Preview: no rules would touch {total} message(s) in {folder}; no mailbox changes made"
        );
    } else {
        println!(
            "Preview: {} proposed action(s) across {total} message(s) in {folder}; no mailbox changes made",
            matches.len()
        );
        for m in &matches {
            println!(
                "  UID {}: {} -> {}",
                m["uid"],
                m["rule"].as_str().unwrap_or("?"),
                m["action"].as_str().unwrap_or("?")
            );
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
    confirm: bool,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    if !confirm {
        bail!(
            "rule run mutates the mailbox; preview first with `envelope rule preview`, then rerun with --confirm"
        );
    }
    let (db, creds) = setup_credentials(account, backend)?;
    let account_id = creds.account.id.clone();

    let mut client = imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    let result = apply_core(&mut client, &db, &account_id, folder, limit).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if result.get("message").and_then(|m| m.as_str()) == Some("no enabled rules") {
        println!("No enabled rules — nothing to do");
    } else {
        let total = result["processed"].as_u64().unwrap_or(0);
        let actions_taken = result["actions"].as_u64().unwrap_or(0);
        println!("processed {total}/{total}, {actions_taken} action(s) taken");
    }

    Ok(())
}

/// Execute a single rule action against a message.
///
/// Server-side Sieve actions (`reject`, `ereject`) are export-only: when
/// encountered here they record a stable non-mutating skip instead of
/// trying to fabricate a bounce on already-delivered mail.
async fn execute_action(
    client: &mut imap::ImapClient,
    db: &envelope_email_store::Database,
    account_id: &str,
    action: &Action,
    uid: u32,
    folder: &str,
    rule_name: Option<&str>,
    ctx: Option<&MessageContext>,
) -> Result<String> {
    if let Some(skip) = action.local_execution_skip_reason() {
        return Ok(format!("skipped: {skip}"));
    }
    match action {
        Action::Move(dest) => {
            // A canonical sentinel (`\Junk`/`\Archive`/`\Trash`) is resolved to
            // this account's real provider folder before moving; a literal folder
            // passes through unchanged. An unresolved sentinel fails loudly rather
            // than misfiling into a literal `\Junk` mailbox.
            let real = envelope_email_transport::folders::resolve_move_destination(
                client, db, account_id, dest,
            )
            .await
            .with_context(|| format!("failed to resolve move target {dest} for UID {uid}"))?
            .with_context(|| {
                format!(
                    "no provider folder for canonical move target {dest} (UID {uid}); \
                     not moving into a literal {dest}"
                )
            })?;
            imap::move_message(client, uid, folder, &real)
                .await
                .with_context(|| format!("failed to move UID {uid} to {real}"))?;
            Ok(format!("moved to {real}"))
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
            Ok(format!(
                "snooze:{_until} (use 'envelope snooze set' instead)"
            ))
        }
        Action::Unsubscribe => {
            // Unsubscribe requires HTTP/SMTP; log as unsupported in batch.
            Ok("unsubscribe (use 'envelope unsubscribe' instead)".to_string())
        }
        Action::Webhook(url) => {
            let payload = serde_json::json!({
                "event": "rule_matched",
                "rule": rule_name.unwrap_or("unknown"),
                "uid": uid,
                "folder": folder,
                "message": {
                    "from": ctx.map(|c| c.from_addr.as_str()).unwrap_or(""),
                    "to": ctx.map(|c| c.to_addr.as_str()).unwrap_or(""),
                    "subject": ctx.map(|c| c.subject.as_str()).unwrap_or(""),
                }
            });
            let http = reqwest::Client::new();
            let body = serde_json::to_vec(&payload)
                .map_err(|e| anyhow::anyhow!("failed to serialize webhook payload: {e}"))?;
            match http
                .post(url.as_str())
                .header("Content-Type", "application/json")
                .body(body)
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
            {
                Ok(resp) => Ok(format!("webhook: {}", resp.status())),
                Err(_) => Err(anyhow::anyhow!("webhook failed")),
            }
        }
        // Server-side Sieve actions are intercepted at the top of this
        // function — these arms are unreachable but kept exhaustive.
        Action::Reject(_) | Action::Ereject(_) => {
            Ok(format!("skipped: {}", rules::SERVER_SIDE_ONLY_SKIP_REASON))
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
        println!(
            "{}",
            serde_json::json!({
                "action": "enable",
                "name": name,
                "id": rule.id,
                "ui": ui::rules_ui(&acct.id),
            })
        );
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
        println!(
            "{}",
            serde_json::json!({
                "action": "disable",
                "name": name,
                "id": rule.id,
                "ui": ui::rules_ui(&acct.id),
            })
        );
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
        println!(
            "{}",
            serde_json::json!({
                "action": "delete",
                "name": name,
                "id": rule.id,
                "ui": ui::rules_ui(&acct.id),
            })
        );
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
    fn parse_action_webhook_accepts_public_https() {
        let a = parse_action("webhook=https://hooks.example.com/rule").unwrap();
        assert_eq!(
            a,
            Action::Webhook("https://hooks.example.com/rule".to_string())
        );
    }

    #[test]
    fn parse_action_webhook_rejects_link_local_metadata() {
        // AWS/GCP/Azure instance-metadata endpoint must be rejected.
        // `{:#}` renders the full anyhow chain inline (outer context + source),
        // matching what the CLI's error printer shows the user.
        let err = format!(
            "{:#}",
            parse_action("webhook=http://169.254.169.254/latest/meta-data/").unwrap_err()
        );
        assert!(
            err.contains("rejected") && err.contains("private/reserved"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_action_webhook_rejects_localhost_and_private() {
        assert!(parse_action("webhook=http://localhost:8080/hook").is_err());
        assert!(parse_action("webhook=http://10.0.0.5/hook").is_err());
    }

    #[test]
    fn parse_action_reject() {
        let a = parse_action("reject=No such address").unwrap();
        assert_eq!(a, Action::Reject("No such address".to_string()));
    }

    #[test]
    fn parse_action_ereject() {
        let a = parse_action("ereject=Mailbox closed").unwrap();
        assert_eq!(a, Action::Ereject("Mailbox closed".to_string()));
    }

    #[test]
    fn parse_action_reject_requires_reason() {
        // reject without `=<reason>` should fail with a clear error, since
        // the Sieve `reject` action requires a reason string.
        let err = parse_action("reject").unwrap_err().to_string();
        assert!(
            err.contains("reject") && err.contains("reason"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_action_unknown_message_mentions_reject_and_ereject() {
        let err = parse_action("banana=split").unwrap_err().to_string();
        assert!(
            err.contains("reject") && err.contains("ereject"),
            "expected reject/ereject in error hint, got: {err}"
        );
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

    #[test]
    fn build_context_from_summary_maps_fields() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        let summary = envelope_email_store::MessageSummary {
            uid: 42,
            message_id: Some("<test@example.com>".to_string()),
            from_addr: "alice@example.com".to_string(),
            to_addr: "bob@example.com".to_string(),
            subject: "Hello from Alice".to_string(),
            date: None,
            flags: vec![],
            size: 1024,
            provider_spam: None,
        };

        let ctx = build_message_context_from_summary(&summary, &db, "test-account").unwrap();

        assert_eq!(ctx.from_addr, "alice@example.com");
        assert_eq!(ctx.to_addr, "bob@example.com");
        assert_eq!(ctx.subject, "Hello from Alice");
        assert!(ctx.tags.is_empty());
        assert!(ctx.scores.is_empty());
        assert!(ctx.contact_tags.is_empty());
    }

    #[test]
    fn rule_subject_glob_matches_context_from_summary() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        let summary = envelope_email_store::MessageSummary {
            uid: 1,
            message_id: None,
            from_addr: "sender@example.com".to_string(),
            to_addr: "recipient@example.com".to_string(),
            subject: "Important Test Message".to_string(),
            date: None,
            flags: vec![],
            size: 512,
            provider_spam: None,
        };

        let ctx = build_message_context_from_summary(&summary, &db, "test-account").unwrap();
        let expr = rules::MatchExpr::Subject("*Test*".to_string());

        assert!(rules::evaluate(&expr, &ctx));
    }

    #[test]
    fn rule_from_glob_matches_context_from_summary() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        let summary = envelope_email_store::MessageSummary {
            uid: 2,
            message_id: None,
            from_addr: "noreply@spam.com".to_string(),
            to_addr: "victim@example.com".to_string(),
            subject: "You won!".to_string(),
            date: None,
            flags: vec![],
            size: 256,
            provider_spam: None,
        };

        let ctx = build_message_context_from_summary(&summary, &db, "test-account").unwrap();
        let expr = rules::MatchExpr::From("*@spam.com".to_string());

        assert!(rules::evaluate(&expr, &ctx));
    }

    #[test]
    fn summary_provider_spam_seeds_context_score() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        let summary = envelope_email_store::MessageSummary {
            uid: 7,
            message_id: Some("<spammy@example.com>".to_string()),
            from_addr: "noreply@promo.example".to_string(),
            to_addr: "me@example.com".to_string(),
            subject: "Deal!".to_string(),
            date: None,
            flags: vec![],
            size: 128,
            provider_spam: Some(6.5),
        };

        let ctx = build_message_context_from_summary(&summary, &db, "test-account").unwrap();

        assert_eq!(
            ctx.scores.get(rules::PROVIDER_SPAM_DIMENSION),
            Some(&6.5),
            "derived provider_spam must seed the batch summary context"
        );
        // A score_above rule can now match the header signal.
        let expr = rules::MatchExpr::ScoreAbove {
            dimension: rules::PROVIDER_SPAM_DIMENSION.to_string(),
            threshold: 5.0,
        };
        assert!(rules::evaluate(&expr, &ctx));
    }

    #[test]
    fn persisted_provider_spam_wins_over_derived_across_bracketed_summary_id() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        // Scores are persisted under the bare Message-ID (mail_parser / tag-cmd
        // form)...
        db.set_score(
            "test-account",
            "pinned@example.com",
            rules::PROVIDER_SPAM_DIMENSION,
            2.0,
            None,
            None,
        )
        .unwrap();
        // ...while the IMAP ENVELOPE summary hands us the bracketed id.
        let summary = envelope_email_store::MessageSummary {
            uid: 8,
            message_id: Some("<pinned@example.com>".to_string()),
            from_addr: "sender@example.com".to_string(),
            to_addr: "me@example.com".to_string(),
            subject: "Pinned".to_string(),
            date: None,
            flags: vec![],
            size: 128,
            provider_spam: Some(9.9),
        };

        let ctx = build_message_context_from_summary(&summary, &db, "test-account").unwrap();

        assert_eq!(
            ctx.scores.get(rules::PROVIDER_SPAM_DIMENSION),
            Some(&2.0),
            "persisted score keyed on the bare id must be found for a bracketed \
             summary id and win over the derived header value"
        );
    }
}

/// Hard ceiling on user-supplied ManageSieve network timeout. Matches the
/// quickstart cap; keeps a typo'd `--timeout-secs 999999` from blocking a
/// CLI invocation indefinitely.
const MAX_MANAGESIEVE_TIMEOUT_SECS: u64 = 60;
const MIN_MANAGESIEVE_TIMEOUT_SECS: u64 = 1;

/// Validate a user-supplied script name. ManageSieve allows quoted strings
/// containing arbitrary bytes, but Envelope refuses anything that would
/// require the literal form here — CR/LF/NUL are rejected outright, and
/// empty names are rejected because Pigeonhole maps them to "the default
/// script" which is not what an operator passing `--script-name` expects.
fn validate_script_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("--script-name must not be empty");
    }
    if name.contains(['\r', '\n', '\0']) {
        bail!("--script-name must not contain control characters");
    }
    if name.len() > 128 {
        bail!("--script-name must be 128 characters or fewer");
    }
    Ok(())
}

/// `envelope rule publish-sieve` — render the export and (optionally)
/// upload it to a ManageSieve server.
///
/// Safety contract:
/// - When neither `--dry-run` nor `--confirm` is provided, runs in
///   dry-run mode so the default surface is non-mutating.
/// - `--confirm` is mandatory before any network upload happens.
/// - Dry-run JSON includes the resolved endpoint and the generated script
///   so an operator can diff it against the server's current `envelope-rules`
///   without exposing credentials.
#[allow(clippy::too_many_arguments)]
#[tokio::main]
pub async fn run_publish_sieve(
    account: Option<&str>,
    script_name: &str,
    host: Option<&str>,
    port: Option<u16>,
    timeout_secs: u64,
    dry_run: bool,
    confirm: bool,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    validate_script_name(script_name)?;

    if dry_run && confirm {
        bail!("--dry-run and --confirm are mutually exclusive");
    }

    let timeout_secs =
        timeout_secs.clamp(MIN_MANAGESIEVE_TIMEOUT_SECS, MAX_MANAGESIEVE_TIMEOUT_SECS);

    let db = envelope_email_store::Database::open_default().context("failed to open database")?;
    let acct = super::common::resolve_account(&db, account)?;
    let account_id = acct.id.clone();
    let imap_host = acct.imap_host.clone();

    let rules = db
        .list_enabled_rules(&account_id)
        .context("failed to list rules")?;
    let (script, skipped) = envelope_email_transport::sieve::export_sieve(&rules);
    let exported_count = rules.len() - skipped.len();

    let (resolved_host, resolved_port) =
        envelope_email_transport::managesieve::resolve_sieve_endpoint(&imap_host, host, port);

    if !confirm {
        let plan = envelope_email_transport::managesieve::build_plan(
            &account_id,
            &resolved_host,
            resolved_port,
            script_name,
            script,
            skipped,
            exported_count,
        );
        if json {
            let value = ui::with_ui(&plan, ui::rules_ui(&account_id));
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            println!(
                "ManageSieve dry-run for {account_id}: would PUTSCRIPT \"{name}\" + SETACTIVE on {host}:{port} ({count} rule(s), {sk} skipped). Rerun with --confirm to upload.",
                name = script_name,
                host = resolved_host,
                port = resolved_port,
                count = exported_count,
                sk = plan.skipped.len(),
            );
            if !plan.skipped.is_empty() {
                eprintln!("Skipped local-only rules: {}", plan.skipped.join(", "));
            }
        }
        return Ok(());
    }

    if exported_count == 0 {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "status": "no_exportable_rules",
                    "account_id": account_id,
                    "host": resolved_host,
                    "port": resolved_port,
                    "script_name": script_name,
                    "exported_count": 0,
                    "skipped": skipped,
                    "network_used": false,
                    "ui": ui::rules_ui(&account_id),
                })
            );
        } else {
            println!("No exportable rules — nothing to upload to {resolved_host}:{resolved_port}");
            if !skipped.is_empty() {
                eprintln!("Skipped local-only rules: {}", skipped.join(", "));
            }
        }
        return Ok(());
    }

    let creds = {
        let passphrase = envelope_email_store::credential_store::get_or_create_passphrase(backend)
            .context("credential store error")?;
        db.get_account_with_credentials(&account_id, &passphrase)
            .context("failed to decrypt credentials")?
    };

    let timeout = std::time::Duration::from_secs(timeout_secs);
    let result = envelope_email_transport::managesieve::publish_script(
        &creds,
        &resolved_host,
        resolved_port,
        script_name,
        &script,
        exported_count,
        skipped,
        timeout,
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    if json {
        let value = ui::with_ui(&result, ui::rules_ui(&account_id));
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!(
            "Uploaded {count} rule(s) as \"{name}\" to {host}:{port} ({sk} skipped, active script set to \"{active}\")",
            count = result.exported_count,
            name = result.script_name,
            host = result.host,
            port = result.port,
            sk = result.skipped.len(),
            active = result.active_script,
        );
        if !result.skipped.is_empty() {
            eprintln!("Skipped local-only rules: {}", result.skipped.join(", "));
        }
    }

    Ok(())
}

/// Export rules as a Sieve script.
pub fn run_export(account: Option<&str>, json: bool, _backend: CredentialBackend) -> Result<()> {
    let db = envelope_email_store::Database::open_default().context("failed to open database")?;
    let acct = super::common::resolve_account(&db, account)?;
    let account_id = acct.id;

    let rules = db
        .list_enabled_rules(&account_id)
        .context("failed to list rules")?;

    let (script, skipped) = envelope_email_transport::sieve::export_sieve(&rules);

    if json {
        println!(
            "{}",
            serde_json::json!({
                "script": script,
                "skipped": skipped,
                "exported_count": rules.len() - skipped.len(),
                "ui": ui::rules_ui(&account_id),
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
