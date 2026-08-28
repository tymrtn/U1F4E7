// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Review queue aggregate surface.
//!
//! `GET /api/review` is the operator's daily queue: everything pending, grouped
//! by the decision needed, across ALL accounts. Four groups, in priority order:
//!   1. `decide_now` — pending/blocked drafts, failed agent actions, unacked
//!      actionable events, proposed (disabled) rules
//!   2. `waiting` — scheduled sends with due/countdown facts, due snoozes,
//!      not-yet-due awaiting-reply snoozes
//!   3. `needs_triage` — durable message-anchored events (a uid + folder
//!      exists) with canonical reader links. This is NOT an inbox classifier:
//!      only events already recorded in the store land here.
//!   4. `operational_health` — actual failures only: failed auth, failed
//!      watches, dead-lettered routes. No generic activity.
//!
//! Read-only: one build over the store, no IMAP, no sweep, no send, no ack, no
//! rule application. Unlike the cockpit aggregate, drafts and rules are
//! aggregated globally — nothing is silently empty when no account is selected.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use envelope_email_store::{
    Account, Database, Draft, DraftStatus, Event, SnoozedMessage, errors::Result as StoreResult,
};
use serde_json::{Value, json};

use crate::state::AppState;
use crate::ui_paths::{draft_dashboard_path, message_dashboard_path};

pub async fn get(State(state): State<AppState>) -> impl IntoResponse {
    // Snooze/schedule rows are stored in UTC; `now` must share that frame.
    let now = crate::timefmt::utc_now_string();
    let db = state.db.lock().await;
    match build_review_json(&db, &now) {
        Ok(payload) => Json(payload).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("review db error: {e}"),
        )
            .into_response(),
    }
}

const DRAFT_LIMIT: u32 = 100;
const PER_ACCOUNT_ACTIONS: u32 = 25;
const PER_ACCOUNT_UNACKED: usize = 50;

fn build_review_json(db: &Database, now: &str) -> StoreResult<Value> {
    let accounts = db.list_accounts()?;

    // ── Decide now: drafts, aggregated globally (never empty just because no
    // account is selected — the cockpit regression this endpoint fixes).
    let pending = db.list_all_drafts_by_status(DraftStatus::PendingReview.as_str(), DRAFT_LIMIT)?;
    let blocked = db.list_all_drafts_by_status(DraftStatus::Blocked.as_str(), DRAFT_LIMIT)?;
    let mut awaiting: Vec<&Draft> = pending.iter().chain(blocked.iter()).collect();
    awaiting.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    let draft_items: Vec<Value> = awaiting
        .iter()
        .map(|draft| {
            let mut item = super::agents::draft_approval_summary(draft);
            item["account_label"] = json!(account_label(&accounts, &draft.account_id));
            item["link"] = json!(draft_dashboard_path(&draft.account_id, &draft.id));
            item
        })
        .collect();

    // ── Decide now: failed agent actions. Completed/sent/ok actions are
    // routine activity and stay out of the queue.
    let mut failed_actions = Vec::new();
    for account in &accounts {
        for action in db.list_actions(&account.id, PER_ACCOUNT_ACTIONS)? {
            if matches!(action.action_status.as_str(), "completed" | "sent" | "ok") {
                continue;
            }
            failed_actions.push(json!({
                "id": action.id,
                "account_id": action.account_id,
                "account_label": account_label(&accounts, &action.account_id),
                "action_type": action.action_type,
                "action_status": action.action_status,
                "justification": action.justification,
                "draft_id": action.draft_id,
                "draft_link": action.draft_id.as_deref()
                    .map(|draft_id| draft_dashboard_path(&action.account_id, draft_id)),
                "created_at": action.created_at,
            }));
        }
    }
    sort_by_created_at_desc(&mut failed_actions);

    // ── Unacked events, split by message anchor: an event with a uid names an
    // exact message and belongs in Needs triage with a reader link; one without
    // (policy denial, failure) is a decision in its own right. Routine audit
    // events (send_policy.allowed) never inflate the queue.
    let mut decide_events = Vec::new();
    let mut triage_items = Vec::new();
    for account in &accounts {
        for event in db.list_unacked(&account.id, PER_ACCOUNT_UNACKED)? {
            if super::cockpit::is_routine_audit_event(&event) {
                continue;
            }
            let item = event_summary(&event, &accounts);
            if event.uid.is_some() {
                triage_items.push(item);
            } else {
                decide_events.push(item);
            }
        }
    }
    sort_by_created_at_desc(&mut decide_events);
    sort_by_created_at_desc(&mut triage_items);

    // ── Decide now: proposed rules. Disabled rules are proposals awaiting an
    // operator decision; enabled rules are live automation and stay out.
    // No per-rule deep link exists yet — /rules is the canonical surface.
    let mut proposed_rules = Vec::new();
    for account in &accounts {
        for rule in db.list_rules(&account.id)? {
            if rule.enabled {
                continue;
            }
            proposed_rules.push(json!({
                "id": rule.id,
                "account_id": rule.account_id,
                "account_label": account_label(&accounts, &rule.account_id),
                "name": rule.name,
                "action": super::rules::sanitized_action_json(&rule.action),
                "review_state": "proposed_disabled",
                "live": false,
                "priority": rule.priority,
                "created_at": rule.created_at,
                "updated_at": rule.updated_at,
                "link": "/rules",
            }));
        }
    }

    // ── Waiting: scheduled sends with due/countdown facts.
    let scheduled_drafts = db.list_scheduled_drafts(None, DRAFT_LIMIT)?;
    let now_utc = crate::timefmt::parse_utc(now);
    let scheduled_items: Vec<Value> = scheduled_drafts
        .iter()
        .map(|draft| {
            let send_after = draft.send_after.as_deref();
            let due = matches!(
                (send_after.and_then(crate::timefmt::parse_utc), now_utc),
                (Some(at), Some(n)) if at <= n
            );
            json!({
                "id": draft.id,
                "account_id": draft.account_id,
                "account_label": account_label(&accounts, &draft.account_id),
                "subject": draft.subject,
                "created_by": draft.created_by,
                "send_after": draft.send_after,
                "due": due,
                "seconds_remaining": send_after
                    .and_then(|at| super::scheduled::seconds_between(now, at)),
                "link": draft_dashboard_path(&draft.account_id, &draft.id),
                "action_base": format!("/api/accounts/{}/drafts/{}", draft.account_id, draft.id),
            })
        })
        .collect();
    let scheduled_due = scheduled_items
        .iter()
        .filter(|item| item["due"] == json!(true))
        .count();

    // ── Waiting: due snoozes, plus not-yet-due awaiting-reply follow-ups.
    // A due awaiting-reply snooze belongs in the due list, not both.
    let due_snoozes: Vec<Value> = db
        .list_snoozed_due(now, None)?
        .iter()
        .map(|snooze| snooze_summary(snooze, &accounts, true))
        .collect();
    let awaiting_reply: Vec<Value> = db
        .list_snoozed_awaiting_reply(None)?
        .iter()
        .filter(|snooze| {
            !matches!(
                (crate::timefmt::parse_utc(&snooze.return_at), now_utc),
                (Some(at), Some(n)) if at <= n
            )
        })
        .map(|snooze| snooze_summary(snooze, &accounts, false))
        .collect();

    // ── Operational health: actual failures only.
    let failed_auth: Vec<Value> = db
        .list_failed_auth(None, 50)?
        .iter()
        .map(|attempt| {
            json!({
                "id": attempt.id,
                "account_id": attempt.account_id,
                "account_label": account_label(&accounts, &attempt.account_id),
                "backend": attempt.backend,
                "reason": attempt.reason,
                "retry_guidance": attempt.retry_guidance,
                "created_at": attempt.created_at,
            })
        })
        .collect();
    let failed_watches: Vec<Value> = db
        .list_watches(None, 50)?
        .iter()
        .filter(|watch| super::watches::watch_health(&watch.status) == "danger")
        .map(|watch| {
            json!({
                "id": watch.id,
                "account_id": watch.account_id,
                "account_label": account_label(&accounts, &watch.account_id),
                "folder": watch.folder,
                "status": watch.status,
                "failure_reason": watch.failure_reason,
                "last_heartbeat_at": watch.last_heartbeat_at,
            })
        })
        .collect();
    let mut dead_routes = Vec::new();
    for account in &accounts {
        for route in db.list_event_routes(&account.id)? {
            let (_delivered, _pending, dead) = db.route_delivery_counts(&route.id)?;
            if dead == 0 {
                continue;
            }
            dead_routes.push(json!({
                "id": route.id,
                "account_id": route.account_id,
                "account_label": account_label(&accounts, &route.account_id),
                "match_expr": route.match_expr,
                "enabled": route.enabled,
                "dead": dead,
            }));
        }
    }
    let dead_letter_total = db.dead_letter_count()?;

    let decide_now_count =
        draft_items.len() + failed_actions.len() + decide_events.len() + proposed_rules.len();
    let waiting_count = scheduled_items.len() + due_snoozes.len() + awaiting_reply.len();
    let triage_count = triage_items.len();
    let health_count = failed_auth.len() + failed_watches.len() + dead_routes.len();

    Ok(json!({
        "summary": {
            "decide_now": decide_now_count,
            "waiting": waiting_count,
            "needs_triage": triage_count,
            "operational_health": health_count,
        },
        "decide_now": {
            "count": decide_now_count,
            "drafts": {
                "counts": {
                    "pending_review": pending.len(),
                    "blocked": blocked.len(),
                },
                "items": draft_items,
            },
            "failed_actions": { "count": failed_actions.len(), "items": failed_actions },
            "events": { "count": decide_events.len(), "items": decide_events },
            "proposed_rules": { "count": proposed_rules.len(), "items": proposed_rules },
        },
        "waiting": {
            "count": waiting_count,
            "scheduled": {
                "count": scheduled_items.len(),
                "due": scheduled_due,
                "items": scheduled_items,
            },
            "due_snoozes": { "count": due_snoozes.len(), "items": due_snoozes },
            "awaiting_reply": { "count": awaiting_reply.len(), "items": awaiting_reply },
        },
        "needs_triage": {
            "count": triage_count,
            // Truth-in-labeling: only durable store events land here. This is
            // not an inbox classifier and never scans or scores messages.
            "source": "durable_events",
            "items": triage_items,
        },
        "operational_health": {
            "count": health_count,
            "failed_auth": { "count": failed_auth.len(), "items": failed_auth },
            "failed_watches": { "count": failed_watches.len(), "items": failed_watches },
            "dead_letters": {
                "count": dead_letter_total,
                "routes": dead_routes,
            },
        },
        "generated_at": now,
    }))
}

/// Display label for an account id or username alias: `display_name` → `name`
/// → `username`, falling back to the raw id (same resolution as the cockpit).
fn account_label(accounts: &[Account], account_id: &str) -> String {
    accounts
        .iter()
        .find(|acct| acct.id == account_id || acct.username == account_id)
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
        .unwrap_or_else(|| account_id.to_string())
}

/// Curated event fields only — never the raw payload blob, which writers may
/// stuff with anything.
fn event_summary(event: &Event, accounts: &[Account]) -> Value {
    json!({
        "id": event.id,
        "account_id": event.account_id,
        "account_label": account_label(accounts, &event.account_id),
        "event_type": event.event_type,
        "outcome": super::cockpit::event_outcome_from_type(event),
        "from_addr": event.from_addr,
        "subject": event.subject,
        "snippet": event.snippet,
        "folder": event.folder,
        "uid": event.uid,
        "message_link": event.uid
            .map(|uid| message_dashboard_path(&event.account_id, &event.folder, uid)),
        "secure_pending": event.secure_pending,
        "created_at": event.created_at,
    })
}

/// Snooze summary without the `recipient` column — a full address that must
/// not cross the aggregate surface. The link targets the snoozed folder, where
/// the message currently lives.
fn snooze_summary(snooze: &SnoozedMessage, accounts: &[Account], due: bool) -> Value {
    json!({
        "id": snooze.id,
        "account_id": snooze.account,
        "account_label": account_label(accounts, &snooze.account),
        "subject": snooze.subject,
        "return_at": snooze.return_at,
        "reason": snooze.reason,
        "note": snooze.note,
        "folder": snooze.snoozed_folder,
        "uid": snooze.uid,
        "message_link": message_dashboard_path(&snooze.account, &snooze.snoozed_folder, snooze.uid),
        "due": due,
    })
}

fn sort_by_created_at_desc(items: &mut [Value]) {
    items.sort_by(|a, b| {
        b["created_at"]
            .as_str()
            .unwrap_or_default()
            .cmp(a["created_at"].as_str().unwrap_or_default())
    });
}

#[cfg(test)]
mod tests {
    use envelope_email_store::{Database, DraftStatus, Event};

    fn seed_account(db: &Database, id: &str, name: &str, username: &str) {
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port, encrypted_password)
                 VALUES (?1, ?2, ?3, 'example.com', 'smtp.example.com', 587, 'imap.example.com', 993, 'x')",
                [id, name, username],
            )
            .unwrap();
    }

    fn insert_event(
        db: &Database,
        id: &str,
        account_id: &str,
        event_type: &str,
        uid: Option<i64>,
        secure_pending: bool,
        created_at: &str,
    ) {
        db.insert_event(&Event {
            id: id.to_string(),
            account_id: account_id.to_string(),
            event_type: event_type.to_string(),
            folder: "INBOX".to_string(),
            uid,
            message_id: uid.map(|u| format!("<msg-{u}@example.com>")),
            from_addr: Some("sender@example.com".to_string()),
            subject: Some(format!("Event subject {id}")),
            snippet: Some("Useful snippet".to_string()),
            payload: None,
            idempotency_key: None,
            secure_pending,
            acked_at: None,
            created_at: created_at.to_string(),
        })
        .unwrap();
    }

    const NOW: &str = "2026-05-09T09:00:00";

    #[test]
    fn review_aggregates_drafts_globally_with_canonical_links() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "Work", "op@example.com");
        seed_account(&db, "acc2", "Personal", "me@example.org");
        let pending = db
            .create_draft(
                "acc1",
                "to@example.com",
                Some("Approve outreach"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("mcp"),
            )
            .unwrap();
        db.update_draft_status(&pending.id, DraftStatus::PendingReview)
            .unwrap();
        let blocked = db
            .create_draft(
                "acc2",
                "to@example.org",
                Some("Blocked reply"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_status(&blocked.id, DraftStatus::Blocked)
            .unwrap();

        // No account selection anywhere: the review queue must aggregate
        // globally instead of silently reporting zero drafts.
        let payload = super::build_review_json(&db, NOW).unwrap();

        assert_eq!(
            payload["decide_now"]["drafts"]["counts"]["pending_review"],
            1
        );
        assert_eq!(payload["decide_now"]["drafts"]["counts"]["blocked"], 1);
        let items = payload["decide_now"]["drafts"]["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        let pending_item = items
            .iter()
            .find(|item| item["status"] == "pending_review")
            .unwrap();
        assert_eq!(pending_item["subject"], "Approve outreach");
        assert_eq!(pending_item["account_label"], "Work");
        assert_eq!(
            pending_item["link"],
            format!("/accounts/acc1/drafts/{}", pending.id)
        );
        let blocked_item = items
            .iter()
            .find(|item| item["status"] == "blocked")
            .unwrap();
        assert_eq!(blocked_item["account_label"], "Personal");
        assert_eq!(
            blocked_item["link"],
            format!("/accounts/acc2/drafts/{}", blocked.id)
        );
        assert_eq!(payload["summary"]["decide_now"], 2);
        assert_eq!(payload["generated_at"], NOW);
    }

    #[test]
    fn review_waiting_lists_scheduled_and_due_snoozes_with_facts() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "Work", "op@example.com");
        let due_draft = db
            .create_draft(
                "acc1",
                "to@example.com",
                Some("Due send"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&due_draft.id, "2026-05-09T08:00:00")
            .unwrap();
        let future_draft = db
            .create_draft(
                "acc1",
                "to@example.com",
                Some("Later send"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&future_draft.id, "2026-05-09T09:01:00")
            .unwrap();
        db.create_snoozed(
            "acc1",
            42,
            "INBOX",
            "Snoozed",
            "2026-05-09T08:30:00",
            None,
            Some("Due follow-up"),
            Some("review"),
            None,
            None,
        )
        .unwrap();
        db.create_snoozed(
            "acc1",
            43,
            "INBOX",
            "Snoozed",
            "2026-05-10T09:00:00",
            None,
            Some("Waiting on reply"),
            Some("waiting-reply"),
            None,
            None,
        )
        .unwrap();

        let payload = super::build_review_json(&db, NOW).unwrap();

        assert_eq!(payload["waiting"]["scheduled"]["count"], 2);
        assert_eq!(payload["waiting"]["scheduled"]["due"], 1);
        // list_scheduled_drafts orders soonest-first: the due one leads.
        let scheduled = payload["waiting"]["scheduled"]["items"].as_array().unwrap();
        assert_eq!(scheduled[0]["due"], true);
        assert_eq!(scheduled[0]["subject"], "Due send");
        assert_eq!(
            scheduled[0]["link"],
            format!("/accounts/acc1/drafts/{}", due_draft.id)
        );
        assert_eq!(scheduled[1]["due"], false);
        assert_eq!(scheduled[1]["seconds_remaining"], 60);

        assert_eq!(payload["waiting"]["due_snoozes"]["count"], 1);
        let due_snooze = &payload["waiting"]["due_snoozes"]["items"][0];
        assert_eq!(due_snooze["subject"], "Due follow-up");
        assert_eq!(due_snooze["account_label"], "Work");
        assert_eq!(
            due_snooze["message_link"],
            "/mail/unified/acc1/42?folder=Snoozed"
        );

        // The not-yet-due waiting-reply snooze waits; it is not "due".
        assert_eq!(payload["waiting"]["awaiting_reply"]["count"], 1);
        let awaiting = &payload["waiting"]["awaiting_reply"]["items"][0];
        assert_eq!(awaiting["subject"], "Waiting on reply");
        assert_eq!(awaiting["uid"], 43);
        assert_eq!(payload["summary"]["waiting"], 4);
    }

    #[test]
    fn review_disabled_rules_are_proposals_never_live() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "Work", "op@example.com");
        db.create_rule_with_enabled(
            "acc1",
            "Proposed junk sweep",
            r#"{"subject_contains":"newsletter"}"#,
            r#"{"move":"Junk"}"#,
            10,
            false,
            false,
        )
        .unwrap();
        db.create_rule_with_enabled(
            "acc1",
            "Live VIP move",
            r#"{"from_contains":"vip"}"#,
            r#"{"move":"VIP"}"#,
            5,
            false,
            true,
        )
        .unwrap();

        let payload = super::build_review_json(&db, NOW).unwrap();

        assert_eq!(payload["decide_now"]["proposed_rules"]["count"], 1);
        let proposal = &payload["decide_now"]["proposed_rules"]["items"][0];
        assert_eq!(proposal["name"], "Proposed junk sweep");
        assert_eq!(proposal["review_state"], "proposed_disabled");
        assert_eq!(proposal["live"], false);
        assert_eq!(proposal["link"], "/rules");
        // The live rule is running automation, not a decision — it must not
        // appear anywhere in the review queue.
        let serialized = serde_json::to_string(&payload).unwrap();
        assert!(!serialized.contains("Live VIP move"));
    }

    #[test]
    fn review_routine_audit_events_do_not_inflate_the_queue() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "Work", "op@example.com");
        insert_event(
            &db,
            "audit-1",
            "acc1",
            "send_policy.allowed",
            None,
            false,
            "2026-05-09T08:59:00",
        );

        let payload = super::build_review_json(&db, NOW).unwrap();

        assert_eq!(payload["decide_now"]["events"]["count"], 0);
        assert_eq!(payload["needs_triage"]["count"], 0);
        assert_eq!(payload["summary"]["decide_now"], 0);
        assert_eq!(payload["summary"]["needs_triage"], 0);
    }

    #[test]
    fn review_splits_unacked_events_between_decide_and_triage_by_message_anchor() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "Work", "op@example.com");
        // Message-anchored: a uid + folder exists, so triage links to the message.
        insert_event(
            &db,
            "triage-1",
            "acc1",
            "watch.message_matched",
            Some(101),
            false,
            "2026-05-09T08:58:00",
        );
        // Not message-anchored: a policy denial is a decision, not a message.
        insert_event(
            &db,
            "decide-1",
            "acc1",
            "send_policy.denied",
            None,
            false,
            "2026-05-09T08:59:00",
        );

        let payload = super::build_review_json(&db, NOW).unwrap();

        assert_eq!(payload["needs_triage"]["count"], 1);
        let triage = &payload["needs_triage"]["items"][0];
        assert_eq!(triage["id"], "triage-1");
        assert_eq!(triage["account_label"], "Work");
        assert_eq!(
            triage["message_link"],
            "/mail/unified/acc1/101?folder=INBOX"
        );

        assert_eq!(payload["decide_now"]["events"]["count"], 1);
        let decide = &payload["decide_now"]["events"]["items"][0];
        assert_eq!(decide["id"], "decide-1");
        assert_eq!(decide["message_link"], serde_json::Value::Null);
        assert_eq!(payload["summary"]["decide_now"], 1);
        assert_eq!(payload["summary"]["needs_triage"], 1);
    }

    #[test]
    fn review_failed_actions_surface_with_draft_links() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "Work", "op@example.com");
        // A completed action is routine activity, not a decision.
        db.log_action(
            "acc1",
            "auto_file",
            0.9,
            "filed newsletter",
            "{}",
            None,
            None,
        )
        .unwrap();
        db.conn()
            .execute(
                "INSERT INTO action_log (id, account_id, action_type, confidence, justification, action_taken, draft_id, action_status)
                 VALUES ('act-fail', 'acc1', 'send', 1.0, 'SMTP 550 relay refused', '{}', 'd-123', 'failed')",
                [],
            )
            .unwrap();

        let payload = super::build_review_json(&db, NOW).unwrap();

        assert_eq!(payload["decide_now"]["failed_actions"]["count"], 1);
        let failed = &payload["decide_now"]["failed_actions"]["items"][0];
        assert_eq!(failed["id"], "act-fail");
        assert_eq!(failed["action_status"], "failed");
        assert_eq!(failed["justification"], "SMTP 550 relay refused");
        assert_eq!(failed["draft_link"], "/accounts/acc1/drafts/d-123");
        // The completed action must not appear anywhere.
        let serialized = serde_json::to_string(&payload).unwrap();
        assert!(!serialized.contains("filed newsletter"));
    }

    #[test]
    fn review_operational_health_lists_only_failures() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "Work", "op@example.com");
        db.upsert_watch(envelope_email_store::WatchUpsert {
            account_id: "acc1",
            folder: "INBOX",
            status: "running",
            process_id: Some(1),
            schedule: Some("foreground"),
            last_heartbeat_at: Some("2026-05-09T08:59:00"),
            last_event_at: None,
            failure_reason: None,
        })
        .unwrap();
        db.upsert_watch(envelope_email_store::WatchUpsert {
            account_id: "acc1",
            folder: "Archive",
            status: "failed",
            process_id: None,
            schedule: Some("foreground"),
            last_heartbeat_at: Some("2026-05-09T08:00:00"),
            last_event_at: None,
            failure_reason: Some("IDLE connection dropped"),
        })
        .unwrap();
        db.record_failed_auth(
            "acc1",
            "imap",
            "LOGIN failed for password=secret-token",
            Some("Create an app password and retry verification."),
        )
        .unwrap();
        let route = db
            .create_event_route(
                "acc1",
                r#"{"event_types":["new_message"]}"#,
                r#"{"type":"webhook","url":"https://example.test/hook"}"#,
                true,
                100,
            )
            .unwrap();
        db.enqueue_delivery("d1", "e1", &route.id, "dk1", "2000-01-01T00:00:00")
            .unwrap();
        db.record_delivery_failure(
            "d1",
            Some(500),
            None,
            Some("boom"),
            None,
            "2026-05-09T00:00:00",
        )
        .unwrap();

        let payload = super::build_review_json(&db, NOW).unwrap();

        assert_eq!(payload["operational_health"]["failed_watches"]["count"], 1);
        let watch = &payload["operational_health"]["failed_watches"]["items"][0];
        assert_eq!(watch["folder"], "Archive");
        assert_eq!(watch["failure_reason"], "IDLE connection dropped");

        assert_eq!(payload["operational_health"]["failed_auth"]["count"], 1);
        let auth = &payload["operational_health"]["failed_auth"]["items"][0];
        assert_eq!(auth["backend"], "imap");
        assert!(!auth["reason"].as_str().unwrap().contains("secret-token"));

        assert_eq!(payload["operational_health"]["dead_letters"]["count"], 1);
        let dead_route = &payload["operational_health"]["dead_letters"]["routes"][0];
        assert_eq!(dead_route["dead"], 1);
        assert_eq!(dead_route["account_label"], "Work");
        // 1 failed watch + 1 failed auth + 1 dead-lettered route. The healthy
        // running watch is not an operator problem and must not be counted.
        assert_eq!(payload["summary"]["operational_health"], 3);
        let serialized = serde_json::to_string(&payload).unwrap();
        assert!(
            !serialized.contains("\"INBOX\"") || {
                // The running watch specifically must be absent from health items.
                payload["operational_health"]["failed_watches"]["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|w| w["folder"] != "INBOX")
            }
        );
    }

    #[test]
    fn review_payload_excludes_recipient_body_and_secret_material() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "Work", "op@example.com");
        let draft = db
            .create_draft(
                "acc1",
                "secret-recipient@example.test",
                Some("Approve me"),
                Some("secret-body-marker"),
                None,
                None,
                None,
                None,
                Some("mcp"),
            )
            .unwrap();
        db.update_draft_status(&draft.id, DraftStatus::PendingReview)
            .unwrap();
        let scheduled = db
            .create_draft(
                "acc1",
                "counterparty@gmail.example",
                Some("Queued"),
                Some("scheduled-body-marker"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&scheduled.id, "2000-01-01T00:00:00")
            .unwrap();
        db.create_snoozed(
            "acc1",
            44,
            "INBOX",
            "Snoozed",
            "2026-05-09T08:30:00",
            None,
            Some("Waiting for them"),
            Some("waiting-reply"),
            None,
            Some("waiting-on@example.test"),
        )
        .unwrap();
        db.record_failed_auth("acc1", "imap", "LOGIN failed password=hunter2-secret", None)
            .unwrap();
        let route = db
            .create_event_route(
                "acc1",
                r#"{"event_types":["new_message"]}"#,
                r#"{"type":"webhook","url":"https://example.test/hook"}"#,
                true,
                100,
            )
            .unwrap();
        let full_secret = route.secret.clone().unwrap();
        db.enqueue_delivery("d1", "e1", &route.id, "dk1", "2000-01-01T00:00:00")
            .unwrap();
        db.record_delivery_failure(
            "d1",
            Some(500),
            None,
            Some("boom"),
            None,
            "2026-05-09T00:00:00",
        )
        .unwrap();

        let payload = super::build_review_json(&db, NOW).unwrap();

        // The queue is populated — the absences below are redaction, not
        // an empty payload.
        assert_eq!(
            payload["decide_now"]["drafts"]["counts"]["pending_review"],
            1
        );
        assert_eq!(payload["waiting"]["scheduled"]["count"], 1);

        let serialized = serde_json::to_string(&payload).unwrap();
        assert!(!serialized.contains("secret-recipient@example.test"));
        assert!(!serialized.contains("secret-body-marker"));
        assert!(!serialized.contains("counterparty@gmail.example"));
        assert!(!serialized.contains("scheduled-body-marker"));
        assert!(!serialized.contains("waiting-on@example.test"));
        assert!(!serialized.contains("hunter2-secret"));
        assert!(!serialized.contains(&full_secret));
        assert!(!serialized.contains("to_addr"));
        assert!(!serialized.contains("text_content"));
    }

    #[test]
    fn review_aggregate_is_read_only_against_the_store() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "Work", "op@example.com");
        let draft = db
            .create_draft(
                "acc1",
                "to@example.com",
                Some("Approve me"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("mcp"),
            )
            .unwrap();
        db.update_draft_status(&draft.id, DraftStatus::PendingReview)
            .unwrap();
        insert_event(
            &db,
            "pending-1",
            "acc1",
            "watch.message_matched",
            Some(7),
            false,
            "2026-05-09T08:58:00",
        );
        db.create_snoozed(
            "acc1",
            45,
            "INBOX",
            "Snoozed",
            "2026-05-09T08:30:00",
            None,
            Some("Due item"),
            None,
            None,
            None,
        )
        .unwrap();

        let first = super::build_review_json(&db, NOW).unwrap();
        assert_eq!(first["decide_now"]["drafts"]["counts"]["pending_review"], 1);
        assert_eq!(first["needs_triage"]["count"], 1);
        assert_eq!(first["waiting"]["due_snoozes"]["count"], 1);

        // Building the aggregate again returns the identical payload: nothing
        // was acked, unsnoozed, sent, or status-transitioned by the read.
        let second = super::build_review_json(&db, NOW).unwrap();
        assert_eq!(first, second);
        assert_eq!(db.list_unacked("acc1", 10).unwrap().len(), 1);
        assert_eq!(
            db.get_draft(&draft.id).unwrap().unwrap().status,
            DraftStatus::PendingReview
        );
        assert_eq!(db.list_snoozed(Some("acc1")).unwrap().len(), 1);
    }
}
