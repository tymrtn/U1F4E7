// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Per-agent attribution feed for the v2 Agent Cockpit.
//!
//! `GET /api/agents` aggregates, read-only:
//!   * the agent identity roster (name, token prefix, created/revoked/last-used)
//!   * per-agent action + event counts from the additive `agent_id` audit columns
//!   * each agent's policy summary (send-mode ceiling + allow scopes)
//!   * drafts awaiting approval, grouped by the draft's `created_by` source label
//!
//! Draft *actions* (approve/edit/discard/block/send) are NOT performed here —
//! this endpoint only lists what awaits approval. The UI wires each row's
//! buttons to the existing per-account draft endpoints in `handlers::drafts`.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use envelope_email_store::{
    AgentPolicy, Database, Draft, DraftStatus, errors::Result as StoreResult,
};
use serde_json::{Value, json};

use crate::state::AppState;

pub async fn get(State(state): State<AppState>) -> impl IntoResponse {
    let db = state.db.lock().await;
    match build_agents_json(&db) {
        Ok(payload) => Json(payload).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("agents db error: {e}"),
        )
            .into_response(),
    }
}

fn build_agents_json(db: &Database) -> StoreResult<Value> {
    let identities = db.list_agents()?;
    let counts = db.agent_activity_counts()?;

    let agents: Vec<Value> = identities
        .iter()
        .map(|ident| {
            let activity = counts.get(&ident.id);
            let policy = db
                .get_agent_policy(&ident.id)
                .ok()
                .flatten()
                .unwrap_or_else(|| AgentPolicy::default_for(&ident.id));
            json!({
                "id": ident.id,
                "name": ident.name,
                "token_prefix": ident.token_prefix,
                "created_at": ident.created_at,
                "revoked_at": ident.revoked_at,
                "last_used_at": ident.last_used_at,
                "status": if ident.revoked_at.is_some() { "revoked" } else { "active" },
                "activity": {
                    "action_count": activity.map(|a| a.action_count).unwrap_or(0),
                    "event_count": activity.map(|a| a.event_count).unwrap_or(0),
                    "last_activity_at": activity.and_then(|a| a.last_activity_at.clone()),
                },
                "policy": policy_summary(&policy),
            })
        })
        .collect();

    // Drafts awaiting approval, aggregate across accounts, grouped by the draft
    // source label (`created_by`). Drafts do not carry a resolved agent identity
    // id, so this groups by the coarse source (`mcp`, `cli`, `agent`, …) — an
    // honest reflection of what the store records.
    let pending = db.list_all_drafts_by_status(DraftStatus::PendingReview.as_str(), 100)?;
    let blocked = db.list_all_drafts_by_status(DraftStatus::Blocked.as_str(), 100)?;
    let mut awaiting: Vec<&Draft> = pending.iter().chain(blocked.iter()).collect();
    awaiting.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    let mut groups: std::collections::BTreeMap<String, Vec<Value>> =
        std::collections::BTreeMap::new();
    for draft in awaiting {
        let source = draft
            .created_by
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        groups
            .entry(source)
            .or_default()
            .push(draft_approval_summary(draft));
    }
    let approval_queue: Vec<Value> = groups
        .into_iter()
        .map(
            |(source, drafts)| json!({ "source": source, "count": drafts.len(), "drafts": drafts }),
        )
        .collect();

    let awaiting_total: usize = approval_queue
        .iter()
        .map(|g| g.get("count").and_then(Value::as_u64).unwrap_or(0) as usize)
        .sum();

    Ok(json!({
        "agents": agents,
        "summary": {
            "agents": identities.len(),
            "active_agents": identities.iter().filter(|i| i.revoked_at.is_none()).count(),
            "awaiting_approval": awaiting_total,
        },
        "approval_queue": approval_queue,
    }))
}

/// A content-free policy summary: send-mode ceiling and whether each scope is
/// unrestricted (`"*"`) or a specific allowlist. Never emits the raw JSON
/// allowlist arrays to keep the card compact and non-leaky.
fn policy_summary(policy: &AgentPolicy) -> Value {
    let scope = |value: &str| -> &'static str {
        if value.trim() == "*" {
            "all"
        } else {
            "restricted"
        }
    };
    json!({
        "send_mode_ceiling": policy.send_mode_ceiling.as_str(),
        "accounts": scope(&policy.allowed_accounts),
        "folders": scope(&policy.allowed_folders),
        "actions": scope(&policy.allowed_actions),
        "recipients": match policy.allow_recipients.as_deref() {
            None | Some("*") => "all",
            Some(_) => "restricted",
        },
    })
}

/// Compact draft summary for the approval queue. Carries the fields the UI rows
/// need (subject, source, age, account) and the per-account action path so each
/// button targets the existing draft endpoints — no recipient bodies leaked.
pub(crate) fn draft_approval_summary(draft: &Draft) -> Value {
    json!({
        "id": draft.id,
        "account_id": draft.account_id,
        "subject": draft.subject,
        "status": draft.status.as_str(),
        "created_by": draft.created_by,
        "created_at": draft.created_at,
        "updated_at": draft.updated_at,
        "send_after": draft.send_after,
        // The revision the operator is viewing. Approve/edit/send requests
        // must echo it back as `expected_revision`; a concurrent edit -> 409.
        "revision": draft.revision,
        "action_base": format!("/api/accounts/{}/drafts/{}", draft.account_id, draft.id),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use envelope_email_store::Event;

    fn seed_account(db: &Database) {
        db.conn().execute("INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port, encrypted_password) VALUES ('acc1', 'Test', 'op@example.com', 'example.com', 'smtp.example.com', 587, 'imap.example.com', 993, 'x')", []).unwrap();
    }

    #[test]
    fn agents_endpoint_exposes_prefix_and_counts_never_hash() {
        let db = Database::open_memory().unwrap();
        seed_account(&db);
        let agent = db.create_agent("skippy").unwrap();
        db.insert_event_with_agent(
            &Event {
                id: "e1".into(),
                account_id: "acc1".into(),
                event_type: "agent.action".into(),
                folder: "INBOX".into(),
                uid: None,
                message_id: None,
                from_addr: None,
                subject: None,
                snippet: None,
                payload: None,
                idempotency_key: None,
                secure_pending: false,
                acked_at: None,
                created_at: "2026-07-01T00:00:00".into(),
            },
            Some(&agent.identity.id),
        )
        .unwrap();

        let payload = build_agents_json(&db).unwrap();
        assert_eq!(payload["summary"]["agents"], 1);
        assert_eq!(payload["agents"][0]["name"], "skippy");
        let prefix = payload["agents"][0]["token_prefix"].as_str().unwrap();
        assert!(prefix.starts_with("envtok_"));
        assert_eq!(payload["agents"][0]["activity"]["event_count"], 1);
        assert_eq!(payload["agents"][0]["status"], "active");
        assert_eq!(
            payload["agents"][0]["policy"]["send_mode_ceiling"],
            "draft-only"
        );
        // Never leak the token hash.
        let serialized = serde_json::to_string(&payload).unwrap();
        assert!(!serialized.contains("token_hash"));
    }

    #[test]
    fn approval_queue_groups_pending_drafts_by_source() {
        let db = Database::open_memory().unwrap();
        seed_account(&db);
        let d = db
            .create_draft(
                "acc1",
                "to@example.com",
                Some("Review me"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("mcp"),
            )
            .unwrap();
        db.update_draft_status(&d.id, DraftStatus::PendingReview)
            .unwrap();

        let payload = build_agents_json(&db).unwrap();
        assert_eq!(payload["summary"]["awaiting_approval"], 1);
        assert_eq!(payload["approval_queue"][0]["source"], "mcp");
        assert_eq!(
            payload["approval_queue"][0]["drafts"][0]["subject"],
            "Review me"
        );
        assert_eq!(
            payload["approval_queue"][0]["drafts"][0]["action_base"],
            format!("/api/accounts/acc1/drafts/{}", d.id)
        );
    }
}
