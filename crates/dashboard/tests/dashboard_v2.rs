// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2
//
// Integration tests for the Envelope v2 webmail mount (`/v2`). These build the
// real dashboard router via `dashboard_router` and assert the committed
// `web/build/` SPA bundle is served correctly: the shell at `/v2`, SPA fallback
// for client-side routes, a hashed asset with the right content type, and the
// invariant that the bundle carries no `cdn.tailwindcss` reference.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use envelope_email_dashboard::dashboard_router;
use envelope_email_dashboard::state::AppState;
use envelope_email_store::{CredentialBackend, Database};
use tower::ServiceExt;

/// The committed v2 SPA shell — compiled into the binary and asserted here so a
/// stale/dirty `web/build/` shows up as a test failure, not a silent 500.
const V2_INDEX: &str = include_str!("../web/build/index.html");

fn state() -> AppState {
    let db = Database::open_memory().unwrap();
    AppState::new(db, CredentialBackend::File)
}

async fn get(uri: &str) -> (StatusCode, Vec<u8>, Option<String>) {
    let app = dashboard_router(state());
    let response = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, body, content_type)
}

#[test]
fn committed_v2_bundle_has_no_tailwind_cdn_reference() {
    // The whole point of a proper Tailwind build: never ship the play-CDN.
    assert!(
        !V2_INDEX.contains("cdn.tailwindcss"),
        "v2 index.html must not reference the Tailwind CDN"
    );
}

#[tokio::test]
async fn v2_root_serves_the_spa_index() {
    let (status, body, content_type) = get("/v2").await;
    assert_eq!(status, StatusCode::OK, "/v2 should serve the SPA shell");
    let html = String::from_utf8(body).unwrap();
    assert!(html.contains("<title>Envelope</title>"));
    assert!(
        html.contains("__sveltekit"),
        "shell should carry the SvelteKit bootstrap"
    );
    assert!(
        html.contains(r#"base: "/v2""#),
        "shell should carry the /v2 base path"
    );
    assert!(
        !html.contains("cdn.tailwindcss"),
        "served shell must not reference the Tailwind CDN"
    );
    let ct = content_type.unwrap_or_default();
    assert!(ct.starts_with("text/html"), "unexpected content-type: {ct}");
}

#[tokio::test]
async fn v2_unknown_client_route_falls_back_to_index() {
    // `/v2/kitchen-sink` is a client-side route with no matching embedded file;
    // the SPA fallback must return the shell so the router can resolve it.
    let (status, body, _) = get("/v2/kitchen-sink").await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(body).unwrap();
    assert!(
        html.contains("<title>Envelope</title>") && html.contains("__sveltekit"),
        "unknown /v2 route should fall back to the SPA shell"
    );
}

#[tokio::test]
async fn v2_hashed_asset_serves_with_correct_content_type() {
    // Discover a real hashed CSS asset path from the served shell so this test
    // is robust to content-hash changes across rebuilds.
    let (_, index_body, _) = get("/v2").await;
    let html = String::from_utf8(index_body).unwrap();
    let css_path = html
        .split('"')
        .find(|token| token.starts_with("/v2/_app/") && token.ends_with(".css"))
        .expect("built shell should reference at least one hashed CSS asset");

    let (status, body, content_type) = get(css_path).await;
    assert_eq!(status, StatusCode::OK, "hashed asset {css_path} should 200");
    assert!(!body.is_empty(), "hashed asset should have bytes");
    let ct = content_type.unwrap_or_default();
    assert!(
        ct.starts_with("text/css"),
        "CSS asset should serve as text/css, got: {ct}"
    );
}
