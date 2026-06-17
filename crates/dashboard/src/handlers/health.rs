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

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use envelope_email_store::{BuildInfo, app_data_dir, database_path};
use serde_json::json;

use crate::state::AppState;

/// `GET /api/health` — version, binary path, credential backend, and resolved
/// state paths for the running dashboard process.
pub async fn get(State(state): State<AppState>) -> impl IntoResponse {
    Json(health_payload(state.backend)).into_response()
}

pub(crate) fn health_payload(
    backend: envelope_email_store::CredentialBackend,
) -> serde_json::Value {
    let build = BuildInfo::current();
    json!({
        "status": "ok",
        "service": "envelope-dashboard",
        "version": build.version,
        "binary_path": build.binary_path,
        "credential_backend": backend.to_string(),
        "database_path": database_path().display().to_string(),
        "app_data_dir": app_data_dir().display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use envelope_email_store::{CredentialBackend, VERSION};

    #[test]
    fn payload_reports_version_and_backend() {
        let value = health_payload(CredentialBackend::File);
        assert_eq!(value["status"], "ok");
        assert_eq!(value["service"], "envelope-dashboard");
        assert_eq!(value["version"], VERSION);
        assert_eq!(value["credential_backend"], "file");
        assert!(value["database_path"].is_string());
    }
}
