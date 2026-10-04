// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Microsoft sign-in against a local mock of the Entra token endpoint.
//! Nothing here reaches Microsoft.

use std::time::Duration;

use envelope_email_transport::oauth::{
    LoopbackListener, MICROSOFT_LOOPBACK_PATH, OAuthClient, OAuthError, Pkce, ProviderConfig,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Serves one canned response on a loopback port and hands back the raw
/// request it received (request line, headers and body).
async fn mock_token_endpoint(
    status: u16,
    body: &'static str,
) -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = format!(
        "http://127.0.0.1:{}/common",
        listener.local_addr().unwrap().port()
    );
    let handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_http_request(&mut stream).await;
        let response = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        request
    });
    (authority, handle)
}

async fn read_http_request(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).to_string();
        if let Some(split) = text.find("\r\n\r\n") {
            let content_length = text[..split]
                .lines()
                .find_map(|l| {
                    let (name, value) = l.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if buf.len() >= split + 4 + content_length {
                return text;
            }
        }
    }
    String::from_utf8_lossy(&buf).to_string()
}

fn oauth(authority: &str) -> OAuthClient {
    OAuthClient::new(ProviderConfig::microsoft("test-client-id", authority)).unwrap()
}

fn google(base: &str) -> OAuthClient {
    let mut config = ProviderConfig::google("gcid.apps.googleusercontent.com", "gsecret");
    config.token_endpoint = format!("{base}/token");
    config.revoke_endpoint = Some(format!("{base}/revoke"));
    OAuthClient::new(config).unwrap()
}

const TOKEN_OK: &str = r#"{"token_type":"Bearer","scope":"Mail.ReadWrite Mail.Send User.Read","expires_in":3600,"access_token":"at-secret-1","refresh_token":"rt-secret-2"}"#;

#[tokio::test]
async fn exchange_code_posts_pkce_form_and_parses_tokens() {
    let (authority, server) = mock_token_endpoint(200, TOKEN_OK).await;
    let tokens = oauth(&authority)
        .exchange_code(
            &reqwest::Client::new(),
            "the-code",
            "http://localhost:5555/oauth/microsoft/callback",
            "the-verifier",
        )
        .await
        .unwrap();

    let request = server.await.unwrap();
    assert!(
        request.starts_with("POST /common/oauth2/v2.0/token "),
        "{request}"
    );
    for field in [
        "grant_type=authorization_code",
        "code=the-code",
        "code_verifier=the-verifier",
        "client_id=test-client-id",
        "redirect_uri=http%3A%2F%2Flocalhost%3A5555%2Foauth%2Fmicrosoft%2Fcallback",
        "offline_access",
    ] {
        assert!(request.contains(field), "missing {field} in {request}");
    }
    assert!(
        !request.contains("client_secret"),
        "public client never sends a secret"
    );

    assert_eq!(tokens.access_token, "at-secret-1");
    assert_eq!(tokens.refresh_token.as_deref(), Some("rt-secret-2"));
    let remaining = tokens.expires_at - chrono::Utc::now();
    assert!(
        remaining > chrono::Duration::seconds(3500) && remaining <= chrono::Duration::seconds(3600)
    );
}

#[tokio::test]
async fn refresh_posts_refresh_grant_and_returns_rotated_token() {
    let (authority, server) = mock_token_endpoint(200, TOKEN_OK).await;
    let tokens = oauth(&authority)
        .refresh(&reqwest::Client::new(), "old-refresh")
        .await
        .unwrap();
    let request = server.await.unwrap();
    assert!(request.contains("grant_type=refresh_token"), "{request}");
    assert!(request.contains("refresh_token=old-refresh"), "{request}");
    assert_eq!(tokens.refresh_token.as_deref(), Some("rt-secret-2"));
}

#[tokio::test]
async fn invalid_grant_asks_for_reauth_with_the_provider_reason() {
    let (authority, _server) = mock_token_endpoint(
        400,
        r#"{"error":"invalid_grant","error_description":"AADSTS70008: The provided authorization code or refresh token has expired due to inactivity.","error_codes":[70008]}"#,
    )
    .await;
    let err = oauth(&authority)
        .refresh(&reqwest::Client::new(), "stale")
        .await
        .unwrap_err();
    match err {
        OAuthError::ReauthRequired { description, .. } => {
            assert!(description.contains("AADSTS70008"), "{description}")
        }
        other => panic!("expected ReauthRequired, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_consent_is_its_own_error() {
    let (authority, _server) = mock_token_endpoint(
        400,
        r#"{"error":"invalid_grant","error_description":"AADSTS65001: The user or administrator has not consented to use the application.","error_codes":[65001]}"#,
    )
    .await;
    let err = oauth(&authority)
        .exchange_code(&reqwest::Client::new(), "c", "http://localhost/x", "v")
        .await
        .unwrap_err();
    assert!(matches!(err, OAuthError::ConsentRequired { .. }), "{err:?}");
}

#[tokio::test]
async fn token_debug_output_never_contains_token_values() {
    let (authority, _server) = mock_token_endpoint(200, TOKEN_OK).await;
    let tokens = oauth(&authority)
        .refresh(&reqwest::Client::new(), "r")
        .await
        .unwrap();
    let debug = format!("{tokens:?}");
    assert!(
        !debug.contains("at-secret-1") && !debug.contains("rt-secret-2"),
        "{debug}"
    );
}

async fn browser_get(port: u16, path_and_query: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let request = format!("GET {path_and_query} HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

#[tokio::test]
async fn loopback_returns_the_code_and_ignores_unrelated_requests() {
    let listener = LoopbackListener::bind("localhost", MICROSOFT_LOOPBACK_PATH)
        .await
        .unwrap();
    let redirect = listener.redirect_uri();
    let port: u16 = url::Url::parse(&redirect).unwrap().port().unwrap();
    assert_eq!(
        redirect,
        format!("http://localhost:{port}{MICROSOFT_LOOPBACK_PATH}")
    );

    let browser = tokio::spawn(async move {
        let favicon = browser_get(port, "/favicon.ico").await;
        let callback = browser_get(
            port,
            &format!("{MICROSOFT_LOOPBACK_PATH}?code=abc123&state=S1"),
        )
        .await;
        (favicon, callback)
    });
    let code = listener
        .wait_for_code("S1", Duration::from_secs(5))
        .await
        .unwrap();
    let (favicon, callback) = browser.await.unwrap();

    assert_eq!(code, "abc123");
    assert!(favicon.starts_with("HTTP/1.1 404"), "{favicon}");
    assert!(callback.starts_with("HTTP/1.1 200"), "{callback}");
    assert!(
        !callback.contains("abc123"),
        "the page must not echo the code"
    );
}

#[tokio::test]
async fn loopback_rejects_a_foreign_state() {
    let listener = LoopbackListener::bind("localhost", MICROSOFT_LOOPBACK_PATH)
        .await
        .unwrap();
    let port: u16 = url::Url::parse(&listener.redirect_uri())
        .unwrap()
        .port()
        .unwrap();
    tokio::spawn(async move {
        browser_get(
            port,
            &format!("{MICROSOFT_LOOPBACK_PATH}?code=abc&state=attacker"),
        )
        .await
    });
    let err = listener
        .wait_for_code("S1", Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(matches!(err, OAuthError::StateMismatch), "{err:?}");
}

#[tokio::test]
async fn loopback_times_out_loudly() {
    let listener = LoopbackListener::bind("localhost", MICROSOFT_LOOPBACK_PATH)
        .await
        .unwrap();
    let err = listener
        .wait_for_code("S1", Duration::from_millis(50))
        .await
        .unwrap_err();
    assert!(matches!(err, OAuthError::Timeout), "{err:?}");
}

#[test]
fn pkce_generation_is_url_safe_and_unique() {
    let a = Pkce::generate();
    let b = Pkce::generate();
    assert_ne!(a.verifier, b.verifier);
    assert_eq!(a.verifier.len(), 43);
    assert!(
        a.verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    );
}

async fn mock_at(
    path_status_body: (u16, &'static str),
) -> (String, tokio::task::JoinHandle<String>) {
    let (authority, handle) = mock_token_endpoint(path_status_body.0, path_status_body.1).await;
    (authority.trim_end_matches("/common").to_string(), handle)
}

#[tokio::test]
async fn google_code_exchange_sends_the_client_secret_and_no_scope() {
    let (base, server) = mock_at((200, r#"{"access_token":"ya29.a","expires_in":3599,"refresh_token":"1//r","scope":"https://mail.google.com/ openid","id_token":"h.e30.s","token_type":"Bearer"}"#)).await;
    let tokens = google(&base)
        .exchange_code(
            &reqwest::Client::new(),
            "4/code",
            "http://127.0.0.1:5555/oauth/google/callback",
            "verifier",
        )
        .await
        .unwrap();
    let request = server.await.unwrap();
    assert!(request.starts_with("POST /token "), "{request}");
    for field in [
        "grant_type=authorization_code",
        "code=4%2Fcode",
        "code_verifier=verifier",
        "client_id=gcid.apps.googleusercontent.com",
        "client_secret=gsecret",
    ] {
        assert!(request.contains(field), "missing {field} in {request}");
    }
    assert!(
        !request.contains("scope="),
        "Google token requests carry no scope: {request}"
    );
    assert_eq!(tokens.refresh_token.as_deref(), Some("1//r"));
    assert!(tokens.id_token.is_some());
}

#[tokio::test]
async fn google_refresh_sends_the_client_secret() {
    let (base, server) = mock_at((200, r#"{"access_token":"ya29.b","expires_in":3599,"scope":"https://mail.google.com/","token_type":"Bearer"}"#)).await;
    let tokens = google(&base)
        .refresh(&reqwest::Client::new(), "1//r")
        .await
        .unwrap();
    let request = server.await.unwrap();
    assert!(
        request.contains("grant_type=refresh_token") && request.contains("client_secret=gsecret"),
        "{request}"
    );
    assert_eq!(
        tokens.refresh_token, None,
        "Google keeps the same refresh token"
    );
}

#[tokio::test]
async fn google_revoked_grant_asks_for_reauth() {
    let (base, _server) = mock_at((
        400,
        r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
    ))
    .await;
    let err = google(&base)
        .refresh(&reqwest::Client::new(), "1//r")
        .await
        .unwrap_err();
    assert!(matches!(err, OAuthError::ReauthRequired { .. }), "{err:?}");
}

#[tokio::test]
async fn revoke_posts_the_token() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_http_request(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        request
    });
    assert!(
        google(&base)
            .revoke(&reqwest::Client::new(), "1//r")
            .await
            .unwrap()
    );
    let request = server.await.unwrap();
    assert!(
        request.starts_with("POST /revoke ") && request.contains("token=1%2F%2Fr"),
        "{request}"
    );
}
