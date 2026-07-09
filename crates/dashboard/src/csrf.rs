// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! CSRF protection for mutating dashboard `/api` routes (security HIGH-2).
//!
//! The dashboard's mutating endpoints authenticate with a bearer token or a
//! Tailscale identity header (see [`crate::auth`]). Neither defends against a
//! *cross-site request forgery*: a browser that is already authenticated to the
//! dashboard (an allowlisted tailnet identity, or plain loopback open mode)
//! carries its cookies/identity automatically, so a malicious page on another
//! origin can drive `fetch`/form POSTs against the dashboard and act as the user.
//!
//! Defense: **double-submit cookie + fetch-metadata check**, with a **bearer
//! exemption**.
//!
//! - [`issue`] (`GET /api/csrf`) mints a random 32-byte hex token, returns it in
//!   the JSON body, and sets it in a cookie the JS reads back. The cookie is
//!   `SameSite=Strict`, `Path=/`, and readable by JS (`HttpOnly` unset) so the
//!   frontend can echo it in the `X-Envelope-CSRF` header.
//! - [`require_csrf`] runs on mutating methods (POST/PUT/DELETE/PATCH) *after*
//!   [`crate::auth::require_auth`]. A request that authenticated via a bearer
//!   token is exempt — a browser cannot attach the `Authorization`/`X-Envelope-
//!   Token` header cross-site, so a valid bearer already proves same-origin
//!   intent. Otherwise the request must present `X-Envelope-CSRF` matching the
//!   cookie (constant-time), and its `Origin`/`Sec-Fetch-Site` must be
//!   consistent with same-origin.
//!
//! ### Cookie name and `Secure`
//! The `__Host-` cookie prefix is the strongest binding (host-only, `Path=/`,
//! `Secure`) but the browser *rejects* a `__Host-` cookie that lacks `Secure`,
//! and a plain-HTTP loopback dashboard cannot set `Secure` cookies. So we pick
//! the name from the request's effective scheme:
//! - HTTPS (e.g. behind `tailscale serve`, or `X-Forwarded-Proto: https`):
//!   [`COOKIE_SECURE`] = `__Host-envelope_csrf`, with `Secure`.
//! - plain-HTTP loopback: [`COOKIE_PLAIN`] = `envelope_csrf`, no `Secure`.
//!
//! Both names are checked when validating, so a token minted under one scheme is
//! not silently invalid if the client later reports the other.

use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rand::RngCore;
use serde_json::json;

use crate::auth::{BearerAuthenticated, constant_time_eq};

/// Cookie name used when the connection is `Secure` (HTTPS). The `__Host-`
/// prefix pins the cookie to the exact host with `Path=/` and requires `Secure`.
pub const COOKIE_SECURE: &str = "__Host-envelope_csrf";
/// Cookie name used on plain-HTTP loopback, where `Secure` (and therefore the
/// `__Host-` prefix) cannot be used.
pub const COOKIE_PLAIN: &str = "envelope_csrf";

/// Request header carrying the echoed CSRF token.
const CSRF_HEADER: &str = "x-envelope-csrf";

/// `GET /api/csrf` — mint a fresh CSRF token, set it as a cookie, and return it
/// in the body so the frontend can echo it in the [`CSRF_HEADER`].
pub async fn issue(headers: HeaderMap) -> Response {
    let token = generate_token();
    let secure = request_is_secure(&headers);
    let cookie = if secure {
        format!("{COOKIE_SECURE}={token}; Path=/; SameSite=Strict; Secure")
    } else {
        format!("{COOKIE_PLAIN}={token}; Path=/; SameSite=Strict")
    };

    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        axum::Json(json!({ "token": token })),
    )
        .into_response()
}

/// Axum middleware enforcing CSRF on mutating methods, layered *after*
/// [`crate::auth::require_auth`]. Non-mutating methods and bearer-authenticated
/// requests pass through untouched.
pub async fn require_csrf(request: Request, next: Next) -> Response {
    if !is_mutating(request.method()) {
        return next.run(request).await;
    }

    // Bearer exemption: a browser cannot attach the Authorization/X-Envelope-
    // Token header cross-site, so a request that authenticated via bearer token
    // is inherently same-origin-intended. `require_auth` records this fact.
    if request.extensions().get::<BearerAuthenticated>().is_some() {
        return next.run(request).await;
    }

    if !validate(request.headers()) {
        return reject();
    }

    next.run(request).await
}

/// True when the double-submit token and fetch-metadata pass for a mutating
/// request; false means reject.
fn validate(headers: &HeaderMap) -> bool {
    // 1. Origin / Sec-Fetch-Site consistency. If a browser sent Origin, it must
    //    match Host; if it sent Sec-Fetch-Site, it must be same-origin/same-site
    //    or an explicit navigation (`none`). A forged cross-site request either
    //    reveals a foreign Origin or a `cross-site` fetch metadata.
    if !origin_is_consistent(headers) {
        return false;
    }

    // 2. Double-submit: header must equal the cookie value, constant-time.
    let header_token = header_str(headers, CSRF_HEADER)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let cookie_token = csrf_cookie(headers);

    match (header_token, cookie_token) {
        (Some(h), Some(c)) => constant_time_eq(h.as_bytes(), c.as_bytes()),
        _ => false,
    }
}

/// True when the request's `Origin`/`Sec-Fetch-Site` are consistent with a
/// same-origin request. Absent headers are permissive (non-browser clients).
fn origin_is_consistent(headers: &HeaderMap) -> bool {
    if let Some(fetch_site) = header_str(headers, "sec-fetch-site") {
        // `same-origin`/`same-site` are safe; `none` is a user-initiated
        // navigation (no cross-site initiator). `cross-site` is the attack.
        match fetch_site.trim() {
            "same-origin" | "same-site" | "none" => {}
            _ => return false,
        }
    }

    if let Some(origin) = header_str(headers, header::ORIGIN.as_str()) {
        // Compare the Origin's host:port to the Host header. A cross-site page's
        // Origin will not match the dashboard's Host.
        let origin_host = origin
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(origin);
        match header_str(headers, header::HOST.as_str()) {
            Some(host) if origin_host.eq_ignore_ascii_case(host.trim()) => {}
            _ => return false,
        }
    }

    true
}

/// Extract the CSRF cookie value under either the secure or plain name.
fn csrf_cookie(headers: &HeaderMap) -> Option<String> {
    let raw = header_str(headers, header::COOKIE.as_str())?;
    for pair in raw.split(';') {
        let pair = pair.trim();
        if let Some(val) = pair
            .strip_prefix(COOKIE_SECURE)
            .and_then(|r| r.strip_prefix('='))
            .or_else(|| {
                pair.strip_prefix(COOKIE_PLAIN)
                    .and_then(|r| r.strip_prefix('='))
            })
        {
            let val = val.trim();
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// Whether the request reached the server over HTTPS. Behind `tailscale serve`
/// the loopback listener sees plain HTTP but the proxy sets
/// `X-Forwarded-Proto: https`; honor that so tailnet clients get a `Secure`,
/// `__Host-`-prefixed cookie.
fn request_is_secure(headers: &HeaderMap) -> bool {
    header_str(headers, "x-forwarded-proto")
        .map(|p| {
            p.split(',')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("https")
        })
        .unwrap_or(false)
}

fn is_mutating(method: &axum::http::Method) -> bool {
    matches!(
        *method,
        axum::http::Method::POST
            | axum::http::Method::PUT
            | axum::http::Method::DELETE
            | axum::http::Method::PATCH
    )
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// 32 random bytes, lowercase-hex encoded (64 chars).
fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn reject() -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(json!({
            "error": "csrf",
            "code": "dashboard_csrf_required",
        })),
    )
        .into_response()
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
    fn generated_tokens_are_64_hex_chars_and_unique() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "tokens must not repeat");
    }

    #[test]
    fn cookie_parsing_handles_both_names_and_other_cookies() {
        let h = headers(&[("cookie", "foo=bar; envelope_csrf=abc123; baz=qux")]);
        assert_eq!(csrf_cookie(&h).as_deref(), Some("abc123"));

        let h = headers(&[("cookie", "__Host-envelope_csrf=deadbeef")]);
        assert_eq!(csrf_cookie(&h).as_deref(), Some("deadbeef"));

        assert!(csrf_cookie(&headers(&[("cookie", "unrelated=1")])).is_none());
    }

    #[test]
    fn secure_scheme_detection_uses_forwarded_proto() {
        assert!(request_is_secure(&headers(&[(
            "x-forwarded-proto",
            "https"
        )])));
        assert!(request_is_secure(&headers(&[(
            "x-forwarded-proto",
            "https, http"
        )])));
        assert!(!request_is_secure(&headers(&[(
            "x-forwarded-proto",
            "http"
        )])));
        assert!(!request_is_secure(&HeaderMap::new()));
    }

    #[test]
    fn matching_header_and_cookie_passes_validation() {
        let h = headers(&[("cookie", "envelope_csrf=tok"), ("x-envelope-csrf", "tok")]);
        assert!(validate(&h));
    }

    #[test]
    fn mismatched_header_and_cookie_is_rejected() {
        let h = headers(&[("cookie", "envelope_csrf=tok"), ("x-envelope-csrf", "nope")]);
        assert!(!validate(&h));
    }

    #[test]
    fn missing_header_or_cookie_is_rejected() {
        assert!(!validate(&headers(&[("cookie", "envelope_csrf=tok")])));
        assert!(!validate(&headers(&[("x-envelope-csrf", "tok")])));
    }

    #[test]
    fn cross_site_origin_is_rejected_even_with_matching_token() {
        let h = headers(&[
            ("cookie", "envelope_csrf=tok"),
            ("x-envelope-csrf", "tok"),
            ("origin", "https://evil.example"),
            ("host", "localhost:3141"),
        ]);
        assert!(!validate(&h), "foreign Origin must fail");
    }

    #[test]
    fn same_origin_origin_passes() {
        let h = headers(&[
            ("cookie", "envelope_csrf=tok"),
            ("x-envelope-csrf", "tok"),
            ("origin", "http://localhost:3141"),
            ("host", "localhost:3141"),
        ]);
        assert!(validate(&h));
    }

    #[test]
    fn cross_site_fetch_metadata_is_rejected() {
        let h = headers(&[
            ("cookie", "envelope_csrf=tok"),
            ("x-envelope-csrf", "tok"),
            ("sec-fetch-site", "cross-site"),
        ]);
        assert!(!validate(&h));
    }

    #[test]
    fn same_origin_fetch_metadata_passes() {
        for site in ["same-origin", "same-site", "none"] {
            let h = headers(&[
                ("cookie", "envelope_csrf=tok"),
                ("x-envelope-csrf", "tok"),
                ("sec-fetch-site", site),
            ]);
            assert!(validate(&h), "site={site} should pass");
        }
    }
}
