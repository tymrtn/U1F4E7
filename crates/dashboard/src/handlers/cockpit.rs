// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Agent Cockpit aggregate surface.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use envelope_email_store::{Account, Database, DraftStatus, Event, errors::Result as StoreResult};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;
use crate::ui_paths::message_dashboard_path;

#[derive(Debug, Deserialize)]
pub struct CockpitQuery {
    account_id: Option<String>,
}

pub async fn get(
    State(state): State<AppState>,
    Query(query): Query<CockpitQuery>,
) -> impl IntoResponse {
    // Snooze/schedule rows are stored in UTC; `now` must share that frame.
    let now = crate::timefmt::utc_now_string();
    let db = state.db.lock().await;
    match build_cockpit_json(&db, query.account_id.as_deref(), &now) {
        Ok(payload) => Json(payload).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cockpit db error: {e}"),
        )
            .into_response(),
    }
}

pub async fn get_for_account(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
) -> impl IntoResponse {
    let now = crate::timefmt::utc_now_string();
    let db = state.db.lock().await;
    match build_cockpit_json(&db, Some(&account_id), &now) {
        Ok(payload) => Json(payload).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cockpit db error: {e}"),
        )
            .into_response(),
    }
}

fn build_cockpit_json(db: &Database, account_id: Option<&str>, now: &str) -> StoreResult<Value> {
    let accounts = db.list_accounts()?;
    let selected_account = account_id.and_then(|id| {
        accounts
            .iter()
            .find(|acct| acct.id == id || acct.username == id)
            .cloned()
    });
    let account_status = match (account_id, selected_account.as_ref()) {
        (Some(_), Some(_)) => "selected",
        (Some(_), None) => "not_found",
        (None, _) => "not_selected",
    };
    let resolved_account_id = selected_account.as_ref().map(|acct| acct.id.as_str());
    let operational_scope = resolved_account_id.or(account_id);

    let raw_events = match account_status {
        "not_found" => Vec::new(),
        _ => db.list_events(operational_scope, 36)?,
    };
    let (operator_events, audit_events) = cockpit_event_streams(&raw_events, &accounts);
    let recent_events: Vec<_> = operator_events.iter().take(12).cloned().collect();
    let audit_events: Vec<_> = audit_events.into_iter().take(12).collect();
    let needs_attention_events: Vec<_> = recent_events
        .iter()
        .filter(|evt| evt.get("bucket").and_then(Value::as_str) == Some("needs_attention"))
        .take(6)
        .cloned()
        .collect();
    let mailbox_events: Vec<_> = recent_events
        .iter()
        .filter(|evt| evt.get("bucket").and_then(Value::as_str) == Some("mailbox"))
        .take(6)
        .cloned()
        .collect();
    let agent_action_events: Vec<_> = recent_events
        .iter()
        .filter(|evt| evt.get("bucket").and_then(Value::as_str) == Some("agent_action"))
        .take(6)
        .cloned()
        .collect();
    let pending_events = match (account_status, operational_scope) {
        ("not_found", _) | (_, None) => Vec::new(),
        (_, Some(id)) => db.list_unacked(id, 12)?,
    };
    let pending_events: Vec<_> = pending_events
        .iter()
        .filter(|evt| !is_routine_audit_event(evt))
        .map(|evt| cockpit_event_json(evt, &accounts))
        .collect();
    let recent_actions = match (account_status, operational_scope) {
        ("not_found", _) | (_, None) => Vec::new(),
        (_, Some(id)) => db.list_actions(id, 12)?,
    };
    let failed_actions: Vec<_> = recent_actions
        .iter()
        .filter(|action| !matches!(action.action_status.as_str(), "completed" | "sent" | "ok"))
        .cloned()
        .collect();
    let watches = match account_status {
        "not_found" => Vec::new(),
        _ => db.list_watches(operational_scope, 12)?,
    };
    let failed_auth = match account_status {
        "not_found" => Vec::new(),
        _ => db.list_failed_auth(operational_scope, 12)?,
    };

    let pending_review_drafts =
        list_drafts_by_status(db, resolved_account_id, DraftStatus::PendingReview)?;
    let blocked_drafts = list_drafts_by_status(db, resolved_account_id, DraftStatus::Blocked)?;
    let draft_drafts = list_drafts_by_status(db, resolved_account_id, DraftStatus::Draft)?;
    let mut pending_drafts = pending_review_drafts.clone();
    pending_drafts.extend(blocked_drafts.clone());

    let all_snoozes = match (account_status, operational_scope) {
        ("not_found", _) => Vec::new(),
        (_, Some(id)) => db.list_snoozed(Some(id))?,
        (_, None) => db.list_snoozed(None)?,
    };
    let now_utc = crate::timefmt::parse_utc(now);
    let due_snoozes: Vec<_> = all_snoozes
        .iter()
        .filter(|item| {
            matches!(
                (crate::timefmt::parse_utc(&item.return_at), now_utc),
                (Some(r), Some(n)) if r <= n
            )
        })
        .cloned()
        .collect();

    let rules = match resolved_account_id {
        Some(id) => db.list_rules(id)?,
        None => Vec::new(),
    };
    let reviewable_rules = rules.iter().filter(|rule| !rule.enabled).count();
    let rules_json: Vec<_> = rules
        .iter()
        .map(|rule| {
            json!({
                "id": rule.id,
                "account_id": rule.account_id,
                "name": rule.name,
                "match_expr": rule.match_expr,
                "action": crate::handlers::rules::sanitized_action_json(&rule.action),
                "enabled": rule.enabled,
                "review_state": if rule.enabled { "live_enabled" } else { "proposed_disabled" },
                "preview": { "status": "not_requested", "mutated": false },
                "priority": rule.priority,
                "stop": rule.stop,
                "sieve_exportable": rule.sieve_exportable,
                "hit_count": rule.hit_count,
                "last_hit_at": rule.last_hit_at,
                "created_at": rule.created_at,
                "updated_at": rule.updated_at,
            })
        })
        .collect();
    let enabled_rules = rules.iter().filter(|rule| rule.enabled).count();
    let recent_rule_runs = db.list_rule_runs(resolved_account_id, 12)?;

    let draft_actions = match account_status {
        "selected" => json!({
            "approve": "available",
            "edit": "available",
            "discard": "available",
            "block": "available",
            "send": "available_confirm_required"
        }),
        _ => json!({
            "approve": "not_available",
            "edit": "not_available",
            "discard": "not_available",
            "block": "not_available",
            "send": "not_available"
        }),
    };
    let draft_unavailable_reason = match account_status {
        "selected" => Value::Null,
        "not_found" => json!("account_not_found"),
        _ => json!("select_account_required"),
    };

    Ok(json!({
        "account": selected_account,
        "account_status": account_status,
        "summary": {
            "accounts": accounts.len(),
            "watches": { "status": "available", "count": watches.len() },
            "recent_events": recent_events.len(),
            "audit_events": audit_events.len(),
            "needs_attention_events": needs_attention_events.len(),
            "mailbox_events": mailbox_events.len(),
            "agent_action_events": agent_action_events.len(),
            "pending_events": pending_events.len(),
            "pending_drafts": pending_drafts.len(),
            "failed_actions": failed_actions.len(),
            "rules": rules.len(),
            "enabled_rules": enabled_rules,
            "reviewable_rules": reviewable_rules,
            "recent_rule_runs": recent_rule_runs.len(),
            "due_snoozes": due_snoozes.len()
        },
        "watches": { "status": "available", "items": watches },
        "events": {
            "recent": recent_events,
            "pending": pending_events,
            "needs_attention": needs_attention_events,
            "mailbox": mailbox_events,
            "agent_actions": agent_action_events,
            "audit": audit_events,
            "audit_filter": { "default": "hidden", "types": ["send_policy.allowed"] }
        },
        "drafts": {
            "pending": pending_drafts,
            "counts": { "draft": draft_drafts.len(), "pending_review": pending_review_drafts.len(), "blocked": blocked_drafts.len() },
            "actions": draft_actions,
            "unavailable_reason": draft_unavailable_reason
        },
        "auth": { "status": "available", "items": failed_auth },
        "actions": { "recent": recent_actions, "failed": failed_actions },
        "rules": { "items": rules_json, "recent_runs": recent_rule_runs },
        "snoozes": { "due": due_snoozes, "total": all_snoozes.len() },
        "generated_at": now
    }))
}

fn list_drafts_by_status(
    db: &Database,
    account_id: Option<&str>,
    status: DraftStatus,
) -> StoreResult<Vec<envelope_email_store::Draft>> {
    match account_id {
        Some(id) => db.list_drafts(id, Some(status.as_str()), 12, 0),
        None => Ok(Vec::new()),
    }
}

fn cockpit_event_streams(events: &[Event], accounts: &[Account]) -> (Vec<Value>, Vec<Value>) {
    let mut operator = Vec::new();
    let mut audit = Vec::new();
    for event in events {
        let item = cockpit_event_json(event, accounts);
        if is_routine_audit_event(event) {
            audit.push(item);
        } else {
            operator.push(item);
        }
    }
    (operator, audit)
}

fn cockpit_event_json(event: &Event, accounts: &[Account]) -> Value {
    let payload = event
        .payload
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or(Value::Null);
    let payload_field = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    };
    let actor = payload_field("actor")
        .or_else(|| payload_field("agent"))
        .or_else(|| payload_field("source"))
        .unwrap_or_else(|| event_source_from_type(&event.event_type).to_string());
    let source = payload_field("source")
        .unwrap_or_else(|| event_source_from_type(&event.event_type).to_string());
    let outcome =
        payload_field("outcome").unwrap_or_else(|| event_outcome_from_type(event).to_string());
    let bucket = event_bucket(event);
    let account_label = accounts
        .iter()
        .find(|acct| acct.id == event.account_id || acct.username == event.account_id)
        .map(|acct| {
            acct.display_name
                .as_deref()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| {
                    if acct.name.is_empty() {
                        &acct.username
                    } else {
                        &acct.name
                    }
                })
                .to_string()
        })
        .unwrap_or_else(|| event.account_id.clone());
    let message_link = event
        .uid
        .map(|uid| message_dashboard_path(&event.account_id, &event.folder, uid));

    json!({
        "id": event.id,
        "account_id": event.account_id,
        "account_label": account_label,
        "actor": actor,
        "source": source,
        "event_type": event.event_type,
        "outcome": outcome,
        "bucket": bucket,
        "folder": event.folder,
        "uid": event.uid,
        "message_id": event.message_id,
        "message_link": message_link,
        "from_addr": event.from_addr,
        "subject": event.subject,
        "snippet": event.snippet,
        "payload": payload,
        "secure_pending": event.secure_pending,
        "acked_at": event.acked_at,
        "ack_state": if event.acked_at.is_some() { "acked" } else { "pending" },
        "created_at": event.created_at,
    })
}

pub(crate) fn is_routine_audit_event(event: &Event) -> bool {
    matches!(event.event_type.as_str(), "send_policy.allowed")
}

fn event_bucket(event: &Event) -> &'static str {
    let event_type = event.event_type.as_str();
    if event.secure_pending
        || event.acked_at.is_none()
            && [
                "failed", "failure", "error", "denied", "blocked", "pending", "review", "auth",
            ]
            .iter()
            .any(|needle| event_type.contains(needle))
    {
        "needs_attention"
    } else if event_type.starts_with("watch.")
        || event_type.starts_with("mailbox.")
        || event_type.starts_with("message.")
        || event_type.contains("otp")
    {
        "mailbox"
    } else {
        "agent_action"
    }
}

fn event_source_from_type(event_type: &str) -> &'static str {
    if event_type.starts_with("watch.") || event_type.starts_with("mailbox.") {
        "mailbox-watch"
    } else if event_type.starts_with("send_policy.") {
        "send-policy"
    } else if event_type.starts_with("rule.") {
        "rules"
    } else if event_type.starts_with("draft.") {
        "drafts"
    } else {
        "envelope"
    }
}

pub(crate) fn event_outcome_from_type(event: &Event) -> &'static str {
    let event_type = event.event_type.as_str();
    if event.secure_pending {
        "needs_review"
    } else if event.acked_at.is_some() {
        "acked"
    } else if event_type.contains("failed") || event_type.contains("error") {
        "failed"
    } else if event_type.contains("allowed") || event_type.contains("matched") {
        "ok"
    } else if event_type.contains("pending") {
        "pending"
    } else {
        "recorded"
    }
}

#[cfg(test)]
mod tests {
    use envelope_email_store::{Database, Event};

    #[test]
    fn cockpit_reports_pending_drafts_and_due_snoozes() {
        let db = Database::open_memory().unwrap();
        seed_account(&db);
        let draft = db
            .create_draft(
                "acc1",
                "buyer@example.com",
                Some("Approve me"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_status(&draft.id, envelope_email_store::DraftStatus::PendingReview)
            .unwrap();
        db.conn().execute("INSERT INTO snoozed (id, account, uid, original_folder, snoozed_folder, return_at, subject, created_at) VALUES ('snz1', 'acc1', 42, 'INBOX', 'Snoozed', '2026-05-08T08:00:00', 'Due item', '2026-05-07T08:00:00')", []).unwrap();
        let payload = super::build_cockpit_json(&db, Some("acc1"), "2026-05-09T09:00:00").unwrap();
        assert_eq!(payload["summary"]["pending_drafts"], 1);
        assert_eq!(payload["summary"]["due_snoozes"], 1);
        assert_eq!(payload["drafts"]["pending"][0]["subject"], "Approve me");
        assert_eq!(payload["snoozes"]["due"][0]["subject"], "Due item");
    }

    #[test]
    fn cockpit_marks_draft_actions_unavailable_without_selected_account() {
        let db = Database::open_memory().unwrap();
        seed_account(&db);

        let payload = super::build_cockpit_json(&db, None, "2026-05-09T09:00:00").unwrap();

        assert_eq!(payload["drafts"]["actions"]["approve"], "not_available");
        assert_eq!(payload["drafts"]["actions"]["send"], "not_available");
        assert_eq!(
            payload["drafts"]["unavailable_reason"],
            "select_account_required"
        );
    }

    #[test]
    fn cockpit_marks_draft_actions_unavailable_for_unknown_account() {
        let db = Database::open_memory().unwrap();
        seed_account(&db);

        let payload =
            super::build_cockpit_json(&db, Some("missing"), "2026-05-09T09:00:00").unwrap();

        assert_eq!(payload["account_status"], "not_found");
        assert_eq!(payload["drafts"]["actions"]["approve"], "not_available");
        assert_eq!(payload["drafts"]["actions"]["send"], "not_available");
        assert_eq!(payload["drafts"]["unavailable_reason"], "account_not_found");
    }

    #[test]
    fn cockpit_events_hide_routine_audit_and_expose_operator_buckets() {
        let db = Database::open_memory().unwrap();
        seed_account(&db);
        insert_event(
            &db,
            "audit-1",
            "send_policy.allowed",
            false,
            Some("{\"actor\":\"hermes\",\"source\":\"mcp\",\"outcome\":\"allowed\"}"),
        );
        insert_event(
            &db,
            "mail-1",
            "watch.message_matched",
            false,
            Some("{\"actor\":\"watch\",\"source\":\"imap-watch\",\"outcome\":\"matched\"}"),
        );
        insert_event(
            &db,
            "attention-1",
            "draft.pending_review",
            true,
            Some("{\"actor\":\"codex\",\"source\":\"agent\",\"outcome\":\"needs_review\"}"),
        );

        let payload = super::build_cockpit_json(&db, Some("acc1"), "2026-05-09T09:00:00").unwrap();

        assert_eq!(payload["summary"]["recent_events"], 2);
        assert_eq!(payload["summary"]["audit_events"], 1);
        assert_eq!(
            payload["events"]["audit"][0]["event_type"],
            "send_policy.allowed"
        );
        assert_eq!(
            payload["events"]["needs_attention"][0]["event_type"],
            "draft.pending_review"
        );
        assert_eq!(
            payload["events"]["mailbox"][0]["event_type"],
            "watch.message_matched"
        );
        assert_eq!(payload["events"]["recent"][0]["account_label"], "Test");
        assert_eq!(payload["events"]["recent"][0]["actor"], "codex");
        assert_eq!(payload["events"]["recent"][0]["outcome"], "needs_review");
        assert_eq!(
            payload["events"]["recent"][0]["message_link"],
            "/mail/unified/acc1/101?folder=INBOX"
        );
        assert_eq!(payload["events"]["recent"][0]["ack_state"], "pending");
    }

    #[test]
    fn cockpit_lists_disabled_rules_as_reviewable_without_previewing() {
        let db = Database::open_memory().unwrap();
        seed_account(&db);
        db.create_rule_with_enabled(
            "acc1",
            "Review newsletter junk",
            r#"{"subject_contains":"newsletter"}"#,
            r#"{"move":"Junk"}"#,
            10,
            false,
            false,
        )
        .unwrap();

        let payload = super::build_cockpit_json(&db, Some("acc1"), "2026-05-09T09:00:00").unwrap();

        assert_eq!(payload["summary"]["rules"], 1);
        assert_eq!(payload["summary"]["enabled_rules"], 0);
        assert_eq!(payload["summary"]["reviewable_rules"], 1);
        assert_eq!(payload["rules"]["items"][0]["enabled"], false);
        assert_eq!(
            payload["rules"]["items"][0]["review_state"],
            "proposed_disabled"
        );
        assert_eq!(
            payload["rules"]["items"][0]["preview"]["status"],
            "not_requested"
        );
        assert_eq!(payload["rules"]["items"][0]["preview"]["mutated"], false);
    }

    #[test]
    fn cockpit_reads_operational_primitives_without_stubs() {
        let db = Database::open_memory().unwrap();
        seed_account(&db);
        db.upsert_watch(envelope_email_store::WatchUpsert {
            account_id: "acc1",
            folder: "INBOX",
            status: "running",
            process_id: Some(4242),
            schedule: Some("foreground"),
            last_heartbeat_at: Some("2026-05-09T08:59:00"),
            last_event_at: Some("2026-05-09T08:58:00"),
            failure_reason: None,
        })
        .unwrap();
        db.record_failed_auth(
            "acc1",
            "imap",
            "LOGIN failed for password=secret-token",
            Some("Create an app password and retry verification."),
        )
        .unwrap();
        db.record_rule_run(envelope_email_store::RuleRunAuditInput {
            account_id: "acc1",
            rule_id: Some("rule-1"),
            rule_name: Some("VIP move"),
            uid: Some(99),
            folder: Some("INBOX"),
            action: Some("moved to VIP"),
            status: "ok",
            error: None,
        })
        .unwrap();

        let payload = super::build_cockpit_json(&db, Some("acc1"), "2026-05-09T09:00:00").unwrap();
        assert_eq!(payload["watches"]["status"], "available");
        assert_eq!(payload["summary"]["watches"]["count"], 1);
        assert_eq!(payload["watches"]["items"][0]["folder"], "INBOX");
        assert_eq!(payload["auth"]["status"], "available");
        assert_eq!(payload["auth"]["items"][0]["backend"], "imap");
        assert!(
            !payload["auth"]["items"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("secret-token")
        );
        assert_eq!(payload["rules"]["recent_runs"][0]["rule_name"], "VIP move");
        assert_eq!(payload["drafts"]["actions"]["approve"], "available");
        assert_eq!(payload["drafts"]["actions"]["discard"], "available");
    }

    fn seed_account(db: &Database) {
        db.conn().execute("INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port, encrypted_password) VALUES ('acc1', 'Test', 'operator@example.com', 'example.com', 'smtp.example.com', 587, 'imap.example.com', 993, 'encrypted')", []).unwrap();
    }

    fn insert_event(
        db: &Database,
        id: &str,
        event_type: &str,
        secure_pending: bool,
        payload: Option<&str>,
    ) {
        db.insert_event(&Event {
            id: id.to_string(),
            account_id: "acc1".to_string(),
            event_type: event_type.to_string(),
            folder: "INBOX".to_string(),
            uid: Some(101),
            message_id: Some("<msg-101@example.com>".to_string()),
            from_addr: Some("sender@example.com".to_string()),
            subject: Some("Operator-visible subject".to_string()),
            snippet: Some("Useful message snippet".to_string()),
            payload: payload.map(str::to_string),
            idempotency_key: None,
            secure_pending,
            acked_at: None,
            created_at: match id {
                "attention-1" => "2026-05-09T08:59:00".to_string(),
                "mail-1" => "2026-05-09T08:58:00".to_string(),
                _ => "2026-05-09T08:57:00".to_string(),
            },
        })
        .unwrap();
    }
}
