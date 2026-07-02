// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Read-only health/identity endpoint for drift detection (issue #46).
//!
//! The dashboard can run from an installed launchd binary that is a different
//! version than the CLI/repo binary an operator is actively using. When that
//! happens, the dashboard surfaces noisy mailbox errors even though the CLI can
//! authenticate. This endpoint lets `envelope doctor` (or an operator) compare
//! the *running dashboard* version/binary path against the local CLI to
//! distinguish stale-binary drift from real credential problems.
//!
//! Strictly read-only: no auth probes, no IMAP, no mutation. Consistent with
//! the dashboard aggregate read-only invariant.
//!
//! Info-disclosure guard: absolute filesystem paths (`binary_path`,
//! `database_path`, `app_data_dir`) are only returned to *authorized* callers.
//! An unauthenticated request — e.g. a liveness probe from a `tailscale serve`
//! front-end — gets a minimal `status`/`service`/`version` payload with no local
//! paths. In open loopback mode (auth disabled) the full payload is returned, so
//! local `envelope doctor` drift detection is unchanged.

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use envelope_email_store::{BuildInfo, app_data_dir, database_path};
use serde_json::json;

use crate::state::AppState;

/// `GET /api/health` — version always; binary path, credential backend, and
/// resolved state paths only for authorized callers.
pub async fn get(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let authorized = state.auth.authorize(&headers);
    Json(health_payload(state.backend, authorized)).into_response()
}

pub(crate) fn health_payload(
    backend: envelope_email_store::CredentialBackend,
    authorized: bool,
) -> serde_json::Value {
    let build = BuildInfo::current();
    let mut payload = json!({
        "status": "ok",
        "service": "envelope-dashboard",
        "version": build.version,
    });
    if authorized {
        let map = payload.as_object_mut().expect("payload is an object");
        map.insert("binary_path".into(), json!(build.binary_path));
        map.insert("credential_backend".into(), json!(backend.to_string()));
        map.insert(
            "database_path".into(),
            json!(database_path().display().to_string()),
        );
        map.insert(
            "app_data_dir".into(),
            json!(app_data_dir().display().to_string()),
        );
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use envelope_email_store::{CredentialBackend, VERSION};

    #[test]
    fn authorized_payload_reports_version_backend_and_paths() {
        let value = health_payload(CredentialBackend::File, true);
        assert_eq!(value["status"], "ok");
        assert_eq!(value["service"], "envelope-dashboard");
        assert_eq!(value["version"], VERSION);
        assert_eq!(value["credential_backend"], "file");
        assert!(value["database_path"].is_string());
    }

    #[test]
    fn unauthorized_payload_omits_filesystem_paths() {
        let value = health_payload(CredentialBackend::File, false);
        assert_eq!(value["status"], "ok");
        assert_eq!(value["version"], VERSION);
        assert!(value["binary_path"].is_null(), "must not leak binary path");
        assert!(value["database_path"].is_null(), "must not leak db path");
        assert!(
            value["app_data_dir"].is_null(),
            "must not leak app data dir"
        );
        assert!(value["credential_backend"].is_null());
    }
}
