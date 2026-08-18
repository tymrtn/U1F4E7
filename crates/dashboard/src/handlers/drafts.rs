// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Draft list and dashboard operator actions.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use envelope_email_store::DraftStatus;
use envelope_email_store::models::Account;
use envelope_email_store::{Database, StoreError};
use envelope_email_transport::outbound::{
    OUTBOX_COOLDOWN_REASON, OUTBOX_COOLDOWN_REASON_CODE, resolve_cooldown_seconds,
};
use serde::Deserialize;
use serde_json::json;

use crate::state::AppState;
use crate::ui_paths::draft_dashboard_path;

fn dashboard_base_url(headers: &HeaderMap) -> Option<String> {
    let host = headers.get("host")?.to_str().ok()?.trim();
    if host.is_empty() {
        return None;
    }
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("http");
    Some(format!("{proto}://{host}"))
}

fn draft_dashboard_url(headers: &HeaderMap, account_id: &str, draft_id: &str) -> Option<String> {
    Some(format!(
        "{}{}",
        dashboard_base_url(headers)?,
        draft_dashboard_path(account_id, draft_id)
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftEditRequest {
    /// The [`Draft::revision`] the client was viewing when it composed this
    /// edit. Required: the server never re-reads-and-blesses the latest row —
    /// a concurrent change returns 409 instead of overwriting content the
    /// human never saw.
    pub expected_revision: i64,
    pub to_addr: Option<String>,
    pub cc_addr: Option<String>,
    pub bcc_addr: Option<String>,
    pub subject: Option<String>,
    /// The two body fields are one unit — the draft's body representation set.
    /// Sending either one replaces the body with exactly that pair and CLEARS
    /// the omitted alternate; sending neither leaves both bodies untouched. A
    /// single-format editor therefore cannot leave a stale alternate behind for
    /// `multipart/alternative` delivery to surface instead of the edit.
    pub text_content: Option<String>,
    pub html_content: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftBlockRequest {
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftApproveRequest {
    /// The [`Draft::revision`] the human reviewed. Required — approval is
    /// bound to the viewed revision; a concurrent edit returns 409.
    pub expected_revision: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftSendRequest {
    #[serde(default)]
    pub confirm: bool,
    /// The [`Draft::revision`] the human reviewed before clicking send.
    /// Required — the approval attestation is bound to this exact revision;
    /// a concurrent edit returns 409 and nothing is queued.
    pub expected_revision: i64,
    /// Optional override for the outbox cooldown (seconds). Omitted → the shared
    /// default cooldown. Negative values clamp to zero. There is intentionally no
    /// immediate-SMTP dashboard bypass: the queued draft is transmitted later by
    /// the shared scheduled-send sweep, after the Governor gate.
    #[serde(default)]
    pub cooldown_seconds: Option<i64>,
}

pub async fn list(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    let account = match resolve_account(&db, &account_id) {
        Ok(Some(account)) => account,
        Ok(None) => return (StatusCode::NOT_FOUND, "account not found").into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response();
        }
    };

    match db.list_drafts(&account.id, Some("draft"), 100, 0) {
        Ok(drafts) => Json(json!({ "drafts": drafts })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response(),
    }
}

pub async fn show(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((account_id, draft_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    let account = match resolve_account(&db, &account_id) {
        Ok(Some(account)) => account,
        Ok(None) => return (StatusCode::NOT_FOUND, "account not found").into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response();
        }
    };

    let draft = match db.get_draft(&draft_id) {
        Ok(Some(draft)) if draft.account_id == account.id => draft,
        Ok(Some(_)) | Ok(None) => {
            return (StatusCode::NOT_FOUND, "draft not found").into_response();
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response();
        }
    };

    let dashboard_path = draft_dashboard_path(&account.id, &draft.id);
    let dashboard_url = draft_dashboard_url(&headers, &account.id, &draft.id);

    Json(json!({
        "draft": draft,
        "account": account,
        "dashboard_path": dashboard_path,
        "dashboard_url": dashboard_url,
        "review_url": dashboard_url,
        "metadata": {
            "dashboard_path": dashboard_path,
            "dashboard_url": dashboard_url,
            "review_url": dashboard_url,
        },
    }))
    .into_response()
}

pub async fn show_by_imap_uid(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((account_id, imap_uid)): Path<(String, u32)>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    let account = match resolve_account(&db, &account_id) {
        Ok(Some(account)) => account,
        Ok(None) => return (StatusCode::NOT_FOUND, "account not found").into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response();
        }
    };

    let draft = match db.get_draft_by_imap_uid(&account.id, imap_uid) {
        Ok(Some(draft)) => draft,
        Ok(None) => return (StatusCode::NOT_FOUND, "draft not found").into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response();
        }
    };

    let dashboard_path = draft_dashboard_path(&account.id, &draft.id);
    let dashboard_url = draft_dashboard_url(&headers, &account.id, &draft.id);

    Json(json!({
        "draft": draft,
        "account": account,
        "dashboard_path": dashboard_path,
        "dashboard_url": dashboard_url,
        "review_url": dashboard_url,
        "source": {
            "kind": "imap_uid",
            "imap_uid": imap_uid,
        },
        "metadata": {
            "dashboard_path": dashboard_path,
            "dashboard_url": dashboard_url,
            "review_url": dashboard_url,
        },
    }))
    .into_response()
}

pub(crate) fn resolve_account(
    db: &Database,
    account_id: &str,
) -> Result<Option<Account>, StoreError> {
    if let Some(account) = db.get_account(account_id)? {
        return Ok(Some(account));
    }
    db.find_account_by_email(account_id)
}

pub async fn approve(
    State(state): State<AppState>,
    Path((account_id, draft_id)): Path<(String, String)>,
    Json(req): Json<DraftApproveRequest>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    // The dashboard approve action is an explicit human decision: the status
    // flip and the durable attestation land in one store transaction,
    // compare-and-set against the revision the human VIEWED (carried on the
    // request — never a server-side re-read of the latest row). A concurrent
    // content edit rolls the whole approval back (409) — the edited draft
    // never inherits `tyler_approved`.
    match ensure_draft_account(&db, &account_id, &draft_id).and_then(|draft| {
        db.approve_draft_revision(
            &draft.id,
            req.expected_revision,
            "human:dashboard",
            &crate::timefmt::utc_now_string(),
        )
    }) {
        Ok(draft) => {
            state
                .events
                .publish(crate::events::DashboardEvent::DraftStatusChanged {
                    account_id: draft.account_id.clone(),
                    draft_id: draft.id.clone(),
                    status: DraftStatus::Draft.as_str().to_string(),
                });
            Json(json!({ "draft": draft, "status": "approved" })).into_response()
        }
        Err(e) => draft_error(e),
    }
}

pub async fn edit(
    State(state): State<AppState>,
    Path((account_id, draft_id)): Path<(String, String)>,
    Json(req): Json<DraftEditRequest>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    match ensure_draft_account(&db, &account_id, &draft_id).and_then(|draft| {
        db.update_draft_content_for_revision(
            &draft.id,
            req.expected_revision,
            req.to_addr.as_deref(),
            req.cc_addr.as_deref().or(draft.cc_addr.as_deref()),
            req.bcc_addr.as_deref().or(draft.bcc_addr.as_deref()),
            req.subject.as_deref(),
            req.text_content.as_deref(),
            req.html_content.as_deref(),
        )
    }) {
        Ok(draft) => Json(json!({ "draft": draft, "status": "edited" })).into_response(),
        Err(e) => draft_error(e),
    }
}

pub async fn discard(
    State(state): State<AppState>,
    Path((account_id, draft_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    // Resolve the canonical account id up front so the emitted event carries the
    // account id, not a caller-supplied email alias.
    let resolved_account = ensure_draft_account(&db, &account_id, &draft_id)
        .map(|draft| draft.account_id)
        .unwrap_or_else(|_| account_id.clone());
    match ensure_draft_account(&db, &account_id, &draft_id)
        .and_then(|_| db.discard_draft(&draft_id))
    {
        Ok(true) => {
            state
                .events
                .publish(crate::events::DashboardEvent::DraftStatusChanged {
                    account_id: resolved_account,
                    draft_id: draft_id.clone(),
                    status: DraftStatus::Discarded.as_str().to_string(),
                });
            Json(json!({ "draft_id": draft_id, "status": "discarded" })).into_response()
        }
        Ok(false) => (StatusCode::CONFLICT, "draft is not discardable").into_response(),
        Err(e) => draft_error(e),
    }
}

pub async fn block(
    State(state): State<AppState>,
    Path((account_id, draft_id)): Path<(String, String)>,
    Json(req): Json<DraftBlockRequest>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    match ensure_draft_account(&db, &account_id, &draft_id)
        .and_then(|_| db.update_draft_status(&draft_id, DraftStatus::Blocked))
        .and_then(|_| db.get_draft(&draft_id))
    {
        Ok(Some(draft)) => {
            state
                .events
                .publish(crate::events::DashboardEvent::DraftStatusChanged {
                    account_id: draft.account_id.clone(),
                    draft_id: draft.id.clone(),
                    status: DraftStatus::Blocked.as_str().to_string(),
                });
            Json(json!({
                "draft": draft,
                "status": "blocked",
                "reason": req.reason.unwrap_or_else(|| "changes requested".to_string())
            }))
            .into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "draft not found").into_response(),
        Err(e) => draft_error(e),
    }
}

/// Queue an approved draft into the Envelope outbox cooldown instead of
/// transmitting it inline.
///
/// Setting `send_after` (and ensuring the draft stays in sweep-eligible `draft`
/// status) hands the real send to the shared scheduled-send sweep
/// (`run_scheduled_send_sweep`), which applies persisted attachment
/// snapshots, reply threading headers, and the fail-closed Governor gate before
/// any SMTP — exactly like CLI `draft send` and MCP `send_draft`. The draft is
/// never marked sent here (that would drop it from the sweep and could strand it
/// unsent) and never discarded.
///
/// Promotion, `send_after`, and the human-approval attestation are one atomic
/// store transaction (`queue_draft_with_human_approval`), compare-and-set
/// against `expected_revision` — the revision the human VIEWED, carried on the
/// request rather than re-read server-side. A concurrent content edit rolls
/// everything back: no partially queued, unapproved state, and the edited
/// content never inherits `tyler_approved` (an input attribute for Governor's
/// blind scoring, never a gate bypass).
///
/// Returns the resolved `send_after` timestamp and the exact attested [`Draft`]
/// row the atomic queue produced (no post-commit reload).
fn queue_draft_for_outbox(
    db: &Database,
    draft_id: &str,
    expected_revision: i64,
    cooldown_seconds: i64,
) -> envelope_email_store::errors::Result<(String, envelope_email_store::Draft)> {
    let send_at = (chrono::Utc::now() + chrono::Duration::seconds(cooldown_seconds))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let attested = db.queue_draft_with_human_approval(
        draft_id,
        expected_revision,
        &send_at,
        "human:dashboard",
        &crate::timefmt::utc_now_string(),
    )?;
    Ok((send_at, attested))
}

pub async fn send(
    State(state): State<AppState>,
    Path((account_id, draft_id)): Path<(String, String)>,
    Json(req): Json<DraftSendRequest>,
) -> impl IntoResponse {
    if !req.confirm {
        return (
            StatusCode::BAD_REQUEST,
            "draft send mutates the outside world; send confirm=true",
        )
            .into_response();
    }

    let db = state.db.lock().await;
    let draft = match ensure_draft_account(&db, &account_id, &draft_id) {
        Ok(draft) => draft,
        Err(e) => return draft_error(e),
    };

    if draft.status == DraftStatus::Sent {
        return (StatusCode::CONFLICT, "draft already sent").into_response();
    }

    // Do NOT transmit immediately. Queue into the outbox cooldown so the shared
    // scheduled-send sweep performs the real SMTP send (with attachments,
    // threading, and the Governor gate). This keeps the dashboard send path
    // aligned with CLI/MCP/scheduled outbound semantics; there is no immediate
    // dashboard SMTP bypass.
    let cooldown = resolve_cooldown_seconds(req.cooldown_seconds);
    match queue_draft_for_outbox(&db, &draft.id, req.expected_revision, cooldown) {
        Ok((send_after, attested)) => {
            // The additive human-origin attribution block (tyler_approved derived;
            // no fabricated bot declaration) is built from the EXACT attested row
            // the queue returned — no reload, no pre-attestation fallback — with
            // the account domain derived from the username exactly as the sweep
            // does. If the account cannot be loaded, fail rather than fabricate.
            let account = match db.get_account(&draft.account_id) {
                Ok(Some(a)) => a,
                Ok(None) => {
                    return draft_error(envelope_email_store::StoreError::AccountNotFound(
                        draft.account_id.clone(),
                    ));
                }
                Err(e) => return draft_error(e),
            };
            let attribution = crate::human_queue_attribution_block(&attested, &account.username);
            state
                .events
                .publish(crate::events::DashboardEvent::DraftQueued {
                    account_id: draft.account_id.clone(),
                    draft_id: draft.id.clone(),
                    origin: "queue",
                });
            Json(json!({
                "draft_id": draft.id,
                "sent": false,
                "status": "queued",
                "send_after": send_after,
                "cooldown_seconds": cooldown,
                "queued_reason_code": OUTBOX_COOLDOWN_REASON_CODE,
                "queued_reason": OUTBOX_COOLDOWN_REASON,
                "attribution": attribution,
            }))
            .into_response()
        }
        Err(e) => draft_error(e),
    }
}

fn ensure_draft_account(
    db: &envelope_email_store::Database,
    account_id: &str,
    draft_id: &str,
) -> envelope_email_store::errors::Result<envelope_email_store::Draft> {
    let resolved = match db.find_account_by_email(account_id)? {
        Some(acct) => acct.id,
        None => account_id.to_string(),
    };
    let draft = db
        .get_draft(draft_id)?
        .ok_or_else(|| envelope_email_store::StoreError::DraftNotFound(draft_id.to_string()))?;
    if draft.account_id != resolved {
        return Err(envelope_email_store::StoreError::DraftNotFound(
            draft_id.to_string(),
        ));
    }
    Ok(draft)
}

fn draft_error(e: envelope_email_store::StoreError) -> axum::response::Response {
    match e {
        envelope_email_store::StoreError::DraftNotFound(_) => {
            (StatusCode::NOT_FOUND, format!("{e}")).into_response()
        }
        envelope_email_store::StoreError::DraftNotEditable(_) => {
            (StatusCode::CONFLICT, format!("{e}")).into_response()
        }
        envelope_email_store::StoreError::DraftModifiedConcurrently(_) => {
            (StatusCode::CONFLICT, format!("{e}")).into_response()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use envelope_email_store::{Database, DraftStatus};

    #[test]
    fn draft_operator_primitives_are_account_scoped() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "a@example.com");
        seed_account(&db, "acc2", "b@example.com");
        let draft = db
            .create_draft(
                "acc1",
                "buyer@example.com",
                Some("Old"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_status(&draft.id, DraftStatus::PendingReview)
            .unwrap();

        assert!(super::ensure_draft_account(&db, "acc1", &draft.id).is_ok());
        assert!(super::ensure_draft_account(&db, "a@example.com", &draft.id).is_ok());
        assert!(super::ensure_draft_account(&db, "acc2", &draft.id).is_err());
        db.update_draft_status(&draft.id, DraftStatus::Draft)
            .unwrap();
        let edited = db
            .update_draft_content(
                &draft.id,
                Some("new@example.com"),
                None,
                None,
                Some("New"),
                Some("New body"),
                None,
            )
            .unwrap();
        assert_eq!(edited.to_addr, "new@example.com");
        assert_eq!(edited.subject.as_deref(), Some("New"));
        assert!(db.discard_draft(&draft.id).unwrap());
    }

    fn seed_account(db: &Database, id: &str, username: &str) {
        db.conn().execute("INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port, encrypted_password) VALUES (?1, 'Test', ?2, 'example.com', 'smtp.example.com', 587, 'imap.example.com', 993, 'encrypted')", (id, username)).unwrap();
    }

    /// Regression for issue #68: dashboard draft send must queue into the outbox
    /// cooldown (handing the real send to the shared scheduled-send sweep) rather
    /// than transmitting inline. It must never mark the draft sent itself, never
    /// discard it, and must preserve persisted attachments and reply threading.
    #[test]
    fn dashboard_send_queues_into_outbox_without_inline_transmit() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "agent@example.com");
        let draft = db
            .create_draft(
                "acc1",
                "buyer@example.com",
                Some("Re: order"),
                Some("Body"),
                None,
                Some("parent-msg-id"),
                None,
                None,
                Some("agent"),
            )
            .unwrap();

        // Persisted attachment snapshot + reply-threading metadata, as a
        // contextual reply draft would carry them.
        db.update_draft_attachments(
            &draft.id,
            &[serde_json::json!({
                "filename": "packet.pdf",
                "content_type": "application/pdf",
                "size": 5,
                "data_base64": "aGVsbG8=",
            })],
        )
        .unwrap();
        db.set_draft_metadata(
            &draft.id,
            &serde_json::json!({
                "in_reply_to": "<parent-msg-id>",
                "references": ["<root-id>", "<parent-msg-id>"],
            }),
        )
        .unwrap();
        let draft = db.get_draft(&draft.id).unwrap().unwrap();

        let (send_at, attested) =
            super::queue_draft_for_outbox(&db, &draft.id, draft.revision, 120).unwrap();

        // The queue returns the EXACT attested row (no reload): approved, promoted,
        // and scheduled — the row the handler builds its success block from.
        assert!(attested.human_approved(), "returned row is attested");
        assert_eq!(attested.status, DraftStatus::Draft);
        assert_eq!(attested.send_after.as_deref(), Some(send_at.as_str()));

        // The attribution block built from that row carries the real attachment
        // fact (derived from the persisted snapshot), agreeing with the sweep.
        let block = crate::human_queue_attribution_block(&attested, "agent@example.com");
        let derived: Vec<&str> = block["derived_attrs"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a.as_str())
            .collect();
        assert!(
            derived.contains(&"has_attachment"),
            "attachment fact derived from the attested row: {block}"
        );

        let reloaded = db.get_draft(&draft.id).unwrap().unwrap();
        // Queued, not sent: sweep-eligible status and a future send_after, with no
        // sent_at stamped by the dashboard path.
        assert_eq!(reloaded.status, DraftStatus::Draft);
        assert_eq!(reloaded.send_after.as_deref(), Some(send_at.as_str()));
        assert!(reloaded.sent_at.is_none());

        // A 120s cooldown means the sweep does not pick it up immediately, proving
        // there is no inline transmission.
        let due_now: Vec<_> = db
            .list_drafts_due_for_send()
            .unwrap()
            .into_iter()
            .filter(|d| d.id == reloaded.id)
            .collect();
        assert!(
            due_now.is_empty(),
            "queued draft must not be due before cooldown"
        );

        // Attachments and threading survive for the shared sweep send path.
        assert_eq!(reloaded.attachments.len(), 1);
        assert_eq!(reloaded.attachments[0]["filename"], "packet.pdf");
        let (irt, refs) = (
            reloaded
                .metadata
                .as_ref()
                .and_then(|m| m.get("in_reply_to"))
                .and_then(|v| v.as_str()),
            reloaded
                .metadata
                .as_ref()
                .and_then(|m| m.get("references"))
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0),
        );
        assert_eq!(irt, Some("<parent-msg-id>"));
        assert_eq!(refs, 2);

        // Once the cooldown elapses the shared sweep query selects it.
        let past = (chrono::Utc::now() - chrono::Duration::seconds(5))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        db.update_draft_send_after(&reloaded.id, &past).unwrap();
        let due_later: Vec<_> = db
            .list_drafts_due_for_send()
            .unwrap()
            .into_iter()
            .filter(|d| d.id == reloaded.id)
            .collect();
        assert_eq!(due_later.len(), 1, "due draft must reach the sweep");
    }

    /// A failed queue transition (stale/edited revision) must surface as an
    /// error — the handler then fails loud, never reports success off
    /// pre-attestation state. There is no reload/fallback path anymore.
    #[test]
    fn queue_for_outbox_transition_failure_is_an_error_not_stale_success() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "agent@example.com");
        let draft = db
            .create_draft(
                "acc1",
                "buyer@example.com",
                Some("Hello"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        let stale_rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        // A concurrent edit bumps the revision after the human viewed `stale_rev`.
        db.update_draft_content(
            &draft.id,
            Some("attacker@evil.example"),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        let err = super::queue_draft_for_outbox(&db, &draft.id, stale_rev, 120)
            .expect_err("a stale-revision queue must fail rather than fall back");
        assert!(
            matches!(
                err,
                envelope_email_store::StoreError::DraftModifiedConcurrently(_)
            ),
            "{err:?}"
        );

        // Nothing was queued or approved off the stale revision.
        let after = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(
            !after.human_approved(),
            "no attestation persisted on a failed transition"
        );
        assert!(
            after.send_after.is_none(),
            "no schedule persisted on a failed transition"
        );
    }

    /// Queueing must promote a pending-review draft to sweep-eligible `draft`
    /// status (so the sweep can find it) without ever marking it sent.
    #[test]
    fn dashboard_send_promotes_pending_review_draft_to_sweep_eligible() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "agent@example.com");
        let draft = db
            .create_draft(
                "acc1",
                "buyer@example.com",
                Some("Hello"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_status(&draft.id, DraftStatus::PendingReview)
            .unwrap();
        let draft = db.get_draft(&draft.id).unwrap().unwrap();

        super::queue_draft_for_outbox(&db, &draft.id, draft.revision, 0).unwrap();
        let reloaded = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(reloaded.status, DraftStatus::Draft);
        assert!(reloaded.sent_at.is_none());
    }

    /// The dashboard queue/send action is an explicit human decision: it must
    /// durably record the sanitized approval attestation so the scheduled
    /// sweep re-scores with `tyler_approved`. Agent-created state alone (the
    /// draft was created_by=agent) must not derive as approved before the
    /// human action.
    #[test]
    fn dashboard_send_records_durable_human_approval_attestation() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "agent@example.com");
        let draft = db
            .create_draft(
                "acc1",
                "buyer@example.com",
                Some("Hello"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_status(&draft.id, DraftStatus::PendingReview)
            .unwrap();
        let draft = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(
            !draft.human_approved(),
            "agent-created state alone must not self-approve"
        );

        super::queue_draft_for_outbox(&db, &draft.id, draft.revision, 120).unwrap();

        let reloaded = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(reloaded.human_approved());
        let attestation = &reloaded.metadata.as_ref().unwrap()["human_approval"];
        assert_eq!(attestation["approved_by"], "human:dashboard");
        let approved_at = attestation["approved_at"].as_str().unwrap();
        assert!(
            approved_at.ends_with('Z'),
            "attestation timestamp is canonical RFC 3339 UTC, got {approved_at}"
        );
        // Sanitized: no recipient address anywhere in the attestation.
        assert!(!attestation.to_string().contains("buyer@example.com"));
        // Canonical UTC queue timestamp.
        assert!(reloaded.send_after.unwrap().ends_with('Z'));
    }

    /// Handler-boundary regression for revision binding: an edit through the
    /// dashboard edit primitive after a human queue/approval must invalidate
    /// the attestation, so the changed content cannot ride the earlier
    /// `tyler_approved`. A fresh human send re-stamps the new revision.
    #[test]
    fn edit_after_dashboard_send_invalidates_the_approval() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "agent@example.com");
        let draft = db
            .create_draft(
                "acc1",
                "buyer@example.com",
                Some("Hello"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        let draft = db.get_draft(&draft.id).unwrap().unwrap();

        super::queue_draft_for_outbox(&db, &draft.id, draft.revision, 120).unwrap();
        assert!(db.get_draft(&draft.id).unwrap().unwrap().human_approved());

        // The same store call the dashboard edit handler makes.
        db.update_draft_content(
            &draft.id,
            Some("someone-else@example.net"),
            None,
            None,
            None,
            Some("changed body"),
            None,
        )
        .unwrap();
        let edited = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(
            !edited.human_approved(),
            "post-approval edit must clear the human-approval attestation"
        );

        // Re-queueing (a fresh human send) approves the new revision.
        super::queue_draft_for_outbox(&db, &edited.id, edited.revision, 120).unwrap();
        assert!(db.get_draft(&draft.id).unwrap().unwrap().human_approved());
    }

    /// Queueing must not override explicit blocked/discarded terminal operator
    /// states. A blocked draft means changes were requested; it should not be
    /// silently promoted into the outbox by an API call.
    #[test]
    fn dashboard_send_does_not_queue_blocked_or_discarded_drafts() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "agent@example.com");
        let draft = db
            .create_draft(
                "acc1",
                "buyer@example.com",
                Some("Hello"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();

        db.update_draft_status(&draft.id, DraftStatus::Blocked)
            .unwrap();
        let blocked = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(super::queue_draft_for_outbox(&db, &blocked.id, blocked.revision, 0).is_err());
        assert_eq!(
            db.get_draft(&draft.id).unwrap().unwrap().status,
            DraftStatus::Blocked
        );

        db.update_draft_status(&draft.id, DraftStatus::Discarded)
            .unwrap();
        let discarded = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(super::queue_draft_for_outbox(&db, &discarded.id, discarded.revision, 0).is_err());
        assert_eq!(
            db.get_draft(&draft.id).unwrap().unwrap().status,
            DraftStatus::Discarded
        );
    }

    /// Block 7: the dashboard draft-send handler RESPONSE carries the same
    /// sanitized additive `attribution` block as CLI/MCP, reflecting the durable
    /// human attestation (tyler_approved) — never a fabricated bot declaration.
    #[tokio::test]
    async fn dashboard_send_response_carries_human_attribution_block() {
        use axum::extract::{Path as AxumPath, State as AxumState};
        use axum::response::IntoResponse;

        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "agent@example.com");
        let draft = db
            .create_draft(
                "acc1",
                "buyer@example.com",
                Some("Hi"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("human:dashboard"),
            )
            .unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        let state = crate::AppState::new(
            db,
            envelope_email_store::credential_store::CredentialBackend::File,
        );

        let resp = super::send(
            AxumState(state),
            AxumPath(("acc1".to_string(), draft.id.clone())),
            axum::Json(super::DraftSendRequest {
                confirm: true,
                expected_revision: rev,
                cooldown_seconds: Some(120),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"], "queued");
        let attr = &v["attribution"];
        assert_eq!(attr["attribution_state"], "attributed");
        assert!(
            attr["declared_attrs"].as_array().unwrap().is_empty(),
            "no fabricated bot declaration in the dashboard response"
        );
        assert!(
            attr["derived_attrs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a == "tyler_approved"),
            "response advertises the durable human attestation: {v}"
        );
        assert_eq!(attr["governor"], serde_json::Value::Null);
        assert!(!v.to_string().contains("\"score\""), "no score leaked");
    }

    /// The dashboard send handler source must not call any direct simple-send
    /// path — that bypasses the cooldown, Governor gate, attachments, and
    /// threading. This guards against a regression reintroducing it.
    #[test]
    fn dashboard_send_does_not_call_direct_simple_send() {
        let source = include_str!("drafts.rs");
        // Build the call-form needle by concatenation so this assertion's own
        // source does not match it.
        let needle = format!("send_simple{}", "(");
        assert!(
            !source.contains(&needle),
            "dashboard draft send must not call the direct simple-send path"
        );
    }
}
