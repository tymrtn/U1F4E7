// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Draft attachment list, upload, removal, and download.
//!
//! Draft attachments live inside the draft row as a JSON array, each entry
//! carrying `filename`, `content_type`, `size`, and the base64 bytes
//! (`data_base64`) snapshotted at attach time so a later send does not depend
//! on the operator's file still existing.
//!
//! Two rules shape this module:
//!
//!   • **Bytes never ride the JSON API.** `data_base64` is stripped from every
//!     draft body this crate serializes ([`crate::handlers::drafts::draft_json`])
//!     and is reachable only through [`download`], which streams one named
//!     attachment. A review page that inlined the bytes would ship every
//!     attachment in full on each poll and echo the field the store explicitly
//!     forbids echoing.
//!   • **Attaching is editing.** Adding or removing a file changes what will be
//!     sent, so both mutations carry the revision the operator was shown, bump
//!     the revision, and drop the approval attestation — the same contract as
//!     a body edit in [`crate::handlers::drafts::edit`].

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Deserialize;
use serde_json::json;

use crate::handlers::attachments::attachment_disposition;
use crate::handlers::drafts::{draft_error, draft_json, ensure_draft_account};
use crate::state::AppState;

/// Total attachment bytes one draft may carry, matching the 25 MB ceiling the
/// major providers enforce at the far end. Rejecting here — with the running
/// total in the message — beats letting the operator attach 60 MB and only
/// learn at SMTP time, after the cooldown, that the send bounced.
pub const MAX_DRAFT_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftAttachmentUploadRequest {
    /// The [`Draft::revision`] the operator was shown. Required for the same
    /// reason edit and send require it: the attachment array is rebuilt from
    /// the caller's view of it, so a concurrent change must 409 rather than
    /// resurrect files someone else removed.
    pub expected_revision: i64,
    pub attachments: Vec<UploadedAttachment>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadedAttachment {
    pub filename: String,
    pub content_type: String,
    /// Base64 of the file's bytes. Named to match the compose surface's
    /// `data_b64`, and stored as the draft array's `data_base64`.
    pub data_b64: String,
}

#[derive(Debug, Deserialize)]
pub struct RevisionQuery {
    pub expected_revision: i64,
}

#[derive(Debug, Deserialize)]
pub struct DownloadQuery {
    #[serde(default)]
    pub inline: bool,
}

/// Strip a client-supplied filename to a single safe path segment.
///
/// The filename is the download route's path parameter and the name written
/// into the outgoing MIME part, so it must not carry directory traversal or
/// separators. Returns `None` when nothing usable survives.
pub(crate) fn sanitize_filename(raw: &str) -> Option<String> {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
        .trim_matches('.')
        .trim();
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return None;
    }
    Some(cleaned)
}

/// Give `filename` a `name (2).ext` suffix until it no longer collides.
///
/// Downloads address an attachment by name, so two entries sharing one is not
/// a cosmetic duplicate — it makes one of them unreachable. Uniquifying on
/// attach keeps every file both visible and retrievable.
pub(crate) fn uniquify_filename(filename: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == filename) {
        return filename.to_string();
    }
    let (stem, ext) = match filename.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
        _ => (filename, None),
    };
    for n in 2..=1000 {
        let candidate = match ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        if !taken.iter().any(|t| t == &candidate) {
            return candidate;
        }
    }
    // 999 same-named files on one draft is not a real case; fall back to the
    // original rather than loop forever, and let the caller's collision check
    // stand.
    filename.to_string()
}

/// Byte length an existing draft attachment entry accounts for.
fn entry_size(entry: &serde_json::Value) -> usize {
    entry
        .get("size")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as usize
}

fn entry_filename(entry: &serde_json::Value) -> Option<&str> {
    entry.get("filename").and_then(serde_json::Value::as_str)
}

/// `POST /api/accounts/{id}/drafts/{draft_id}/attachments`
pub async fn upload(
    State(state): State<AppState>,
    Path((account_id, draft_id)): Path<(String, String)>,
    Json(req): Json<DraftAttachmentUploadRequest>,
) -> Response {
    if req.attachments.is_empty() {
        return (StatusCode::BAD_REQUEST, "no attachments supplied").into_response();
    }

    let db = state.db.lock().await;
    let draft = match ensure_draft_account(&db, &account_id, &draft_id) {
        Ok(draft) => draft,
        Err(e) => return draft_error(e),
    };

    let mut merged = draft.attachments.clone();
    let mut total: usize = merged.iter().map(entry_size).sum();

    for incoming in &req.attachments {
        let Some(filename) = sanitize_filename(&incoming.filename) else {
            return (
                StatusCode::BAD_REQUEST,
                format!("attachment name '{}' is not usable", incoming.filename),
            )
                .into_response();
        };
        let data = match B64.decode(incoming.data_b64.as_bytes()) {
            Ok(data) => data,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("attachment '{filename}' is not valid base64: {e}"),
                )
                    .into_response();
            }
        };
        if data.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                format!("attachment '{filename}' is empty"),
            )
                .into_response();
        }

        total += data.len();
        if total > MAX_DRAFT_ATTACHMENT_BYTES {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "attachments would total {total} bytes, over the {MAX_DRAFT_ATTACHMENT_BYTES} byte limit for one message"
                ),
            )
                .into_response();
        }

        let taken: Vec<String> = merged
            .iter()
            .filter_map(entry_filename)
            .map(str::to_string)
            .collect();
        let filename = uniquify_filename(&filename, &taken);
        let content_type = if incoming.content_type.trim().is_empty() {
            "application/octet-stream".to_string()
        } else {
            incoming.content_type.trim().to_string()
        };

        merged.push(json!({
            "filename": filename,
            "content_type": content_type,
            "size": data.len(),
            "data_base64": incoming.data_b64,
        }));
    }

    match db.update_draft_attachments_for_revision(&draft.id, req.expected_revision, &merged) {
        Ok(updated) => Json(json!({
            "draft": draft_json(&updated),
            "status": "attached",
        }))
        .into_response(),
        Err(e) => draft_error(e),
    }
}

/// `DELETE /api/accounts/{id}/drafts/{draft_id}/attachments/{filename}`
pub async fn remove(
    State(state): State<AppState>,
    Path((account_id, draft_id, filename)): Path<(String, String, String)>,
    Query(q): Query<RevisionQuery>,
) -> Response {
    let db = state.db.lock().await;
    let draft = match ensure_draft_account(&db, &account_id, &draft_id) {
        Ok(draft) => draft,
        Err(e) => return draft_error(e),
    };

    let remaining: Vec<serde_json::Value> = draft
        .attachments
        .iter()
        .filter(|entry| entry_filename(entry) != Some(filename.as_str()))
        .cloned()
        .collect();

    if remaining.len() == draft.attachments.len() {
        return (
            StatusCode::NOT_FOUND,
            format!("draft has no attachment named '{filename}'"),
        )
            .into_response();
    }

    match db.update_draft_attachments_for_revision(&draft.id, q.expected_revision, &remaining) {
        Ok(updated) => Json(json!({
            "draft": draft_json(&updated),
            "status": "detached",
        }))
        .into_response(),
        Err(e) => draft_error(e),
    }
}

/// `GET /api/accounts/{id}/drafts/{draft_id}/attachments/{filename}`
///
/// The only route that returns draft attachment bytes. Serves from the stored
/// base64 snapshot — no IMAP round trip, because an unsent draft's files exist
/// nowhere else.
pub async fn download(
    State(state): State<AppState>,
    Path((account_id, draft_id, filename)): Path<(String, String, String)>,
    Query(q): Query<DownloadQuery>,
) -> Response {
    let db = state.db.lock().await;
    let draft = match ensure_draft_account(&db, &account_id, &draft_id) {
        Ok(draft) => draft,
        Err(e) => return draft_error(e),
    };

    let Some(entry) = draft
        .attachments
        .iter()
        .find(|entry| entry_filename(entry) == Some(filename.as_str()))
    else {
        return (
            StatusCode::NOT_FOUND,
            format!("draft has no attachment named '{filename}'"),
        )
            .into_response();
    };

    let Some(encoded) = entry.get("data_base64").and_then(serde_json::Value::as_str) else {
        return (
            StatusCode::CONFLICT,
            format!("attachment '{filename}' has no stored bytes"),
        )
            .into_response();
    };

    let data = match B64.decode(encoded.as_bytes()) {
        Ok(data) => data,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("attachment '{filename}' failed to decode: {e}"),
            )
                .into_response();
        }
    };

    // The stored content_type is client-supplied, so it decides only how the
    // browser labels a download it was told to save. `nosniff` plus a
    // disposition that is `inline` for images alone keeps a mislabelled entry
    // from being rendered as active content on the dashboard's own origin.
    let content_type = entry
        .get("content_type")
        .and_then(serde_json::Value::as_str)
        .filter(|ct| !ct.trim().is_empty())
        .unwrap_or("application/octet-stream");

    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header("X-Content-Type-Options", "nosniff")
        .header(
            header::CONTENT_DISPOSITION,
            attachment_disposition(&filename, q.inline, content_type),
        )
        .body(Body::from(data))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use envelope_email_store::{CredentialBackend, Database, Draft};

    fn b64(bytes: &[u8]) -> String {
        B64.encode(bytes)
    }

    /// An account with one editable draft, plus the draft row itself.
    fn test_state() -> (AppState, Draft) {
        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acc1', 'Work', 'me@example.test', 'example.test',
                         'smtp.example.test', 587, 'imap.example.test', 993, 'encrypted')",
                [],
            )
            .unwrap();
        let draft = db
            .create_draft(
                "acc1",
                "them@example.test",
                Some("Test cases"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        (AppState::new(db, CredentialBackend::File), draft)
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn body_bytes(response: Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    fn upload_request(
        revision: i64,
        files: &[(&str, &str, &[u8])],
    ) -> DraftAttachmentUploadRequest {
        DraftAttachmentUploadRequest {
            expected_revision: revision,
            attachments: files
                .iter()
                .map(|(name, ct, data)| UploadedAttachment {
                    filename: (*name).to_string(),
                    content_type: (*ct).to_string(),
                    data_b64: b64(data),
                })
                .collect(),
        }
    }

    async fn do_upload(
        state: &AppState,
        draft_id: &str,
        req: DraftAttachmentUploadRequest,
    ) -> Response {
        upload(
            State(state.clone()),
            Path(("acc1".to_string(), draft_id.to_string())),
            Json(req),
        )
        .await
    }

    #[tokio::test]
    async fn upload_appends_the_file_and_bumps_the_revision() {
        let (state, draft) = test_state();

        let response = do_upload(
            &state,
            &draft.id,
            upload_request(
                draft.revision,
                &[("case-one.md", "text/markdown", b"hello")],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let body = body_json(response).await;
        let attachments = body["draft"]["attachments"].as_array().unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0]["filename"], "case-one.md");
        assert_eq!(attachments[0]["content_type"], "text/markdown");
        assert_eq!(attachments[0]["size"], 5);
        assert_eq!(
            body["draft"]["revision"].as_i64().unwrap(),
            draft.revision + 1,
            "attaching a file is an edit and must bump the revision"
        );

        // The bytes are persisted even though the response withheld them.
        let stored = state.db.lock().await.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(stored.attachments[0]["data_base64"], b64(b"hello"));
    }

    /// The whole point of the endpoint pair: the JSON never carries bytes.
    #[tokio::test]
    async fn upload_response_never_echoes_attachment_bytes() {
        let (state, draft) = test_state();
        let response = do_upload(
            &state,
            &draft.id,
            upload_request(
                draft.revision,
                &[("secret.txt", "text/plain", b"classified")],
            ),
        )
        .await;

        let body = body_json(response).await;
        let serialized = serde_json::to_string(&body).unwrap();
        assert!(
            !serialized.contains("data_base64"),
            "draft JSON must not echo attachment bytes: {serialized}"
        );
        assert!(!serialized.contains(&b64(b"classified")));
    }

    #[tokio::test]
    async fn upload_refuses_a_stale_revision() {
        let (state, draft) = test_state();
        do_upload(
            &state,
            &draft.id,
            upload_request(draft.revision, &[("first.txt", "text/plain", b"a")]),
        )
        .await;

        // Second caller still holds the pre-upload revision.
        let response = do_upload(
            &state,
            &draft.id,
            upload_request(draft.revision, &[("second.txt", "text/plain", b"b")]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);

        let stored = state.db.lock().await.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(
            stored.attachments.len(),
            1,
            "the stale write must not resurrect its view of the array"
        );
    }

    #[tokio::test]
    async fn upload_rejects_bad_payloads() {
        let (state, draft) = test_state();

        let bad_b64 = DraftAttachmentUploadRequest {
            expected_revision: draft.revision,
            attachments: vec![UploadedAttachment {
                filename: "x.txt".into(),
                content_type: "text/plain".into(),
                data_b64: "!!!not base64!!!".into(),
            }],
        };
        assert_eq!(
            do_upload(&state, &draft.id, bad_b64).await.status(),
            StatusCode::BAD_REQUEST
        );

        let empty_file = upload_request(draft.revision, &[("x.txt", "text/plain", b"")]);
        assert_eq!(
            do_upload(&state, &draft.id, empty_file).await.status(),
            StatusCode::BAD_REQUEST
        );

        let no_name = upload_request(draft.revision, &[("   ", "text/plain", b"data")]);
        assert_eq!(
            do_upload(&state, &draft.id, no_name).await.status(),
            StatusCode::BAD_REQUEST
        );

        let none = DraftAttachmentUploadRequest {
            expected_revision: draft.revision,
            attachments: vec![],
        };
        assert_eq!(
            do_upload(&state, &draft.id, none).await.status(),
            StatusCode::BAD_REQUEST
        );

        let stored = state.db.lock().await.get_draft(&draft.id).unwrap().unwrap();
        assert!(
            stored.attachments.is_empty(),
            "no rejected upload may leave a partial entry behind"
        );
        assert_eq!(
            stored.revision, draft.revision,
            "and none may bump revision"
        );
    }

    #[tokio::test]
    async fn upload_enforces_the_total_size_ceiling() {
        let (state, draft) = test_state();
        let oversized = vec![0u8; MAX_DRAFT_ATTACHMENT_BYTES + 1];
        let response = do_upload(
            &state,
            &draft.id,
            upload_request(
                draft.revision,
                &[("big.bin", "application/octet-stream", &oversized)],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

        // The ceiling is a running total across the draft, not per file.
        let half = vec![0u8; MAX_DRAFT_ATTACHMENT_BYTES / 2 + 1];
        let first = do_upload(
            &state,
            &draft.id,
            upload_request(
                draft.revision,
                &[("a.bin", "application/octet-stream", &half)],
            ),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);

        let second = do_upload(
            &state,
            &draft.id,
            upload_request(
                draft.revision + 1,
                &[("b.bin", "application/octet-stream", &half)],
            ),
        )
        .await;
        assert_eq!(second.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn upload_sanitizes_names_and_keeps_duplicates_addressable() {
        let (state, draft) = test_state();
        let response = do_upload(
            &state,
            &draft.id,
            upload_request(
                draft.revision,
                &[
                    ("../../etc/passwd", "text/plain", b"root"),
                    ("report.pdf", "application/pdf", b"one"),
                    ("report.pdf", "application/pdf", b"two"),
                ],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let body = body_json(response).await;
        let names: Vec<&str> = body["draft"]["attachments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["filename"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["passwd", "report.pdf", "report (2).pdf"]);
    }

    #[tokio::test]
    async fn upload_defaults_a_blank_content_type() {
        let (state, draft) = test_state();
        let response = do_upload(
            &state,
            &draft.id,
            upload_request(draft.revision, &[("mystery", "", b"data")]),
        )
        .await;
        let body = body_json(response).await;
        assert_eq!(
            body["draft"]["attachments"][0]["content_type"],
            "application/octet-stream"
        );
    }

    #[tokio::test]
    async fn remove_detaches_one_file_and_leaves_the_rest() {
        let (state, draft) = test_state();
        let uploaded = do_upload(
            &state,
            &draft.id,
            upload_request(
                draft.revision,
                &[
                    ("keep.txt", "text/plain", b"a"),
                    ("drop.txt", "text/plain", b"b"),
                ],
            ),
        )
        .await;
        let revision = body_json(uploaded).await["draft"]["revision"]
            .as_i64()
            .unwrap();

        let response = remove(
            State(state.clone()),
            Path(("acc1".to_string(), draft.id.clone(), "drop.txt".to_string())),
            Query(RevisionQuery {
                expected_revision: revision,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let body = body_json(response).await;
        let attachments = body["draft"]["attachments"].as_array().unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0]["filename"], "keep.txt");
        assert_eq!(body["draft"]["revision"].as_i64().unwrap(), revision + 1);
    }

    #[tokio::test]
    async fn remove_reports_an_unknown_name_and_refuses_a_stale_revision() {
        let (state, draft) = test_state();
        let uploaded = do_upload(
            &state,
            &draft.id,
            upload_request(draft.revision, &[("only.txt", "text/plain", b"a")]),
        )
        .await;
        let revision = body_json(uploaded).await["draft"]["revision"]
            .as_i64()
            .unwrap();

        let missing = remove(
            State(state.clone()),
            Path((
                "acc1".to_string(),
                draft.id.clone(),
                "ghost.txt".to_string(),
            )),
            Query(RevisionQuery {
                expected_revision: revision,
            }),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        let stale = remove(
            State(state.clone()),
            Path(("acc1".to_string(), draft.id.clone(), "only.txt".to_string())),
            Query(RevisionQuery {
                expected_revision: revision - 1,
            }),
        )
        .await;
        assert_eq!(stale.status(), StatusCode::CONFLICT);

        let stored = state.db.lock().await.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(stored.attachments.len(), 1, "neither call may detach");
    }

    #[tokio::test]
    async fn download_returns_the_stored_bytes_with_safe_headers() {
        let (state, draft) = test_state();
        do_upload(
            &state,
            &draft.id,
            upload_request(
                draft.revision,
                &[("case-one.md", "text/markdown", b"# Case one")],
            ),
        )
        .await;

        let response = download(
            State(state.clone()),
            Path((
                "acc1".to_string(),
                draft.id.clone(),
                "case-one.md".to_string(),
            )),
            Query(DownloadQuery { inline: false }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("X-Content-Type-Options").unwrap(),
            "nosniff"
        );
        assert_eq!(
            response.headers().get(header::CONTENT_DISPOSITION).unwrap(),
            "attachment; filename=\"case-one.md\""
        );
        assert_eq!(body_bytes(response).await, b"# Case one");
    }

    /// `inline` is honoured for images only — a mislabelled entry must not be
    /// rendered as active content on the dashboard's own origin.
    #[tokio::test]
    async fn download_only_inlines_images() {
        let (state, draft) = test_state();
        do_upload(
            &state,
            &draft.id,
            upload_request(
                draft.revision,
                &[
                    ("shot.png", "image/png", b"\x89PNG"),
                    ("page.html", "text/html", b"<script>"),
                ],
            ),
        )
        .await;

        let png = download(
            State(state.clone()),
            Path(("acc1".to_string(), draft.id.clone(), "shot.png".to_string())),
            Query(DownloadQuery { inline: true }),
        )
        .await;
        assert_eq!(
            png.headers().get(header::CONTENT_DISPOSITION).unwrap(),
            "inline; filename=\"shot.png\""
        );

        let html = download(
            State(state.clone()),
            Path((
                "acc1".to_string(),
                draft.id.clone(),
                "page.html".to_string(),
            )),
            Query(DownloadQuery { inline: true }),
        )
        .await;
        assert_eq!(
            html.headers().get(header::CONTENT_DISPOSITION).unwrap(),
            "attachment; filename=\"page.html\""
        );
    }

    #[tokio::test]
    async fn download_404s_for_an_unknown_name_and_a_foreign_draft() {
        let (state, draft) = test_state();
        do_upload(
            &state,
            &draft.id,
            upload_request(draft.revision, &[("real.txt", "text/plain", b"a")]),
        )
        .await;

        let unknown = download(
            State(state.clone()),
            Path((
                "acc1".to_string(),
                draft.id.clone(),
                "ghost.txt".to_string(),
            )),
            Query(DownloadQuery { inline: false }),
        )
        .await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

        // A draft that belongs to a different account is not reachable through
        // this account's path, even with the right draft id.
        let foreign = download(
            State(state.clone()),
            Path(("acc2".to_string(), draft.id.clone(), "real.txt".to_string())),
            Query(DownloadQuery { inline: false }),
        )
        .await;
        assert_eq!(foreign.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn sanitize_filename_reduces_paths_to_one_safe_segment() {
        assert_eq!(
            sanitize_filename("report.pdf").as_deref(),
            Some("report.pdf")
        );
        assert_eq!(
            sanitize_filename("../../etc/passwd").as_deref(),
            Some("passwd")
        );
        assert_eq!(
            sanitize_filename("C:\\Users\\me\\notes.txt").as_deref(),
            Some("notes.txt")
        );
        assert_eq!(
            sanitize_filename("  spaced.txt  ").as_deref(),
            Some("spaced.txt")
        );
        assert_eq!(sanitize_filename("").as_deref(), None);
        assert_eq!(sanitize_filename("   ").as_deref(), None);
        assert_eq!(sanitize_filename("..").as_deref(), None);
        assert_eq!(sanitize_filename("/").as_deref(), None);
    }

    #[test]
    fn uniquify_filename_keeps_every_attachment_addressable() {
        let taken = vec!["report.pdf".to_string()];
        assert_eq!(uniquify_filename("report.pdf", &taken), "report (2).pdf");
        assert_eq!(uniquify_filename("other.pdf", &taken), "other.pdf");

        let taken = vec!["report.pdf".to_string(), "report (2).pdf".to_string()];
        assert_eq!(uniquify_filename("report.pdf", &taken), "report (3).pdf");

        let taken = vec!["README".to_string()];
        assert_eq!(uniquify_filename("README", &taken), "README (2)");

        // A leading-dot name is all extension — suffix the whole thing.
        let taken = vec![".gitignore".to_string()];
        assert_eq!(uniquify_filename(".gitignore", &taken), ".gitignore (2)");
    }
}
