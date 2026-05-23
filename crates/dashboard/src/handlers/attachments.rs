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

fn attachment_disposition(filename: &str, inline: bool) -> String {
    let disposition = if inline { "inline" } else { "attachment" };
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
                .header(header::CONTENT_TYPE, content_type)
                .header(
                    header::CONTENT_DISPOSITION,
                    attachment_disposition(&fname, q.inline),
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
    fn content_disposition_defaults_to_download_and_supports_inline_rendering() {
        assert_eq!(
            attachment_disposition("report.pdf", false),
            "attachment; filename=\"report.pdf\""
        );
        assert_eq!(
            attachment_disposition("logo.png", true),
            "inline; filename=\"logo.png\""
        );
        assert_eq!(
            attachment_disposition("bad\"name.png", true),
            "inline; filename=\"bad_name.png\""
        );
    }
}
