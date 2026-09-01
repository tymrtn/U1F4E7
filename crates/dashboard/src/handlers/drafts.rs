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
    /// default cooldown. Negative values clamp to zero.
    #[serde(default)]
    pub cooldown_seconds: Option<i64>,
    /// Send this message immediately instead of waiting out the outbox cooldown.
    ///
    /// This is NOT a transmit bypass. The draft is queued exactly as it always
    /// is — zero cooldown — and then the shared scheduled-send sweep is run at
    /// once, so the message travels the identical path (claim, attachments,
    /// threading, Governor gate) that a cooldown send takes a minute later. The
    /// only thing skipped is the waiting.
    #[serde(default)]
    pub send_now: bool,
}

/// Serialize a draft for the JSON API with attachment bytes stripped.
///
/// Draft attachment entries carry `data_base64` — the snapshot the send sweep
/// transmits from. The store is explicit that the field is never logged or
/// echoed, and the CLI has stripped it since its first attachment listing
/// (`cli::commands::attachments::attachment_summary`). Serializing the raw row
/// put it on the wire anyway: every draft fetch shipped every attachment in
/// full to the browser, which then rendered a count. Callers that want the
/// bytes ask for one file by name at
/// `GET /accounts/{id}/drafts/{draft_id}/attachments/{filename}`.
pub(crate) fn draft_json(draft: &envelope_email_store::Draft) -> serde_json::Value {
    let mut value = serde_json::to_value(draft).unwrap_or_else(|_| json!({}));
    if let Some(attachments) = value.get_mut("attachments").and_then(|a| a.as_array_mut()) {
        for entry in attachments.iter_mut() {
            if let Some(object) = entry.as_object_mut() {
                object.remove("data_base64");
            }
        }
    }
    value
}

/// [`draft_json`] over a slice, for list responses.
fn drafts_json(drafts: &[envelope_email_store::Draft]) -> Vec<serde_json::Value> {
    drafts.iter().map(draft_json).collect()
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
        Ok(drafts) => Json(json!({ "drafts": drafts_json(&drafts) })).into_response(),
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
        "draft": draft_json(&draft),
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
        "draft": draft_json(&draft),
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
    //
    // Review only. It queues nothing, so it sends nothing, and it writes no
    // `human_send` authorization, so a later agent send of this draft (CLI
    // `draft send`, MCP `send_draft`) is still fully Governor-gated. Sending
    // from the dashboard is Human-only Send ([`send`]) — a separate click.
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
            Json(json!({ "draft": draft_json(&draft), "status": "approved" })).into_response()
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
        Ok(draft) => {
            Json(json!({ "draft": draft_json(&draft), "status": "edited" })).into_response()
        }
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

/// Take a queued draft back out of the outbox without discarding it.
///
/// The destructive counterpart lives at `/discard`. This one clears
/// `send_after` and leaves the row in `draft` status, so the review composer
/// unlocks and the operator can finish the message and re-queue it later —
/// which is what "I need more time" actually means, and what discarding a
/// queued draft could never give them.
///
/// Deliberately NOT revision-guarded. Hold only ever removes a pending send, so
/// there is nothing a concurrent edit could make unsafe about it; refusing on a
/// stale revision would strand an operator watching a countdown they cannot
/// stop. The real race — the sweep having already claimed the row — is settled
/// in the store by the `status = 'draft'` guard, which surfaces here as 409.
pub async fn hold(
    State(state): State<AppState>,
    Path((account_id, draft_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    match ensure_draft_account(&db, &account_id, &draft_id)
        .and_then(|draft| db.hold_scheduled_draft(&draft.id))
    {
        Ok(draft) => {
            // The status did not change (`draft` → `draft`), but the queue did:
            // any surface showing this row as scheduled — the cockpit panel, an
            // open second tab — has to drop the countdown.
            state
                .events
                .publish(crate::events::DashboardEvent::DraftStatusChanged {
                    account_id: draft.account_id.clone(),
                    draft_id: draft.id.clone(),
                    status: DraftStatus::Draft.as_str().to_string(),
                });
            Json(json!({ "draft": draft_json(&draft), "status": "held" })).into_response()
        }
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
                "draft": draft_json(&draft),
                "status": "blocked",
                "reason": req.reason.unwrap_or_else(|| "changes requested".to_string())
            }))
            .into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "draft not found").into_response(),
        Err(e) => draft_error(e),
    }
}

/// Queue the operator's **Human-only Send** into the Envelope outbox cooldown
/// instead of transmitting it inline.
///
/// Setting `send_after` (and ensuring the draft stays in sweep-eligible `draft`
/// status) hands the real send to the shared scheduled-send sweep
/// (`run_scheduled_send_sweep`), which applies persisted attachment
/// snapshots and reply threading headers before any SMTP — exactly like CLI
/// `draft send` and MCP `send_draft`. The draft is never marked sent here (that
/// would drop it from the sweep and could strand it unsent) and never discarded.
///
/// Promotion, `send_after`, the human-approval attestation, and the `human_send`
/// authorization are one atomic store transaction
/// (`queue_draft_with_human_send`), compare-and-set against `expected_revision`
/// — the revision the human VIEWED, carried on the request rather than re-read
/// server-side. A concurrent content edit rolls everything back: no partially
/// queued state, and no authorization for a send that was not queued.
///
/// That authorization is what the sweep's Human-only Send exception reads
/// (`dashboard_human_send_authorized`): this queue transition is the only thing
/// that mints it, so the exception can only ever cover a send the operator
/// started from here. Hold, an edit, and an agent re-queue each withdraw it, and
/// approving a draft never creates one.
///
/// Returns the resolved `send_after` timestamp and the exact authorized [`Draft`]
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
    let attested = db.queue_draft_with_human_send(
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

    // The dashboard never opens its own SMTP socket. Every send queues into the
    // outbox and the shared scheduled-send sweep performs the real transmission
    // (attachments, threading, Governor gate), keeping this path identical to
    // CLI/MCP/scheduled outbound.
    //
    // `send_now` changes the WAIT, not the PATH: the cooldown is zero and the
    // sweep is kicked immediately after the queue commits, so the operator who
    // means "now" gets now instead of watching a countdown they did not ask for.
    let cooldown = if req.send_now {
        0
    } else {
        resolve_cooldown_seconds(req.cooldown_seconds)
    };
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
            let attribution =
                crate::human_queue_attribution_block(&db, &attested, &account.username);
            state
                .events
                .publish(crate::events::DashboardEvent::DraftQueued {
                    account_id: draft.account_id.clone(),
                    draft_id: draft.id.clone(),
                    origin: if req.send_now { "send_now" } else { "queue" },
                });
            // Release the DB lock before the sweep runs — it takes the same lock
            // to claim the row, and holding it here would deadlock the send we
            // just atomically acknowledged.
            drop(db);
            if req.send_now {
                let sweep_state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = crate::run_scheduled_send_sweep(&sweep_state).await {
                        tracing::warn!("send-now sweep failed: {e}");
                    }
                });
            }
            Json(json!({
                "draft_id": draft.id,
                "sent": false,
                "status": if req.send_now { "sending" } else { "queued" },
                "send_after": send_after,
                "cooldown_seconds": cooldown,
                "send_now": req.send_now,
                "queued_reason_code": OUTBOX_COOLDOWN_REASON_CODE,
                "queued_reason": OUTBOX_COOLDOWN_REASON,
                "attribution": attribution,
            }))
            .into_response()
        }
        Err(e) => draft_error(e),
    }
}

pub(crate) fn ensure_draft_account(
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

pub(crate) fn draft_error(e: envelope_email_store::StoreError) -> axum::response::Response {
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
        envelope_email_store::StoreError::DraftNotScheduled(_) => {
            (StatusCode::CONFLICT, format!("{e}")).into_response()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use envelope_email_store::{Database, DraftStatus};

    use super::{draft_json, drafts_json};

    /// Attachment bytes must not ride the JSON API. The store is explicit that
    /// `data_base64` is never logged or echoed, and the CLI has stripped it
    /// from every attachment listing since the field existed; serializing the
    /// raw draft row put it on the wire regardless, shipping every attachment
    /// in full on each fetch to a client that needs only name, type, and size.
    #[test]
    fn draft_json_strips_attachment_bytes_and_keeps_the_metadata() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "a@example.com");
        let draft = db
            .create_draft(
                "acc1",
                "buyer@example.com",
                Some("With attachments"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_attachments(
            &draft.id,
            &[
                serde_json::json!({
                    "filename": "packet.pdf",
                    "content_type": "application/pdf",
                    "size": 5,
                    "data_base64": "aGVsbG8=",
                }),
                serde_json::json!({
                    "filename": "notes.txt",
                    "content_type": "text/plain",
                    "size": 3,
                    "data_base64": "Zm9v",
                }),
            ],
        )
        .unwrap();
        let stored = db.get_draft(&draft.id).unwrap().unwrap();

        let value = draft_json(&stored);
        let serialized = serde_json::to_string(&value).unwrap();
        assert!(
            !serialized.contains("data_base64"),
            "draft JSON must not carry the bytes field: {serialized}"
        );
        assert!(!serialized.contains("aGVsbG8="));
        assert!(!serialized.contains("Zm9v"));

        // Everything the client actually renders survives.
        let attachments = value["attachments"].as_array().unwrap();
        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0]["filename"], "packet.pdf");
        assert_eq!(attachments[0]["content_type"], "application/pdf");
        assert_eq!(attachments[0]["size"], 5);
        assert_eq!(attachments[1]["filename"], "notes.txt");
        // And so does the rest of the draft.
        assert_eq!(value["id"], stored.id);
        assert_eq!(value["subject"], "With attachments");
        assert_eq!(value["revision"], stored.revision);

        // The bytes are still in the store — only the wire form drops them.
        assert_eq!(stored.attachments[0]["data_base64"], "aGVsbG8=");

        // The list form strips identically.
        let listed = serde_json::to_string(&drafts_json(&[stored])).unwrap();
        assert!(!listed.contains("data_base64"), "list form: {listed}");
        assert!(listed.contains("packet.pdf"));
    }

    /// Every handler that returns a draft must route it through [`draft_json`].
    /// `hold` did not: it was added after the stripping fix was written, so it
    /// serialized the raw row and shipped `data_base64` on every hold. The
    /// stripping helper only protects the responses that actually call it, so
    /// guard the call sites rather than trusting each new handler to remember.
    #[test]
    fn every_draft_response_strips_attachment_bytes() {
        let source = include_str!("drafts.rs");
        let raw: Vec<&str> = source
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("//"))
            .filter(|line| {
                (line.contains("\"draft\":") || line.contains("\"drafts\":"))
                    && !line.contains("draft_json")
                    && !line.contains("drafts_json")
            })
            .collect();
        assert!(
            raw.is_empty(),
            "these responses serialize a raw draft and leak data_base64 — \
             wrap each in draft_json()/drafts_json(): {raw:?}"
        );
    }

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
        let block = crate::human_queue_attribution_block(&db, &attested, "agent@example.com");
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
        // The send authorization the sweep's Human-only Send exception reads,
        // minted by this queue transition and bound to the queued revision.
        assert_eq!(reloaded.human_send_surface(), Some("human:dashboard"));
        let authorization = &reloaded.metadata.as_ref().unwrap()["human_send"];
        assert_eq!(authorization["revision"], draft.revision);
        // Sanitized: no recipient address anywhere in either record.
        assert!(!attestation.to_string().contains("buyer@example.com"));
        assert!(!authorization.to_string().contains("buyer@example.com"));
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
        assert_eq!(
            edited.human_send_surface(),
            None,
            "post-send edit must clear the send authorization too"
        );

        // Re-queueing (a fresh human send) approves and authorizes the new
        // revision.
        super::queue_draft_for_outbox(&db, &edited.id, edited.revision, 120).unwrap();
        let requeued = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(requeued.human_approved());
        assert_eq!(requeued.human_send_surface(), Some("human:dashboard"));
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
                send_now: false,
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

    // ── Hold: unqueue without discarding ──────────────────────────────

    /// Queue a fresh draft through the real dashboard send path and hand back
    /// the state plus the draft id, so hold is exercised against a draft that
    /// was genuinely queued (attestation and all), not a hand-built row.
    async fn queued_through_dashboard_send() -> (crate::AppState, String) {
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
            AxumState(state.clone()),
            AxumPath(("acc1".to_string(), draft.id.clone())),
            axum::Json(super::DraftSendRequest {
                confirm: true,
                expected_revision: rev,
                // A long schedule, not a 60s undo window: hold has to work for
                // a send parked hours out, which is the case discard ruins.
                cooldown_seconds: Some(6 * 60 * 60),
                send_now: false,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        (state, draft.id)
    }

    async fn hold_response(
        state: &crate::AppState,
        account: &str,
        draft_id: &str,
    ) -> (axum::http::StatusCode, String) {
        use axum::extract::{Path as AxumPath, State as AxumState};
        use axum::response::IntoResponse;

        let resp = super::hold(
            AxumState(state.clone()),
            AxumPath((account.to_string(), draft_id.to_string())),
        )
        .await
        .into_response();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    /// The core promise: hold returns an editable draft, not a discarded one.
    #[tokio::test]
    async fn hold_clears_the_schedule_and_returns_an_editable_draft() {
        let (state, draft_id) = queued_through_dashboard_send().await;

        let (status, body) = hold_response(&state, "acc1", &draft_id).await;

        assert_eq!(status, axum::http::StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["status"], "held");
        assert_eq!(v["draft"]["status"], "draft");
        assert_eq!(v["draft"]["send_after"], serde_json::Value::Null);
        // The message survives — that is the whole difference from discard.
        assert_eq!(v["draft"]["subject"], "Hi");
        assert_eq!(v["draft"]["text_content"], "Body");

        let db = state.db.lock().await;
        let held = db.get_draft(&draft_id).unwrap().unwrap();
        assert_eq!(held.status, DraftStatus::Draft);
        assert!(held.send_after.is_none());
        assert!(
            db.list_drafts_due_for_send().unwrap().is_empty(),
            "a held draft must be out of the sweep's reach"
        );
    }

    /// Hold is account-scoped exactly like every other operator primitive: a
    /// caller naming the wrong account cannot unqueue someone else's draft.
    #[tokio::test]
    async fn hold_is_account_scoped() {
        let (state, draft_id) = queued_through_dashboard_send().await;
        {
            let db = state.db.lock().await;
            seed_account(&db, "acc2", "other@example.com");
        }

        let (status, _) = hold_response(&state, "acc2", &draft_id).await;

        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
        let db = state.db.lock().await;
        assert!(
            db.get_draft(&draft_id)
                .unwrap()
                .unwrap()
                .send_after
                .is_some(),
            "a cross-account hold must not touch the schedule"
        );
    }

    /// Once the sweep owns the row there is nothing to hold — report the
    /// conflict rather than pretending the send was stopped.
    #[tokio::test]
    async fn hold_conflicts_once_the_sweep_has_claimed_the_draft() {
        let (state, draft_id) = queued_through_dashboard_send().await;
        {
            let db = state.db.lock().await;
            // Bring the schedule due, then let the sweep claim it.
            db.update_draft_send_after(&draft_id, "2000-01-01T00:00:00")
                .unwrap();
            let rev = db.get_draft(&draft_id).unwrap().unwrap().revision;
            assert!(
                db.claim_draft_for_sending(&draft_id, rev)
                    .unwrap()
                    .is_some()
            );
        }

        let (status, _) = hold_response(&state, "acc1", &draft_id).await;

        assert_eq!(status, axum::http::StatusCode::CONFLICT);
        let db = state.db.lock().await;
        assert_eq!(
            db.get_draft(&draft_id).unwrap().unwrap().status,
            DraftStatus::Sending
        );
    }

    /// A draft that was never queued gets a truthful refusal, not a no-op 200
    /// that claims a schedule was cleared.
    #[tokio::test]
    async fn hold_conflicts_on_a_draft_that_was_never_queued() {
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
                Some("agent"),
            )
            .unwrap();
        let state = crate::AppState::new(
            db,
            envelope_email_store::credential_store::CredentialBackend::File,
        );

        let (status, _) = hold_response(&state, "acc1", &draft.id).await;

        assert_eq!(status, axum::http::StatusCode::CONFLICT);
    }

    /// Hold must never be a quiet discard. Guards the handler source against a
    /// regression that swaps the store primitive for the destructive one.
    #[test]
    fn hold_handler_never_discards() {
        let source = include_str!("drafts.rs");
        let hold_fn = source
            .split("pub async fn hold(")
            .nth(1)
            .and_then(|rest| rest.split("\npub async fn ").next())
            .expect("hold handler must exist");
        assert!(
            hold_fn.contains("hold_scheduled_draft"),
            "hold must go through the non-destructive store primitive"
        );
        assert!(
            !hold_fn.contains("discard_draft"),
            "hold must never discard the draft it unqueues"
        );
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
