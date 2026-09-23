// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! The one rule executor.
//!
//! Every caller that runs rule actions (`envelope rule run`, MCP `rules_run`,
//! the dashboard's run-enabled endpoint, `envelope watch --run-rules`, and
//! `envelope actions confirm`) goes through [`execute_action`]. Each executed
//! action writes one `action_log` row carrying an [`ActionAttribution`]; rule
//! runs also keep writing `rule_run_audit`. The `action_log` row is keyed by
//! `(event_id, action_type)`, which is UNIQUE, so replaying a rule over the
//! same message is a no-op instead of a second mutation.
//!
//! Mailbox effects go through [`RuleMailbox`] and database access through
//! [`ExecDb`], so the dashboard can keep its "no DB guard across an IMAP
//! await" discipline and tests can run the real executor against a fake.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use envelope_email_store::action_log::EventActionLogInput;
use envelope_email_store::event_catalog::ACTION_OFFERED;
use envelope_email_store::models::{ActionLog, Event, MessageSummary, Rule};
use envelope_email_store::{Database, RuleRunAuditInput};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::http::Allowance;
use crate::imap::{self, ImapClient};
use crate::rules::{self, Action, ConfirmableAction, MatchExpr, MessageContext, StoredRuleAction};

/// IMAP folder snoozed messages are parked in (same as `envelope snooze set`).
pub const SNOOZED_FOLDER: &str = "Snoozed";

/// `action_log.action_type` of the row that records an offer's confirm or
/// dismiss decision. UNIQUE per offer event, so an offer resolves once.
pub const OFFER_RESOLUTION: &str = "offer_resolution";

/// Who caused an action. Serialized names are a public contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionSource {
    Rule,
    CairnAction,
    Reader,
    Cli,
    Mcp,
}

impl ActionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ActionSource::Rule => "rule",
            ActionSource::CairnAction => "cairn_action",
            ActionSource::Reader => "reader",
            ActionSource::Cli => "cli",
            ActionSource::Mcp => "mcp",
        }
    }
}

/// Attribution written into `action_log` for every executed action.
///
/// `event_id` is the idempotency key. When it is `None` and a rule drives the
/// action, the executor derives `rule:<rule_id>:<message>` so a replay of the
/// same rule on the same message never mutates twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionAttribution {
    pub source: ActionSource,
    pub agent_id: Option<String>,
    pub event_id: Option<String>,
}

impl ActionAttribution {
    pub fn new(source: ActionSource) -> Self {
        ActionAttribution {
            source,
            agent_id: None,
            event_id: None,
        }
    }

    pub fn with_agent(mut self, agent_id: Option<&str>) -> Self {
        self.agent_id = agent_id.map(str::to_string);
        self
    }
}

/// Mailbox effects the executor needs. Implemented over a live
/// [`ImapClient`] by [`ImapRuleMailbox`] (and by the dashboard over its
/// pooled client); tests implement it with a recorder.
#[allow(async_fn_in_trait)]
pub trait RuleMailbox {
    /// Resolve a move destination: canonical sentinels (`\Junk`, `\Archive`,
    /// …) map to the provider's real folder, literal names pass through. An
    /// unresolvable sentinel is an error, never a literal `\Junk` mailbox.
    async fn resolve_folder(&mut self, dest: &str) -> Result<String>;
    async fn move_message(&mut self, folder: &str, uid: u32, dest: &str) -> Result<()>;
    async fn set_flag(&mut self, folder: &str, uid: u32, flag: &str) -> Result<()>;
    async fn remove_flag(&mut self, folder: &str, uid: u32, flag: &str) -> Result<()>;
    async fn delete_message(&mut self, folder: &str, uid: u32) -> Result<()>;
    async fn ensure_folder(&mut self, name: &str) -> Result<()>;
    /// `(List-Unsubscribe, List-Unsubscribe-Post)` header values.
    async fn list_unsubscribe_headers(
        &mut self,
        folder: &str,
        uid: u32,
    ) -> Result<(Option<String>, Option<String>)>;
}

/// Scoped database access. The CLI passes its `Database` directly; the
/// dashboard locks its shared handle only for the duration of `f`.
#[allow(async_fn_in_trait)]
pub trait ExecDb {
    async fn with_db<R>(&self, f: impl FnOnce(&Database) -> R) -> R;
}

impl ExecDb for Database {
    async fn with_db<R>(&self, f: impl FnOnce(&Database) -> R) -> R {
        f(self)
    }
}

/// [`RuleMailbox`] over a live IMAP client, resolving sentinels through the
/// account's cached/detected provider folders.
pub struct ImapRuleMailbox<'a> {
    pub client: &'a mut ImapClient,
    pub db: &'a Database,
    pub account_id: &'a str,
}

impl RuleMailbox for ImapRuleMailbox<'_> {
    async fn resolve_folder(&mut self, dest: &str) -> Result<String> {
        crate::folders::resolve_move_destination(self.client, self.db, self.account_id, dest)
            .await
            .with_context(|| format!("failed to resolve move target {dest}"))?
            .with_context(|| {
                format!("no provider folder for canonical move target {dest}; not moving into a literal {dest}")
            })
    }

    async fn move_message(&mut self, folder: &str, uid: u32, dest: &str) -> Result<()> {
        imap::move_message(self.client, uid, folder, dest)
            .await
            .with_context(|| format!("failed to move UID {uid} to {dest}"))
    }

    async fn set_flag(&mut self, folder: &str, uid: u32, flag: &str) -> Result<()> {
        imap::set_flag(self.client, folder, uid, flag)
            .await
            .with_context(|| format!("failed to set flag '{flag}' on UID {uid}"))
    }

    async fn remove_flag(&mut self, folder: &str, uid: u32, flag: &str) -> Result<()> {
        imap::remove_flag(self.client, folder, uid, flag)
            .await
            .with_context(|| format!("failed to remove flag '{flag}' from UID {uid}"))
    }

    async fn delete_message(&mut self, folder: &str, uid: u32) -> Result<()> {
        imap::delete_message(self.client, folder, uid)
            .await
            .with_context(|| format!("failed to delete UID {uid}"))
    }

    async fn ensure_folder(&mut self, name: &str) -> Result<()> {
        imap::create_folder(self.client, name)
            .await
            .with_context(|| format!("failed to create folder {name}"))
    }

    async fn list_unsubscribe_headers(
        &mut self,
        folder: &str,
        uid: u32,
    ) -> Result<(Option<String>, Option<String>)> {
        imap::fetch_list_unsubscribe_headers(self.client, folder, uid)
            .await
            .with_context(|| format!("failed to fetch List-Unsubscribe headers for UID {uid}"))
    }
}

/// The message an action applies to.
pub struct MessageTarget<'a> {
    pub account_id: &'a str,
    /// Account login (the snooze table's account key).
    pub account_email: &'a str,
    pub folder: &'a str,
    pub uid: u32,
    /// Canonical (unbracketed) Message-ID, when the message has one.
    pub message_id: Option<&'a str>,
    pub ctx: &'a MessageContext,
}

impl MessageTarget<'_> {
    fn message_key(&self) -> String {
        match self.message_id {
            Some(mid) if !mid.is_empty() => format!("msg:{mid}"),
            _ => format!("uid:{}:{}:{}", self.account_id, self.folder, self.uid),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecStatus {
    /// The action ran and an `action_log` row was written.
    Executed,
    /// An `action_log` row already exists for this event; nothing ran.
    AlreadyApplied,
    /// A `confirm` rule recorded an `action_offered` event; nothing ran.
    Offered,
    /// The action is not executed locally (server-side Sieve actions).
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutcome {
    pub status: ExecStatus,
    pub description: String,
    /// The `action_offered` event id, for [`ExecStatus::Offered`].
    pub offer_event_id: Option<String>,
}

impl ExecOutcome {
    fn new(status: ExecStatus, description: impl Into<String>) -> Self {
        ExecOutcome {
            status,
            description: description.into(),
            offer_event_id: None,
        }
    }
}

/// A rule whose JSON parsed and whose action passed the compatibility gate.
#[derive(Debug, Clone)]
pub struct LoadedRule {
    pub rule: Rule,
    pub match_expr: MatchExpr,
    pub action: Action,
}

/// A rule the loader refused to run, with a stable reason string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedRule {
    pub rule_id: String,
    pub rule_name: String,
    pub reason: String,
}

fn skip(rule: &Rule, reason: String) -> SkippedRule {
    SkippedRule {
        rule_id: rule.id.clone(),
        rule_name: rule.name.clone(),
        reason,
    }
}

/// Split stored rules into those whose match can be evaluated and those that
/// must be reported instead: an unparseable match, or one with an empty
/// condition list. Previews and `rule test` use this directly;
/// [`load_rules`] applies it before the action gate.
pub fn split_evaluable_rules(rules: Vec<Rule>) -> (Vec<(Rule, MatchExpr)>, Vec<SkippedRule>) {
    let mut evaluable = Vec::new();
    let mut skipped = Vec::new();
    for rule in rules {
        match serde_json::from_str::<MatchExpr>(&rule.match_expr) {
            Ok(expr) if expr.has_empty_condition_list() => skipped.push(skip(
                &rule,
                rules::EMPTY_CONDITION_LIST_SKIP_REASON.to_string(),
            )),
            Ok(expr) => evaluable.push((rule, expr)),
            Err(e) => skipped.push(skip(&rule, format!("invalid match_expr: {e}"))),
        }
    }
    (evaluable, skipped)
}

/// Parse rules and apply the compatibility gate. Invalid JSON, matches with
/// an empty condition list, and unacknowledged snooze/unsubscribe rules are
/// reported, never run.
pub fn load_rules(rules: Vec<Rule>) -> (Vec<LoadedRule>, Vec<SkippedRule>) {
    let (evaluable, mut skipped) = split_evaluable_rules(rules);
    let mut loaded = Vec::new();
    for (rule, match_expr) in evaluable {
        let stored = match StoredRuleAction::parse(&rule.action) {
            Ok(stored) => stored,
            Err(e) => {
                skipped.push(skip(&rule, format!("invalid action: {e}")));
                continue;
            }
        };
        if let Some(reason) = stored.gate_skip_reason() {
            skipped.push(skip(&rule, reason.to_string()));
            continue;
        }
        loaded.push(LoadedRule {
            rule,
            match_expr,
            action: stored.action,
        });
    }
    (loaded, skipped)
}

pub fn load_enabled_rules(
    db: &Database,
    account_id: &str,
) -> Result<(Vec<LoadedRule>, Vec<SkippedRule>)> {
    let rules = db
        .list_enabled_rules(account_id)
        .context("failed to list enabled rules")?;
    Ok(load_rules(rules))
}

/// Enabled rules whose action did nothing in batch runs before the unified
/// executor and now runs (add_tag, snooze, unsubscribe). `rule run` lists
/// these before its confirm gate.
pub fn newly_live_actions(rules: &[Rule]) -> Vec<Value> {
    rules
        .iter()
        .filter(|r| r.enabled)
        .filter_map(|r| {
            let stored = StoredRuleAction::parse(&r.action).ok()?;
            stored.action.became_live_in_unified_executor().then(|| {
                json!({
                    "rule": r.name,
                    "rule_id": r.id,
                    "action": stored.action.kind(),
                    "gated": stored.gate_skip_reason().is_some(),
                })
            })
        })
        .collect()
}

/// Mark a rule's snooze/unsubscribe action as acknowledged so batch runs
/// execute it. Returns the rewritten stored action.
pub fn acknowledge_batch_actions(db: &Database, rule_id: &str) -> Result<StoredRuleAction> {
    let rule = db
        .get_rule(rule_id)
        .context("database error")?
        .ok_or_else(|| anyhow!("rule {rule_id} not found"))?;
    let mut stored = StoredRuleAction::parse(&rule.action)
        .with_context(|| format!("rule '{}' has an invalid action", rule.name))?;
    if stored.action.requires_batch_acknowledgement() && !stored.acknowledged_batch_actions {
        stored.acknowledged_batch_actions = true;
        let json = stored.to_json().context("failed to serialize action")?;
        db.set_rule_action(rule_id, &json)
            .context("failed to record acknowledgement")?;
    }
    Ok(stored)
}

/// Parse an authored action, flattening `confirm` rule references (by id or
/// name, within `account_id`) into concrete allowlisted actions.
pub fn flatten_authored_action(
    db: &Database,
    account_id: &str,
    raw: &Value,
) -> std::result::Result<Action, String> {
    rules::parse_authored_action(raw, |reference| {
        let by_id = db
            .get_rule(reference)
            .ok()
            .flatten()
            .filter(|r| r.account_id == account_id);
        by_id
            .or_else(|| db.find_rule_by_name(account_id, reference).ok().flatten())
            .map(|r| r.action)
    })
}

/// Build a rule-evaluation context from a header-only summary plus the local
/// tag/score/contact stores. No body fetch.
pub fn build_summary_context(
    summary: &MessageSummary,
    db: &Database,
    account_id: &str,
) -> Result<MessageContext> {
    // Canonicalize so summary/full/persistence keys agree (IMAP ENVELOPE ids
    // arrive bracketed; persisted scores/tags use the bare id).
    let message_id =
        envelope_email_store::canonical_message_id(summary.message_id.as_deref().unwrap_or(""));

    let tags: Vec<String> = if message_id.is_empty() {
        Vec::new()
    } else {
        db.get_tags(account_id, message_id)
            .context("failed to get tags")?
            .into_iter()
            .map(|t| t.tag)
            .collect()
    };

    let mut scores: HashMap<String, f64> = if message_id.is_empty() {
        HashMap::new()
    } else {
        db.get_scores(account_id, message_id)
            .context("failed to get scores")?
            .into_iter()
            .map(|s| (s.dimension, s.value))
            .collect()
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

/// Display-safe action JSON: webhook URLs are redacted.
pub fn sanitized_action(action: &Action) -> Value {
    match action {
        Action::Webhook(_) => json!({"webhook": "[redacted]"}),
        other => serde_json::to_value(other).unwrap_or(Value::Null),
    }
}

/// Display-safe form of a stored `rules.action` string.
pub fn sanitized_stored_action(raw: &str) -> Value {
    match StoredRuleAction::parse(raw) {
        Ok(stored) => sanitized_action(&stored.action),
        Err(_) => Value::String("[invalid action]".to_string()),
    }
}

fn rule_event_id(rule: &Rule, target: &MessageTarget<'_>) -> String {
    format!("rule:{}:{}", rule.id, target.message_key())
}

/// Execute one action against one message and record it.
///
/// Returns [`ExecStatus::AlreadyApplied`] without touching the mailbox when
/// `action_log` already holds a completed row for this action's event.
/// A `confirm` action never runs its `then` list here: it records an
/// `action_offered` event (after re-validating the list) and returns
/// [`ExecStatus::Offered`].
pub async fn execute_action<M: RuleMailbox, D: ExecDb>(
    mbox: &mut M,
    db: &D,
    target: &MessageTarget<'_>,
    action: &Action,
    rule: Option<&Rule>,
    attribution: &ActionAttribution,
) -> Result<ExecOutcome> {
    if let Some(skip) = action.local_execution_skip_reason() {
        return Ok(ExecOutcome::new(
            ExecStatus::Skipped,
            format!("skipped: {skip}"),
        ));
    }

    let event_id = attribution
        .event_id
        .clone()
        .or_else(|| rule.map(|r| rule_event_id(r, target)));
    let action_type = action.kind();

    if let Some(event_id) = &event_id {
        let existing = db
            .with_db(|d| d.get_action_by_event(event_id, action_type))
            .await
            .context("failed to check action log")?;
        if let Some(existing) = existing
            && existing.action_status == "completed"
        {
            return Ok(ExecOutcome::new(
                ExecStatus::AlreadyApplied,
                format!("already applied ({})", existing.id),
            ));
        }
    }

    if let Action::Confirm { prompt, then } = action {
        return record_offer(db, target, prompt, then, rule, attribution, event_id).await;
    }

    let description = perform(mbox, db, target, action, rule).await?;
    record_action(
        db,
        target,
        action,
        rule,
        attribution,
        event_id.as_deref(),
        &description,
        json!({}),
    )
    .await?;
    Ok(ExecOutcome::new(ExecStatus::Executed, description))
}

async fn perform<M: RuleMailbox, D: ExecDb>(
    mbox: &mut M,
    db: &D,
    target: &MessageTarget<'_>,
    action: &Action,
    rule: Option<&Rule>,
) -> Result<String> {
    let (folder, uid) = (target.folder, target.uid);
    match action {
        Action::Move(dest) => {
            let real = mbox.resolve_folder(dest).await?;
            mbox.move_message(folder, uid, &real).await?;
            Ok(format!("moved to {real}"))
        }
        Action::Flag(flag) => {
            mbox.set_flag(folder, uid, flag).await?;
            record_own_flag(db, target, flag, true).await?;
            Ok(format!("flagged {flag}"))
        }
        Action::Unflag(flag) => {
            mbox.remove_flag(folder, uid, flag).await?;
            record_own_flag(db, target, flag, false).await?;
            Ok(format!("unflagged {flag}"))
        }
        Action::Delete => {
            mbox.delete_message(folder, uid).await?;
            Ok("deleted".to_string())
        }
        Action::AddTag(tag) => {
            let Some(message_id) = target.message_id.filter(|m| !m.is_empty()) else {
                bail!("add_tag:{tag} needs a Message-ID; UID {uid} in {folder} has none");
            };
            db.with_db(|d| {
                d.add_tag(
                    target.account_id,
                    message_id,
                    tag,
                    Some(i64::from(uid)),
                    Some(folder),
                )
            })
            .await
            .with_context(|| format!("failed to add tag '{tag}' to UID {uid}"))?;
            Ok(format!("tagged {tag}"))
        }
        Action::Snooze(until) => {
            let return_at = crate::snooze_time::parse_until(until)
                .with_context(|| format!("invalid snooze time '{until}'"))?;
            let already = db
                .with_db(|d| d.find_snoozed_by_uid(target.account_email, uid))
                .await
                .context("failed to check snooze state")?;
            if let Some(existing) = already {
                bail!(
                    "UID {uid} is already snoozed (returns at {})",
                    existing.return_at
                );
            }
            if let Err(e) = mbox.ensure_folder(SNOOZED_FOLDER).await {
                warn!("could not create {SNOOZED_FOLDER} folder (may already exist): {e:#}");
            }
            mbox.move_message(folder, uid, SNOOZED_FOLDER).await?;
            let note = rule.map(|r| format!("rule: {}", r.name));
            db.with_db(|d| {
                d.create_snoozed(
                    target.account_email,
                    uid,
                    folder,
                    SNOOZED_FOLDER,
                    &return_at,
                    target.message_id,
                    Some(target.ctx.subject.as_str()),
                    None,
                    note.as_deref(),
                    None,
                )
            })
            .await
            .context("moved to Snoozed but failed to record the snooze")?;
            Ok(format!("snoozed until {return_at}"))
        }
        Action::Unsubscribe => {
            let (header, post) = mbox.list_unsubscribe_headers(folder, uid).await?;
            let header = header
                .ok_or_else(|| anyhow!("no List-Unsubscribe header on UID {uid} ({folder})"))?;
            let info = crate::unsubscribe::parse_list_unsubscribe(&header, post.as_deref())
                .ok_or_else(|| {
                    anyhow!("unparseable List-Unsubscribe header on UID {uid} ({folder})")
                })?;
            // One-click HTTPS only: a mailto unsubscribe is a governed send and
            // must go through `envelope unsubscribe --confirm --attr ...`.
            let Some(result) = crate::unsubscribe::one_click_post(&info).await else {
                bail!(
                    "no successful one-click unsubscribe for UID {uid} ({folder}); a mailto or \
                     non-one-click unsubscribe needs 'envelope unsubscribe {uid} --folder {folder} \
                     --confirm --attr ...' (Governor-gated)"
                );
            };
            let junk = mbox.resolve_folder("\\Junk").await?;
            mbox.move_message(folder, uid, &junk).await?;
            Ok(format!(
                "unsubscribed via {}; moved to {junk}",
                result.method
            ))
        }
        Action::Webhook(url) => {
            let payload = json!({
                "event": "rule_matched",
                "rule": rule.map(|r| r.name.as_str()).unwrap_or("unknown"),
                "uid": uid,
                "folder": folder,
                "message": {
                    "from": target.ctx.from_addr,
                    "to": target.ctx.to_addr,
                    "subject": target.ctx.subject,
                }
            });
            let body = serde_json::to_vec(&payload)
                .map_err(|e| anyhow!("failed to serialize webhook payload: {e}"))?;
            let (http, target_url) = crate::http::client_for(url, &Allowance::Public)
                .await
                .map_err(|e| anyhow!("egress refused: {e}"))?;
            let resp = http
                .post(target_url)
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
                .map_err(|_| anyhow!("webhook delivery failed"))?;
            Ok(format!("webhook delivered: {}", resp.status()))
        }
        Action::Reject(_) | Action::Ereject(_) => {
            Ok(format!("skipped: {}", rules::SERVER_SIDE_ONLY_SKIP_REASON))
        }
        Action::Confirm { .. } => bail!("confirm offers are recorded, never performed directly"),
    }
}

/// Patch the local index after Envelope's own flag STORE succeeded, so the
/// change never reads as another client's read.
async fn record_own_flag<D: ExecDb>(
    db: &D,
    target: &MessageTarget<'_>,
    flag: &str,
    add: bool,
) -> Result<()> {
    let (folder, uid) = (target.folder, target.uid);
    db.with_db(|d| imap::record_own_flag_change(d, target.account_id, folder, &[uid], flag, add))
        .await
        .with_context(|| {
            format!(
                "flag '{flag}' changed on UID {uid}, but updating the local message index failed"
            )
        })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn record_action<D: ExecDb>(
    db: &D,
    target: &MessageTarget<'_>,
    action: &Action,
    rule: Option<&Rule>,
    attribution: &ActionAttribution,
    event_id: Option<&str>,
    description: &str,
    extra: Value,
) -> Result<ActionLog> {
    let mut taken = json!({
        "source": attribution.source.as_str(),
        "rule_id": rule.map(|r| r.id.as_str()),
        "rule_name": rule.map(|r| r.name.as_str()),
        "action": sanitized_action(action),
        "result": description,
        "folder": target.folder,
        "uid": target.uid,
    });
    if let (Some(obj), Value::Object(extra)) = (taken.as_object_mut(), extra) {
        obj.extend(extra);
    }
    let taken = taken.to_string();
    let justification = match rule {
        Some(r) => format!("{}: rule '{}'", attribution.source.as_str(), r.name),
        None => format!("{} action", attribution.source.as_str()),
    };
    let agent_id = attribution.agent_id.as_deref();
    db.with_db(|d| match event_id {
        Some(event_id) => d.log_action_for_event_with_agent(
            EventActionLogInput {
                account_id: target.account_id,
                event_id,
                action_type: action.kind(),
                confidence: 1.0,
                justification: &justification,
                action_taken: &taken,
                action_status: "completed",
                message_id: target.message_id,
                draft_id: None,
            },
            agent_id,
        ),
        None => d.log_action_with_agent(
            target.account_id,
            action.kind(),
            1.0,
            &justification,
            &taken,
            target.message_id,
            None,
            agent_id,
        ),
    })
    .await
    .context("action ran but writing the action log failed")
}

async fn record_offer<D: ExecDb>(
    db: &D,
    target: &MessageTarget<'_>,
    prompt: &str,
    then: &[ConfirmableAction],
    rule: Option<&Rule>,
    attribution: &ActionAttribution,
    event_id: Option<String>,
) -> Result<ExecOutcome> {
    for action in then {
        action
            .validate()
            .map_err(|e| anyhow!("confirm offer failed allowlist re-validation: {e}"))?;
    }
    if then.is_empty() {
        bail!("confirm offer has no actions");
    }
    let idempotency_key = event_id
        .as_deref()
        .map(|id| format!("{}:{ACTION_OFFERED}:{id}", target.account_id));
    let payload = OfferPayload {
        rule_id: rule.map(|r| r.id.clone()),
        rule_name: rule.map(|r| r.name.clone()),
        prompt: prompt.to_string(),
        actions: then.to_vec(),
    };
    let event = Event {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: target.account_id.to_string(),
        event_type: ACTION_OFFERED.to_string(),
        folder: target.folder.to_string(),
        uid: Some(i64::from(target.uid)),
        message_id: target.message_id.map(str::to_string),
        from_addr: Some(target.ctx.from_addr.clone()),
        subject: Some(crate::code_extractor::redact_codes(&target.ctx.subject)),
        snippet: None,
        payload: Some(serde_json::to_string(&payload).context("failed to serialize offer")?),
        idempotency_key: idempotency_key.clone(),
        secure_pending: false,
        acked_at: None,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let agent_id = attribution.agent_id.as_deref();
    let offer_id = db
        .with_db(|d| -> Result<String> {
            if d.insert_event_idempotent_with_agent(&event, agent_id)? {
                return Ok(event.id.clone());
            }
            let key = idempotency_key
                .as_deref()
                .ok_or_else(|| anyhow!("offer event insert was ignored without a key"))?;
            Ok(d.get_event_by_idempotency_key(key)?
                .ok_or_else(|| anyhow!("offer event vanished after duplicate insert"))?
                .id)
        })
        .await
        .context("failed to record action_offered event")?;

    let action = Action::Confirm {
        prompt: prompt.to_string(),
        then: then.to_vec(),
    };
    let description = format!("offered {} action(s): {prompt}", then.len());
    record_action(
        db,
        target,
        &action,
        rule,
        attribution,
        event_id.as_deref(),
        &description,
        json!({"offer_event_id": offer_id}),
    )
    .await?;
    info!("recorded action_offered {offer_id}");
    Ok(ExecOutcome {
        status: ExecStatus::Offered,
        description,
        offer_event_id: Some(offer_id),
    })
}

/// The `action_offered` event payload. Deserializing it re-runs the
/// [`ConfirmableAction`] allowlist, so a tampered row fails closed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OfferPayload {
    pub rule_id: Option<String>,
    pub rule_name: Option<String>,
    pub prompt: String,
    pub actions: Vec<ConfirmableAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OfferResolution {
    Confirmed,
    Dismissed,
}

#[derive(Debug, Clone, Serialize)]
pub struct OfferReport {
    pub event_id: String,
    pub resolution: OfferResolution,
    /// False when the offer was already in this state and nothing changed.
    pub changed: bool,
    pub actions: Vec<Value>,
}

fn load_offer_event(db: &Database, event_id: &str) -> Result<Event> {
    let event = db
        .get_event(event_id)
        .context("failed to load event")?
        .ok_or_else(|| anyhow!("event not found: {event_id}"))?;
    if event.event_type != ACTION_OFFERED {
        bail!(
            "event {event_id} is a {} event, not {ACTION_OFFERED}",
            event.event_type
        );
    }
    Ok(event)
}

fn existing_resolution(db: &Database, event_id: &str) -> Result<Option<OfferResolution>> {
    let Some(row) = db
        .get_action_by_event(event_id, OFFER_RESOLUTION)
        .context("failed to read offer resolution")?
    else {
        return Ok(None);
    };
    let value: Value = serde_json::from_str(&row.action_taken).unwrap_or(Value::Null);
    match value.get("resolution").and_then(Value::as_str) {
        Some("confirmed") => Ok(Some(OfferResolution::Confirmed)),
        Some("dismissed") => Ok(Some(OfferResolution::Dismissed)),
        other => bail!("offer {event_id} has an unreadable resolution {other:?}"),
    }
}

/// Claim the offer's single resolution row. Returns the resolution that
/// actually holds (another caller may have won the UNIQUE race).
fn claim_resolution(
    db: &Database,
    event: &Event,
    resolution: OfferResolution,
    attribution: &ActionAttribution,
) -> Result<OfferResolution> {
    let label = match resolution {
        OfferResolution::Confirmed => "confirmed",
        OfferResolution::Dismissed => "dismissed",
    };
    let taken = json!({"resolution": label, "source": attribution.source.as_str()}).to_string();
    db.log_action_for_event_with_agent(
        EventActionLogInput {
            account_id: &event.account_id,
            event_id: &event.id,
            action_type: OFFER_RESOLUTION,
            confidence: 1.0,
            justification: &format!("offer {label} via {}", attribution.source.as_str()),
            action_taken: &taken,
            action_status: "completed",
            message_id: event.message_id.as_deref(),
            draft_id: None,
        },
        attribution.agent_id.as_deref(),
    )
    .context("failed to record offer resolution")?;
    existing_resolution(db, &event.id)?
        .ok_or_else(|| anyhow!("offer resolution missing after insert"))
}

/// Execute an offer's actions. Idempotent: each action is keyed
/// `<offer>:<index>` in `action_log`, so a second confirm re-runs nothing
/// that already completed. A dismissed offer is never executed.
pub async fn confirm_offer<M: RuleMailbox>(
    mbox: &mut M,
    db: &Database,
    event_id: &str,
    account_email: &str,
    attribution: &ActionAttribution,
) -> Result<OfferReport> {
    let event = load_offer_event(db, event_id)?;
    let payload: OfferPayload = serde_json::from_str(event.payload.as_deref().unwrap_or(""))
        .map_err(|e| {
            anyhow!("offer {event_id} failed allowlist re-validation; nothing executed: {e}")
        })?;
    for action in &payload.actions {
        action.validate().map_err(|e| {
            anyhow!("offer {event_id} failed allowlist re-validation; nothing executed: {e}")
        })?;
    }
    let uid = event
        .uid
        .and_then(|u| u32::try_from(u).ok())
        .ok_or_else(|| anyhow!("offer {event_id} has no message UID"))?;

    let previous = existing_resolution(db, event_id)?;
    if previous == Some(OfferResolution::Dismissed) {
        bail!("offer {event_id} was dismissed; not executing");
    }
    if previous.is_none()
        && claim_resolution(db, &event, OfferResolution::Confirmed, attribution)?
            == OfferResolution::Dismissed
    {
        bail!("offer {event_id} was dismissed; not executing");
    }

    let ctx = MessageContext {
        from_addr: event.from_addr.clone().unwrap_or_default(),
        to_addr: String::new(),
        subject: event.subject.clone().unwrap_or_default(),
        tags: Vec::new(),
        scores: HashMap::new(),
        contact_tags: Vec::new(),
    };
    let target = MessageTarget {
        account_id: &event.account_id,
        account_email,
        folder: &event.folder,
        uid,
        message_id: event.message_id.as_deref(),
        ctx: &ctx,
    };

    // Moves last: they change the message's folder/UID out from under the
    // flag and tag steps.
    let mut order: Vec<usize> = (0..payload.actions.len()).collect();
    order.sort_by_key(|&i| matches!(payload.actions[i], ConfirmableAction::Move(_)));

    let mut results = Vec::new();
    let mut changed = previous.is_none();
    for i in order {
        let action = payload.actions[i].clone().into_action();
        let attr = ActionAttribution {
            source: attribution.source,
            agent_id: attribution.agent_id.clone(),
            event_id: Some(format!("{event_id}:{i}")),
        };
        let outcome = execute_action(mbox, db, &target, &action, None, &attr).await?;
        changed |= outcome.status == ExecStatus::Executed;
        results.push(json!({
            "index": i,
            "action": sanitized_action(&action),
            "status": outcome.status,
            "result": outcome.description,
        }));
    }
    db.mark_acked(event_id)
        .context("failed to ack offer event")?;
    Ok(OfferReport {
        event_id: event_id.to_string(),
        resolution: OfferResolution::Confirmed,
        changed,
        actions: results,
    })
}

/// Dismiss an offer without executing anything. Idempotent; refuses an offer
/// that was already confirmed.
pub fn dismiss_offer(
    db: &Database,
    event_id: &str,
    attribution: &ActionAttribution,
) -> Result<OfferReport> {
    let event = load_offer_event(db, event_id)?;
    let previous = existing_resolution(db, event_id)?;
    let resolution = match previous {
        Some(r) => r,
        None => claim_resolution(db, &event, OfferResolution::Dismissed, attribution)?,
    };
    if resolution == OfferResolution::Confirmed {
        bail!("offer {event_id} was already confirmed; dismiss has no effect");
    }
    db.mark_acked(event_id)
        .context("failed to ack offer event")?;
    Ok(OfferReport {
        event_id: event_id.to_string(),
        resolution,
        changed: previous.is_none(),
        actions: Vec::new(),
    })
}

/// Result of running a rule set over messages.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RuleRunReport {
    pub processed: usize,
    /// Actions that actually executed (offers and replays excluded).
    pub actions: u32,
    pub log: Vec<Value>,
    pub skipped_rules: Vec<SkippedRule>,
}

/// Run `rules` (in priority order) against one message: evaluate, execute
/// through [`execute_action`], bump hit counts, and write `rule_run_audit`.
/// Stops after a rule that moves the message away or has `stop` set.
pub async fn run_rules_on_message<M: RuleMailbox, D: ExecDb>(
    mbox: &mut M,
    db: &D,
    target: &MessageTarget<'_>,
    rules: &[LoadedRule],
    attribution: &ActionAttribution,
    report: &mut RuleRunReport,
) {
    for loaded in rules {
        if !rules::evaluate(&loaded.match_expr, target.ctx) {
            continue;
        }
        let rule = &loaded.rule;
        let result =
            execute_action(mbox, db, target, &loaded.action, Some(rule), attribution).await;
        let (status, desc, error) = match &result {
            Ok(outcome) => {
                let status = match outcome.status {
                    ExecStatus::Executed => "ok",
                    ExecStatus::AlreadyApplied => "already_applied",
                    ExecStatus::Offered => "offered",
                    ExecStatus::Skipped => "skipped",
                };
                (status, Some(outcome.description.clone()), None)
            }
            Err(e) => ("error", None, Some(format!("{e:#}"))),
        };
        let fired = matches!(
            result.as_ref().map(|o| o.status),
            Ok(ExecStatus::Executed | ExecStatus::Offered)
        );
        if fired {
            info!(
                "rule '{}' fired on UID {}: {}",
                rule.name,
                target.uid,
                desc.as_deref().unwrap_or("")
            );
        }
        if matches!(result.as_ref().map(|o| o.status), Ok(ExecStatus::Executed)) {
            report.actions += 1;
        }
        let audit = db
            .with_db(|d| -> envelope_email_store::errors::Result<()> {
                if fired {
                    d.increment_rule_hit(&rule.id)?;
                }
                d.record_rule_run(RuleRunAuditInput {
                    account_id: target.account_id,
                    rule_id: Some(&rule.id),
                    rule_name: Some(&rule.name),
                    uid: Some(i64::from(target.uid)),
                    folder: Some(target.folder),
                    action: desc.as_deref(),
                    status,
                    error: error.as_deref(),
                })?;
                Ok(())
            })
            .await;
        if let Err(e) = audit {
            warn!("failed to write rule_run_audit for '{}': {e}", rule.name);
        }
        let mut entry = json!({
            "uid": target.uid,
            "rule": rule.name,
            "status": status,
        });
        if let Some(obj) = entry.as_object_mut() {
            match (&desc, &error) {
                (_, Some(err)) => {
                    obj.insert("error".to_string(), json!(err));
                }
                (Some(desc), None) => {
                    obj.insert("action".to_string(), json!(desc));
                }
                (None, None) => {}
            }
            if let Ok(ExecOutcome {
                offer_event_id: Some(id),
                ..
            }) = &result
            {
                obj.insert("offer_event_id".to_string(), json!(id));
            }
        }
        report.log.push(entry);

        let leaves_folder = matches!(
            loaded.action,
            Action::Move(_) | Action::Delete | Action::Snooze(_) | Action::Unsubscribe
        );
        if leaves_folder || rule.stop {
            break;
        }
    }
}

/// The account a batch runs for.
pub struct RunAccount<'a> {
    pub id: &'a str,
    /// Login address (the snooze table's account key).
    pub email: &'a str,
}

/// Apply every enabled, gate-passing rule to header-only `summaries` from
/// `folder`. Shared by `envelope rule run`, MCP `rules_run`, the dashboard,
/// and `envelope watch --run-rules`.
pub async fn apply_rules_to_summaries<M: RuleMailbox, D: ExecDb>(
    mbox: &mut M,
    db: &D,
    account: &RunAccount<'_>,
    folder: &str,
    summaries: &[MessageSummary],
    attribution: &ActionAttribution,
) -> Result<RuleRunReport> {
    let (rules, skipped_rules) = db.with_db(|d| load_enabled_rules(d, account.id)).await?;
    let mut report = RuleRunReport {
        processed: summaries.len(),
        skipped_rules,
        ..RuleRunReport::default()
    };
    if rules.is_empty() {
        return Ok(report);
    }
    for summary in summaries {
        let ctx = db
            .with_db(|d| build_summary_context(summary, d, account.id))
            .await?;
        let message_id = summary
            .message_id
            .as_deref()
            .map(envelope_email_store::canonical_message_id)
            .filter(|m| !m.is_empty());
        let target = MessageTarget {
            account_id: account.id,
            account_email: account.email,
            folder,
            uid: summary.uid,
            message_id,
            ctx: &ctx,
        };
        run_rules_on_message(mbox, db, &target, &rules, attribution, &mut report).await;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records every mailbox call; never touches the network.
    #[derive(Default)]
    struct FakeMailbox {
        calls: Vec<String>,
    }

    impl RuleMailbox for FakeMailbox {
        async fn resolve_folder(&mut self, dest: &str) -> Result<String> {
            Ok(match dest {
                "\\Junk" => "Junk".to_string(),
                "\\Archive" => "Archive".to_string(),
                other => other.to_string(),
            })
        }
        async fn move_message(&mut self, folder: &str, uid: u32, dest: &str) -> Result<()> {
            self.calls.push(format!("move {folder}/{uid} -> {dest}"));
            Ok(())
        }
        async fn set_flag(&mut self, folder: &str, uid: u32, flag: &str) -> Result<()> {
            self.calls.push(format!("flag {folder}/{uid} {flag}"));
            Ok(())
        }
        async fn remove_flag(&mut self, folder: &str, uid: u32, flag: &str) -> Result<()> {
            self.calls.push(format!("unflag {folder}/{uid} {flag}"));
            Ok(())
        }
        async fn delete_message(&mut self, folder: &str, uid: u32) -> Result<()> {
            self.calls.push(format!("delete {folder}/{uid}"));
            Ok(())
        }
        async fn ensure_folder(&mut self, name: &str) -> Result<()> {
            self.calls.push(format!("create {name}"));
            Ok(())
        }
        async fn list_unsubscribe_headers(
            &mut self,
            _folder: &str,
            _uid: u32,
        ) -> Result<(Option<String>, Option<String>)> {
            Ok((None, None))
        }
    }

    const ACCT: &str = "acct-1";
    const EMAIL: &str = "me@example.com";

    fn account() -> RunAccount<'static> {
        RunAccount {
            id: ACCT,
            email: EMAIL,
        }
    }

    fn summary(uid: u32, mid: &str, from: &str) -> MessageSummary {
        MessageSummary {
            uid,
            message_id: Some(format!("<{mid}>")),
            from_addr: from.to_string(),
            to_addr: EMAIL.to_string(),
            subject: "Your flight itinerary".to_string(),
            date: None,
            flags: vec![],
            size: 100,
            provider_spam: None,
        }
    }

    fn rule(db: &Database, name: &str, action: &str) -> Rule {
        db.create_rule(
            ACCT,
            name,
            r#"{"from":"*@airline.example"}"#,
            action,
            100,
            false,
        )
        .unwrap()
    }

    fn action_rows(db: &Database) -> Vec<ActionLog> {
        db.list_actions(ACCT, 100).unwrap()
    }

    async fn run(
        db: &Database,
        mbox: &mut FakeMailbox,
        summaries: &[MessageSummary],
    ) -> RuleRunReport {
        apply_rules_to_summaries(
            mbox,
            db,
            &account(),
            "INBOX",
            summaries,
            &ActionAttribution::new(ActionSource::Rule),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn add_tag_persists_a_tag_and_writes_attributed_action_log() {
        let db = Database::open_memory().unwrap();
        rule(&db, "travel", r#"{"add_tag":"travel"}"#);
        let mut mbox = FakeMailbox::default();

        let report = run(
            &db,
            &mut mbox,
            &[summary(7, "trip@airline.example", "x@airline.example")],
        )
        .await;

        assert_eq!(report.actions, 1, "{report:?}");
        let tags = db.get_tags(ACCT, "trip@airline.example").unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].tag, "travel");
        assert_eq!(tags[0].uid, Some(7));
        let rows = action_rows(&db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action_type, "add_tag");
        assert!(rows[0].event_id.as_deref().unwrap().starts_with("rule:"));
        let taken: Value = serde_json::from_str(&rows[0].action_taken).unwrap();
        assert_eq!(taken["source"], "rule");
        assert_eq!(taken["rule_name"], "travel");
        assert!(mbox.calls.is_empty(), "add_tag must not touch IMAP");
    }

    #[tokio::test]
    async fn replaying_a_rule_yields_one_action_log_row_and_one_mutation() {
        let db = Database::open_memory().unwrap();
        let r = rule(&db, "flagger", r#"{"flag":"flagged"}"#);
        let mut mbox = FakeMailbox::default();
        let msgs = [summary(3, "a@airline.example", "x@airline.example")];

        let first = run(&db, &mut mbox, &msgs).await;
        let second = run(&db, &mut mbox, &msgs).await;

        assert_eq!(first.actions, 1);
        assert_eq!(second.actions, 0);
        assert_eq!(second.log[0]["status"], "already_applied");
        assert_eq!(mbox.calls, vec!["flag INBOX/3 flagged".to_string()]);
        assert_eq!(action_rows(&db).len(), 1);
        assert_eq!(db.get_rule(&r.id).unwrap().unwrap().hit_count, 1);
        let audits = db.list_rule_runs(Some(ACCT), 10).unwrap();
        assert_eq!(audits.len(), 2, "rule_run_audit keeps recording every pass");
    }

    #[tokio::test]
    async fn rule_flag_patches_the_local_index() {
        let db = Database::open_memory().unwrap();
        db.upsert_indexed_message_summaries(
            ACCT,
            "INBOX",
            1,
            &[envelope_email_store::models::IndexedMessageInput {
                uid: 3,
                message_id: Some("<a@airline.example>".into()),
                from_addr: "x@airline.example".into(),
                to_addr: EMAIL.into(),
                subject: "Your flight itinerary".into(),
                date: None,
                flags: vec![],
                size: 100,
                snippet: None,
                thread_id: None,
            }],
        )
        .unwrap();
        rule(&db, "reader", r#"{"flag":"seen"}"#);
        let mut mbox = FakeMailbox::default();

        let report = run(
            &db,
            &mut mbox,
            &[summary(3, "a@airline.example", "x@airline.example")],
        )
        .await;

        assert_eq!(report.actions, 1, "{report:?}");
        let seen = imap::index_flag_name("seen");
        assert_eq!(
            db.patch_indexed_message_flags(ACCT, "INBOX", &[3], &seen, true)
                .unwrap(),
            0,
            "the executor already patched the index"
        );
    }

    #[tokio::test]
    async fn rule_webhook_to_loopback_is_refused_before_connecting() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let db = Database::open_memory().unwrap();
        rule(
            &db,
            "hook",
            &serde_json::to_string(&json!({"webhook": format!("http://{addr}/hook")})).unwrap(),
        );
        let mut mbox = FakeMailbox::default();

        let report = run(
            &db,
            &mut mbox,
            &[summary(4, "w@airline.example", "x@airline.example")],
        )
        .await;

        assert_eq!(report.actions, 0, "{report:?}");
        assert_eq!(report.log[0]["status"], "error");
        let err = report.log[0]["error"].as_str().unwrap();
        assert!(err.contains("egress refused"), "{err}");
        assert!(
            action_rows(&db).is_empty(),
            "a refused webhook is not a completed action"
        );
        let accepted =
            tokio::time::timeout(std::time::Duration::from_millis(200), listener.accept()).await;
        assert!(
            accepted.is_err(),
            "no connection may reach the private target"
        );
    }

    #[tokio::test]
    async fn unacknowledged_snooze_rule_is_skipped_then_runs_after_acknowledge() {
        let db = Database::open_memory().unwrap();
        let r = rule(&db, "later", r#"{"snooze":"1d"}"#);
        let mut mbox = FakeMailbox::default();
        let msgs = [summary(9, "s@airline.example", "x@airline.example")];

        let gated = run(&db, &mut mbox, &msgs).await;
        assert_eq!(gated.actions, 0);
        assert_eq!(
            gated.skipped_rules,
            vec![SkippedRule {
                rule_id: r.id.clone(),
                rule_name: "later".to_string(),
                reason: rules::BATCH_ACTIONS_UNACKNOWLEDGED_REASON.to_string(),
            }]
        );
        assert!(mbox.calls.is_empty());
        assert!(db.find_snoozed_by_uid(EMAIL, 9).unwrap().is_none());

        acknowledge_batch_actions(&db, &r.id).unwrap();
        let live = run(&db, &mut mbox, &msgs).await;
        assert!(live.skipped_rules.is_empty());
        assert_eq!(live.actions, 1, "{live:?}");
        assert!(mbox.calls.contains(&"move INBOX/9 -> Snoozed".to_string()));
        let snoozed = db.find_snoozed_by_uid(EMAIL, 9).unwrap().unwrap();
        assert_eq!(snoozed.original_folder, "INBOX");
    }

    #[tokio::test]
    async fn unsubscribe_rule_is_gated_until_acknowledged() {
        let db = Database::open_memory().unwrap();
        rule(&db, "unsub", r#""unsubscribe""#);
        let mut mbox = FakeMailbox::default();
        let report = run(
            &db,
            &mut mbox,
            &[summary(1, "n@airline.example", "x@airline.example")],
        )
        .await;
        assert_eq!(report.skipped_rules.len(), 1);
        assert_eq!(
            report.skipped_rules[0].reason,
            rules::BATCH_ACTIONS_UNACKNOWLEDGED_REASON
        );
        assert!(mbox.calls.is_empty());
    }

    #[test]
    fn split_evaluable_rules_reports_empty_and_unparseable_matches() {
        let db = Database::open_memory().unwrap();
        for (priority, (name, match_expr)) in [
            ("normal", r#"{"from":"*@airline.example"}"#),
            ("empty-and", r#"{"and":[]}"#),
            ("nested-or", r#"{"or":[{"from":"*@x.example"},{"or":[]}]}"#),
            ("not-empty-or", r#"{"not":{"or":[]}}"#),
            ("broken", "not json"),
        ]
        .into_iter()
        .enumerate()
        {
            db.create_rule(
                ACCT,
                name,
                match_expr,
                r#""delete""#,
                priority as i64,
                false,
            )
            .unwrap();
        }

        let (evaluable, skipped) = split_evaluable_rules(db.list_rules(ACCT).unwrap());

        assert_eq!(evaluable.len(), 1, "{evaluable:?}");
        assert_eq!(evaluable[0].0.name, "normal");
        assert_eq!(
            evaluable[0].1,
            MatchExpr::From("*@airline.example".to_string())
        );
        let skipped: Vec<(&str, &str)> = skipped
            .iter()
            .map(|s| (s.rule_name.as_str(), s.reason.as_str()))
            .collect();
        assert_eq!(
            skipped[..3],
            [
                ("empty-and", rules::EMPTY_CONDITION_LIST_SKIP_REASON),
                ("nested-or", rules::EMPTY_CONDITION_LIST_SKIP_REASON),
                ("not-empty-or", rules::EMPTY_CONDITION_LIST_SKIP_REASON),
            ]
        );
        assert_eq!(skipped.len(), 4, "{skipped:?}");
        assert_eq!(skipped[3].0, "broken");
        assert!(
            skipped[3].1.starts_with("invalid match_expr: "),
            "{skipped:?}"
        );
    }

    #[tokio::test]
    async fn stored_rule_with_empty_condition_list_is_skipped_never_applied() {
        let db = Database::open_memory().unwrap();
        for (name, match_expr) in [
            ("oops", r#"{"and":[]}"#),
            ("nested", r#"{"not":{"or":[]}}"#),
        ] {
            db.create_rule(ACCT, name, match_expr, r#""delete""#, 100, false)
                .unwrap();
        }
        let mut mbox = FakeMailbox::default();

        let report = run(
            &db,
            &mut mbox,
            &[summary(5, "any@elsewhere.example", "x@elsewhere.example")],
        )
        .await;

        assert_eq!(report.actions, 0, "{report:?}");
        assert!(report.log.is_empty(), "{report:?}");
        assert!(mbox.calls.is_empty(), "no message may be deleted");
        assert!(action_rows(&db).is_empty());
        let reasons: Vec<&str> = report
            .skipped_rules
            .iter()
            .map(|s| s.reason.as_str())
            .collect();
        assert_eq!(
            reasons,
            vec![rules::EMPTY_CONDITION_LIST_SKIP_REASON; 2],
            "{report:?}"
        );
    }

    fn confirm_rule(db: &Database) -> Rule {
        rule(
            db,
            "trip-offer",
            r#"{"confirm":{"prompt":"Looks like a trip","then":[{"add_tag":"travel"},{"move":"Travel"}]}}"#,
        )
    }

    #[tokio::test]
    async fn confirm_rule_emits_an_offer_and_executes_nothing() {
        let db = Database::open_memory().unwrap();
        let r = confirm_rule(&db);
        let mut mbox = FakeMailbox::default();
        let msgs = [summary(4, "t@airline.example", "x@airline.example")];

        let report = run(&db, &mut mbox, &msgs).await;
        assert_eq!(report.actions, 0);
        assert_eq!(report.log[0]["status"], "offered");
        let offer_id = report.log[0]["offer_event_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(mbox.calls.is_empty(), "then must not run: {:?}", mbox.calls);
        assert!(db.get_tags(ACCT, "t@airline.example").unwrap().is_empty());

        let event = db.get_event(&offer_id).unwrap().unwrap();
        assert_eq!(event.event_type, ACTION_OFFERED);
        assert_eq!(event.uid, Some(4));
        let payload: Value = serde_json::from_str(event.payload.as_deref().unwrap()).unwrap();
        assert_eq!(payload["rule_id"], r.id.as_str());
        assert_eq!(payload["prompt"], "Looks like a trip");
        assert_eq!(
            payload["actions"],
            json!([{"add_tag": "travel"}, {"move": "Travel"}])
        );

        // Replay does not mint a second offer.
        let again = run(&db, &mut mbox, &msgs).await;
        assert_eq!(again.log[0]["status"], "already_applied");
        let offers: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE event_type = ?1",
                [ACTION_OFFERED],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(offers, 1);
    }

    #[tokio::test]
    async fn offer_with_move_to_trash_is_refused_at_execution() {
        // Built in code, bypassing serde, to prove the executor re-validates.
        let db = Database::open_memory().unwrap();
        let ctx = build_summary_context(&summary(1, "m@x", "a@b"), &db, ACCT).unwrap();
        let target = MessageTarget {
            account_id: ACCT,
            account_email: EMAIL,
            folder: "INBOX",
            uid: 1,
            message_id: Some("m@x"),
            ctx: &ctx,
        };
        let action = Action::Confirm {
            prompt: "p".to_string(),
            then: vec![ConfirmableAction::Move("[Gmail]/Trash".to_string())],
        };
        let mut mbox = FakeMailbox::default();
        let err = execute_action(
            &mut mbox,
            &db,
            &target,
            &action,
            None,
            &ActionAttribution::new(ActionSource::Rule),
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("allowlist"), "{err:#}");
        assert!(db.list_events(Some(ACCT), 10).unwrap().is_empty());
    }

    async fn mint_offer(db: &Database, mbox: &mut FakeMailbox) -> String {
        let report = run(
            db,
            mbox,
            &[summary(4, "t@airline.example", "x@airline.example")],
        )
        .await;
        report.log[0]["offer_event_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn confirming_an_offer_executes_its_actions_exactly_once() {
        let db = Database::open_memory().unwrap();
        confirm_rule(&db);
        let mut mbox = FakeMailbox::default();
        let offer = mint_offer(&db, &mut mbox).await;
        let attr = ActionAttribution::new(ActionSource::Cli);

        let first = confirm_offer(&mut mbox, &db, &offer, EMAIL, &attr)
            .await
            .unwrap();
        let second = confirm_offer(&mut mbox, &db, &offer, EMAIL, &attr)
            .await
            .unwrap();

        assert!(first.changed);
        assert!(!second.changed);
        assert_eq!(mbox.calls, vec!["move INBOX/4 -> Travel".to_string()]);
        assert_eq!(db.get_tags(ACCT, "t@airline.example").unwrap().len(), 1);
        let rows = action_rows(&db);
        let per_action: Vec<_> = rows
            .iter()
            .filter(|r| {
                r.event_id
                    .as_deref()
                    .is_some_and(|e| e.starts_with(&format!("{offer}:")))
            })
            .collect();
        assert_eq!(per_action.len(), 2);
        assert!(
            per_action
                .iter()
                .all(|r| r.action_taken.contains("\"source\":\"cli\""))
        );
        assert!(db.get_event(&offer).unwrap().unwrap().acked_at.is_some());
    }

    #[tokio::test]
    async fn dismissed_offer_cannot_be_confirmed_and_confirmed_cannot_be_dismissed() {
        let db = Database::open_memory().unwrap();
        confirm_rule(&db);
        let mut mbox = FakeMailbox::default();
        let attr = ActionAttribution::new(ActionSource::Cli);

        let offer = mint_offer(&db, &mut mbox).await;
        let dismissed = dismiss_offer(&db, &offer, &attr).unwrap();
        assert_eq!(dismissed.resolution, OfferResolution::Dismissed);
        assert!(!dismiss_offer(&db, &offer, &attr).unwrap().changed);
        let err = confirm_offer(&mut mbox, &db, &offer, EMAIL, &attr)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("dismissed"), "{err}");
        assert!(mbox.calls.is_empty());

        // A second message gets its own offer; confirm then dismiss is refused.
        let report = run(
            &db,
            &mut mbox,
            &[summary(5, "u@airline.example", "x@airline.example")],
        )
        .await;
        let offer2 = report.log[0]["offer_event_id"]
            .as_str()
            .unwrap()
            .to_string();
        confirm_offer(&mut mbox, &db, &offer2, EMAIL, &attr)
            .await
            .unwrap();
        assert!(dismiss_offer(&db, &offer2, &attr).is_err());
    }

    #[tokio::test]
    async fn rule_edited_after_mint_cannot_escalate() {
        let db = Database::open_memory().unwrap();
        let referenced = rule(&db, "travel-tag", r#"{"add_tag":"travel"}"#);
        let authored = json!({"confirm": {"prompt": "Trip?", "then": [{"rule": "travel-tag"}]}});
        let flattened = flatten_authored_action(&db, ACCT, &authored).unwrap();
        rule(
            &db,
            "trip-offer",
            &serde_json::to_string(&flattened).unwrap(),
        );
        // Disable the referenced rule so only the offer rule fires.
        db.disable_rule(&referenced.id).unwrap();

        let mut mbox = FakeMailbox::default();
        let offer = mint_offer(&db, &mut mbox).await;

        // Edit the referenced rule into a destructive one after minting.
        db.update_rule(
            &referenced.id,
            ACCT,
            "travel-tag",
            &referenced.match_expr,
            r#""delete""#,
            100,
            false,
        )
        .unwrap();

        let attr = ActionAttribution::new(ActionSource::Cli);
        confirm_offer(&mut mbox, &db, &offer, EMAIL, &attr)
            .await
            .unwrap();
        assert!(
            mbox.calls.iter().all(|c| !c.starts_with("delete")),
            "{:?}",
            mbox.calls
        );
        assert_eq!(db.get_tags(ACCT, "t@airline.example").unwrap().len(), 1);

        // Tampering with the stored offer to smuggle delete fails closed.
        let report = run(
            &db,
            &mut mbox,
            &[summary(6, "v@airline.example", "x@airline.example")],
        )
        .await;
        let offer2 = report.log[0]["offer_event_id"]
            .as_str()
            .unwrap()
            .to_string();
        db.conn()
            .execute(
                "UPDATE events SET payload = ?1 WHERE id = ?2",
                rusqlite_params(
                    r#"{"rule_id":null,"rule_name":null,"prompt":"p","actions":["delete"]}"#,
                    &offer2,
                ),
            )
            .unwrap();
        let before = mbox.calls.len();
        let err = confirm_offer(&mut mbox, &db, &offer2, EMAIL, &attr)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("allowlist"), "{err}");
        assert_eq!(mbox.calls.len(), before);
    }

    fn rusqlite_params<'a>(a: &'a str, b: &'a str) -> [&'a str; 2] {
        [a, b]
    }

    #[test]
    fn newly_live_actions_lists_tag_snooze_unsubscribe_rules() {
        let db = Database::open_memory().unwrap();
        rule(&db, "t", r#"{"add_tag":"x"}"#);
        rule(&db, "s", r#"{"snooze":"1d"}"#);
        rule(&db, "m", r#"{"move":"Archive"}"#);
        let rules = db.list_rules(ACCT).unwrap();
        let live = newly_live_actions(&rules);
        let gated: HashMap<_, _> = live
            .iter()
            .map(|v| (v["rule"].as_str().unwrap(), v["gated"].as_bool().unwrap()))
            .collect();
        assert_eq!(gated, HashMap::from([("t", false), ("s", true)]));
    }
}
