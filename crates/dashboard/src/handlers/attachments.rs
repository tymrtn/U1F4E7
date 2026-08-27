// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Attachment download handler.

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::state::AppState;

#[derive(Deserialize)]
pub struct DownloadQuery {
    #[serde(default = "default_folder")]
    pub folder: String,
    #[serde(default)]
    pub inline: bool,
}

fn default_folder() -> String {
    "INBOX".to_string()
}

pub(crate) fn is_image_media_type(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .starts_with("image/")
}

pub(crate) fn attachment_disposition(filename: &str, inline: bool, content_type: &str) -> String {
    let disposition = if inline && is_image_media_type(content_type) {
        "inline"
    } else {
        "attachment"
    };
    format!("{disposition}; filename=\"{}\"", filename.replace('"', "_"))
}

pub async fn download(
    State(state): State<AppState>,
    Path((account_id, uid, filename)): Path<(String, u32, String)>,
    Query(q): Query<DownloadQuery>,
) -> Response {
    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response();
        }
    };
    let mut client = client_arc.lock().await;

    match envelope_email_transport::imap::download_attachment(
        &mut client,
        uid,
        &filename,
        &q.folder,
    )
    .await
    {
        Ok((fname, data)) => {
            let content_type = mime_guess::from_path(&fname)
                .first_or_octet_stream()
                .to_string();
            Response::builder()
                .header(header::CONTENT_TYPE, content_type.clone())
                .header("X-Content-Type-Options", "nosniff")
                .header(
                    header::CONTENT_DISPOSITION,
                    attachment_disposition(&fname, q.inline, &content_type),
                )
                .body(Body::from(data))
                .unwrap()
        }
        Err(e) => {
            state.evict_imap(&account_id).await;
            (StatusCode::BAD_GATEWAY, format!("download: {e}")).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_defaults_to_download_and_only_supports_inline_images() {
        assert_eq!(
            attachment_disposition("report.pdf", false, "application/pdf"),
            "attachment; filename=\"report.pdf\""
        );
        assert_eq!(
            attachment_disposition("report.pdf", true, "application/pdf"),
            "attachment; filename=\"report.pdf\""
        );
        assert_eq!(
            attachment_disposition("logo.png", true, "image/png"),
            "inline; filename=\"logo.png\""
        );
        assert_eq!(
            attachment_disposition("bad\"name.png", true, "image/png; charset=binary"),
            "inline; filename=\"bad_name.png\""
        );
    }
}
