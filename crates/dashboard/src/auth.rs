// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Dashboard authentication for exposure beyond loopback.
//!
//! The dashboard REST API mutates real mailboxes (read/delete/move mail, add or
//! delete accounts, queue and send drafts). On `127.0.0.1` that is a
//! single-user local trust boundary and needs no auth. The moment the API is
//! reachable by another host — a non-loopback bind, or (more commonly) a
//! `tailscale serve` front-end proxying loopback onto the tailnet — every one of
//! those routes must require a credential, or any tailnet device can read and
//! send mail as any configured account.
//!
//! Two credential methods, either sufficient (`authorize` returns true if
//! *either* passes):
//!
//! 1. **Bearer token** (`Authorization: Bearer <token>` or `X-Envelope-Token:
//!    <token>`). The unspoofable primitive. Compared in constant time. This is
//!    the agent path (Hermes/OpenClaw/scripted clients).
//! 2. **Tailscale identity allowlist** (`Tailscale-User-Login` ∈ allowlist).
//!    `tailscale serve` injects this header on proxied requests; the tailnet
//!    identity *is* the credential, so a human just opens the `.ts.net` URL — no
//!    token to type. **Only safe behind `tailscale serve`**, which sets/strips
//!    the header; do not enable the allowlist if untrusted local processes can
//!    reach the port and forge the header.
//!
//! Enforcement keys off *configuration*, not bind address: if either method is
//! configured, auth is enforced on every `/api` route regardless of bind. That
//! is deliberate — it protects the common `tailscale serve` → loopback case,
//! where the listener still sees a loopback peer.

use std::collections::BTreeSet;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::state::AppState;

/// Env var holding the dashboard bearer token.
pub const ENV_DASHBOARD_TOKEN: &str = "ENVELOPE_DASHBOARD_TOKEN";
/// Env var holding a comma-separated Tailscale identity allowlist.
pub const ENV_DASHBOARD_TAILSCALE_ALLOW: &str = "ENVELOPE_DASHBOARD_TAILSCALE_ALLOW";

/// Header `tailscale serve` injects with the authenticated tailnet login.
const TAILSCALE_USER_LOGIN: &str = "tailscale-user-login";
/// Convenience header for agents behind proxies that strip `Authorization`.
const X_ENVELOPE_TOKEN: &str = "x-envelope-token";

/// Resolved dashboard authentication policy.
///
/// `Debug` is implemented manually to redact the token: a future
/// `debug!("{auth:?}")` or `#[instrument]` must never dump the secret.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct AuthConfig {
    token: Option<String>,
    tailscale_allow: BTreeSet<String>,
}

impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field(
                "token",
                &self
                    .token
                    .as_ref()
                    .map(|_| "<redacted>")
                    .unwrap_or("<unset>"),
            )
            .field("tailscale_allow", &self.tailscale_allow)
            .finish()
    }
}

impl AuthConfig {
    /// Auth disabled — local loopback trust boundary only.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// True when the only configured method is the Tailscale identity allowlist
    /// (no bearer token). The identity header is forgeable by anything that can
    /// reach the bound port, so this is only safe behind `tailscale serve`.
    pub fn is_identity_only(&self) -> bool {
        self.token.is_none() && !self.tailscale_allow.is_empty()
    }

    /// True when authorization can succeed from a Tailscale identity header.
    /// Such headers are only trustworthy when `tailscale serve` injects them
    /// onto a loopback listener.
    pub fn has_tailscale_identity_allowlist(&self) -> bool {
        !self.tailscale_allow.is_empty()
    }

    /// Build from explicit parts. Empty/whitespace token is treated as unset;
    /// allowlist entries are trimmed and lowercased for case-insensitive match.
    pub fn from_parts(
        token: Option<String>,
        tailscale_allow: impl IntoIterator<Item = String>,
    ) -> Self {
        let token = token
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        let tailscale_allow = tailscale_allow
            .into_iter()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        Self {
            token,
            tailscale_allow,
        }
    }

    /// Resolve from environment only (used by the dashboard library itself; the
    /// CLI merges these with config-file values, env taking precedence).
    pub fn from_env() -> Self {
        let token = std::env::var(ENV_DASHBOARD_TOKEN).ok();
        let allow = std::env::var(ENV_DASHBOARD_TAILSCALE_ALLOW)
            .ok()
            .map(|raw| split_allowlist(&raw))
            .unwrap_or_default();
        Self::from_parts(token, allow)
    }

    /// True when at least one credential method is configured. When false, the
    /// dashboard runs in open loopback mode.
    pub fn is_enforced(&self) -> bool {
        self.token.is_some() || !self.tailscale_allow.is_empty()
    }

    /// Human-readable mode for startup logs and `--json` (never leaks the token).
    pub fn mode_label(&self) -> &'static str {
        match (self.token.is_some(), self.tailscale_allow.is_empty()) {
            (true, false) => "token+tailscale-identity",
            (true, true) => "token",
            (false, false) => "tailscale-identity",
            (false, true) => "open-loopback",
        }
    }

    /// Decide whether a request is authorized based on its headers. Returns true
    /// immediately when auth is not enforced (open loopback mode). Either the
    /// bearer token or an allowlisted Tailscale identity suffices.
    pub fn authorize(&self, headers: &HeaderMap) -> bool {
        if !self.is_enforced() {
            return true;
        }

        self.bearer_authorized(headers) || self.identity_authorized(headers)
    }

    /// True when a *bearer token* (Authorization or X-Envelope-Token) matched.
    ///
    /// Used by CSRF enforcement: a browser cannot attach these headers
    /// cross-site, so a valid bearer proves same-origin intent and exempts the
    /// request from the double-submit check. Returns false in open mode — an
    /// unenforced request never "authorized via bearer".
    pub fn bearer_authorized(&self, headers: &HeaderMap) -> bool {
        match (&self.token, presented_token(headers)) {
            (Some(expected), Some(presented)) => {
                constant_time_eq(expected.as_bytes(), presented.as_bytes())
            }
            _ => false,
        }
    }

    /// True when `presented` matches the configured bearer token in constant
    /// time. Used by the SSE handler for the `?access_token=` query path, which
    /// exists because the browser `EventSource` API cannot set an `Authorization`
    /// header. When no token is configured (open-loopback or identity-only) a
    /// query token authorizes nothing — the SSE handler only consults this after
    /// `authorize(headers)` has already failed, so this returns false there and
    /// the request is rejected.
    pub fn query_token_authorized(&self, presented: &str) -> bool {
        match &self.token {
            Some(expected) => constant_time_eq(expected.as_bytes(), presented.as_bytes()),
            None => false,
        }
    }

    fn identity_authorized(&self, headers: &HeaderMap) -> bool {
        !self.tailscale_allow.is_empty()
            && header_str(headers, TAILSCALE_USER_LOGIN)
                .map(|login| {
                    self.tailscale_allow
                        .contains(&login.trim().to_ascii_lowercase())
                })
                .unwrap_or(false)
    }
}

/// Split a comma/whitespace/newline-separated allowlist into entries.
pub fn split_allowlist(raw: &str) -> Vec<String> {
    raw.split([',', '\n', ' ', '\t'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Extract the presented bearer token from `Authorization: Bearer <t>` or the
/// `X-Envelope-Token` fallback header.
fn presented_token(headers: &HeaderMap) -> Option<String> {
    let bearer = header_str(headers, header::AUTHORIZATION.as_str())
        .and_then(|auth| {
            auth.strip_prefix("Bearer ")
                .or_else(|| auth.strip_prefix("bearer "))
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    bearer.or_else(|| {
        header_str(headers, X_ENVELOPE_TOKEN)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

/// Length-safe constant-time byte comparison. Folds the length difference into
/// the accumulator and scans the longer input so timing does not reveal how many
/// leading bytes matched.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u32;
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= (x ^ y) as u32;
    }
    diff == 0
}

/// Request extension marker inserted by [`require_auth`] when the request was
/// authorized by a valid *bearer token* (as opposed to Tailscale identity or
/// open loopback mode). CSRF enforcement reads it to exempt bearer clients,
/// which cannot be driven cross-site by a browser.
#[derive(Clone, Copy, Debug)]
pub struct BearerAuthenticated;

/// Axum middleware enforcing [`AuthConfig`] on protected `/api` routes.
///
/// Returns `401` with a stable JSON body + `WWW-Authenticate: Bearer` when the
/// request is unauthorized. Passes through unchanged in open loopback mode. When
/// a valid bearer token authorized the request, inserts [`BearerAuthenticated`]
/// into the request extensions so the downstream CSRF layer can exempt it.
pub async fn require_auth(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let header_ok = state.auth.authorize(request.headers());

    // `EventSource` cannot set an `Authorization` header, so the SSE endpoint
    // also accepts the token via `?access_token=`. Honor it here as an equivalent
    // credential when the header path did not already authorize. It is checked
    // with the same constant-time comparison as the header token.
    //
    // A query-token match deliberately does NOT set `BearerAuthenticated`: a
    // query string can be carried by a browser cross-site (navigations, `<img>`,
    // etc.), so unlike a real `Authorization` header it does not prove same-origin
    // intent and must stay subject to CSRF on mutating methods. For the GET-only
    // SSE stream that distinction is moot, but keeping it correct here prevents a
    // query token from becoming a CSRF bypass on other routes.
    let query_ok = !header_ok
        && query_access_token(&request)
            .map(|t| state.auth.query_token_authorized(&t))
            .unwrap_or(false);

    if !header_ok && !query_ok {
        return unauthorized().into_response();
    }
    if state.auth.bearer_authorized(request.headers()) {
        request.extensions_mut().insert(BearerAuthenticated);
    }
    next.run(request).await
}

/// Extract the `access_token` query parameter from the request URI, if present.
fn query_access_token(request: &Request) -> Option<String> {
    let query = request.uri().query()?;
    for pair in query.split('&') {
        if let Some(val) = pair.strip_prefix("access_token=") {
            // Percent-decoding is unnecessary for our tokens (hex/base64url), and
            // a decoding mismatch would only ever fail closed against the
            // constant-time compare. Return the raw value.
            return Some(val.to_string());
        }
    }
    None
}

fn unauthorized() -> impl IntoResponse {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        axum::Json(json!({
            "error": "unauthorized",
            "code": "dashboard_auth_required",
            "detail": "This Envelope dashboard requires authentication. Send Authorization: Bearer <token>, or reach it through `tailscale serve` with an allowlisted identity.",
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn disabled_config_is_not_enforced_and_authorizes_everything() {
        let cfg = AuthConfig::disabled();
        assert!(!cfg.is_enforced());
        assert!(cfg.authorize(&HeaderMap::new()));
        assert_eq!(cfg.mode_label(), "open-loopback");
    }

    #[test]
    fn empty_and_whitespace_token_is_treated_as_unset() {
        assert!(!AuthConfig::from_parts(Some("   ".into()), []).is_enforced());
        assert!(!AuthConfig::from_parts(Some(String::new()), []).is_enforced());
    }

    #[test]
    fn token_required_when_configured() {
        let cfg = AuthConfig::from_parts(Some("s3cr3t".into()), []);
        assert!(cfg.is_enforced());
        assert_eq!(cfg.mode_label(), "token");
        assert!(!cfg.authorize(&HeaderMap::new()), "no header → denied");
        assert!(!cfg.authorize(&headers(&[("authorization", "Bearer wrong")])));
        assert!(cfg.authorize(&headers(&[("authorization", "Bearer s3cr3t")])));
        assert!(
            cfg.authorize(&headers(&[("authorization", "bearer s3cr3t")])),
            "scheme is case-insensitive"
        );
        assert!(
            cfg.authorize(&headers(&[("x-envelope-token", "s3cr3t")])),
            "fallback header works"
        );
    }

    #[test]
    fn wrong_length_token_is_rejected() {
        let cfg = AuthConfig::from_parts(Some("abcdef".into()), []);
        assert!(!cfg.authorize(&headers(&[("authorization", "Bearer abc")])));
        assert!(!cfg.authorize(&headers(&[("authorization", "Bearer abcdefghij")])));
    }

    #[test]
    fn tailscale_identity_allowlist_authorizes_matching_login() {
        let cfg = AuthConfig::from_parts(None, ["Skippy@tailnet.ts.net".to_string()]);
        assert!(cfg.is_enforced());
        assert_eq!(cfg.mode_label(), "tailscale-identity");
        assert!(!cfg.authorize(&HeaderMap::new()));
        assert!(
            cfg.authorize(&headers(&[(
                "tailscale-user-login",
                "skippy@tailnet.ts.net"
            )])),
            "case-insensitive identity match"
        );
        assert!(!cfg.authorize(&headers(&[(
            "tailscale-user-login",
            "intruder@tailnet.ts.net"
        )])));
    }

    #[test]
    fn either_method_suffices_when_both_configured() {
        let cfg = AuthConfig::from_parts(Some("tok".into()), ["a@b.ts.net".to_string()]);
        assert_eq!(cfg.mode_label(), "token+tailscale-identity");
        assert!(cfg.authorize(&headers(&[("authorization", "Bearer tok")])));
        assert!(cfg.authorize(&headers(&[("tailscale-user-login", "a@b.ts.net")])));
        assert!(!cfg.authorize(&headers(&[("tailscale-user-login", "c@d.ts.net")])));
    }

    #[test]
    fn debug_never_prints_the_token() {
        let cfg = AuthConfig::from_parts(Some("supersecret".into()), ["a@b.ts.net".to_string()]);
        let rendered = format!("{cfg:?}");
        assert!(
            !rendered.contains("supersecret"),
            "Debug leaked the token: {rendered}"
        );
        assert!(rendered.contains("<redacted>"));
        // Non-secret allowlist may still appear.
        assert!(rendered.contains("a@b.ts.net"));

        let empty = format!("{:?}", AuthConfig::disabled());
        assert!(empty.contains("<unset>"));
    }

    #[test]
    fn is_identity_only_semantics() {
        assert!(AuthConfig::from_parts(None, ["a@b".to_string()]).is_identity_only());
        assert!(!AuthConfig::from_parts(Some("t".into()), ["a@b".to_string()]).is_identity_only());
        assert!(!AuthConfig::from_parts(Some("t".into()), []).is_identity_only());
        assert!(!AuthConfig::disabled().is_identity_only());
        assert!(
            AuthConfig::from_parts(None, ["a@b".to_string()]).has_tailscale_identity_allowlist()
        );
        assert!(
            AuthConfig::from_parts(Some("t".into()), ["a@b".to_string()])
                .has_tailscale_identity_allowlist()
        );
        assert!(!AuthConfig::from_parts(Some("t".into()), []).has_tailscale_identity_allowlist());
    }

    #[test]
    fn query_access_token_parses_the_param_among_others() {
        let req = |uri: &str| {
            Request::builder()
                .uri(uri)
                .body(axum::body::Body::empty())
                .unwrap()
        };
        assert_eq!(
            query_access_token(&req("/api/events/stream?access_token=abc123")).as_deref(),
            Some("abc123")
        );
        assert_eq!(
            query_access_token(&req("/api/events/stream?foo=1&access_token=xyz&bar=2")).as_deref(),
            Some("xyz")
        );
        assert!(query_access_token(&req("/api/events/stream")).is_none());
        assert!(query_access_token(&req("/api/events/stream?foo=1")).is_none());
    }

    #[test]
    fn query_token_authorized_uses_constant_time_and_needs_configured_token() {
        let cfg = AuthConfig::from_parts(Some("t0ken".into()), []);
        assert!(cfg.query_token_authorized("t0ken"));
        assert!(!cfg.query_token_authorized("nope"));
        assert!(!cfg.query_token_authorized(""));
        // No token configured: a query token authorizes nothing.
        assert!(!AuthConfig::disabled().query_token_authorized("t0ken"));
        assert!(
            !AuthConfig::from_parts(None, ["a@b.ts.net".to_string()])
                .query_token_authorized("t0ken")
        );
    }

    #[test]
    fn constant_time_eq_matches_semantics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn split_allowlist_handles_commas_and_whitespace() {
        assert_eq!(
            split_allowlist("a@x, b@y\nc@z\t d@w"),
            vec!["a@x", "b@y", "c@z", "d@w"]
        );
        assert!(split_allowlist("   ").is_empty());
    }
}
