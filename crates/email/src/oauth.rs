// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! OAuth sign-in for mail providers: the authorization code flow with PKCE
//! for an installed app. The PKCE verifier never leaves the process that
//! started the sign-in. Google desktop clients also carry a client secret,
//! which Google documents as not confidential for installed apps; Microsoft
//! public clients carry none.

use std::fmt;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

pub const MICROSOFT_AUTHORITY: &str = "https://login.microsoftonline.com/common";
/// Paste-back redirect: the browser stops on a Microsoft page and the user
/// copies that page's address into Envelope. Works from any device.
pub const MICROSOFT_NATIVECLIENT_REDIRECT: &str =
    "https://login.microsoftonline.com/common/oauth2/nativeclient";
/// Must match the path registered for `http://localhost` in the Entra app.
/// Entra ignores the port on localhost redirects, so any bound port matches.
pub const MICROSOFT_LOOPBACK_PATH: &str = "/oauth/microsoft/callback";
pub const MICROSOFT_MAIL_SCOPES: &[&str] = &[
    "offline_access",
    "User.Read",
    "Mail.ReadWrite",
    "Mail.Send",
    "MailboxSettings.Read",
];

pub const GOOGLE_AUTHORITY: &str = "https://accounts.google.com";
pub const GOOGLE_AUTHORIZE_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const GOOGLE_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
pub const GOOGLE_REVOKE_ENDPOINT: &str = "https://oauth2.googleapis.com/revoke";
pub const GOOGLE_LOOPBACK_PATH: &str = "/oauth/google/callback";
/// Gmail's only scope that allows IMAP and SMTP with XOAUTH2.
pub const GMAIL_SCOPE: &str = "https://mail.google.com/";
pub const GOOGLE_MAIL_SCOPES: &[&str] = &[GMAIL_SCOPE, "openid", "email"];

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REQUEST_HEAD: usize = 8 * 1024;
const AADSTS_CONSENT_REQUIRED: u64 = 65001;

#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("invalid OAuth configuration: {0}")]
    InvalidConfig(String),
    #[error("the sign-in response does not belong to this sign-in attempt (state mismatch)")]
    StateMismatch,
    #[error("the sign-in response carried no authorization code")]
    MissingCode,
    #[error("timed out waiting for the browser to come back from sign-in")]
    Timeout,
    #[error("the provider needs consent before Envelope can use this mailbox: {description}")]
    ConsentRequired { description: String },
    #[error("sign-in has to be repeated ({error}): {description}")]
    ReauthRequired { error: String, description: String },
    #[error("sign-in failed ({error}): {description}")]
    Provider { error: String, description: String },
    #[error("token request failed: {0}")]
    Http(String),
    #[error("unexpected sign-in response: {0}")]
    MalformedResponse(String),
    #[error("local sign-in listener failed: {0}")]
    Loopback(String),
}

/// Everything that differs between providers.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub provider: &'static str,
    /// Recorded with the grant; where the tokens came from.
    pub authority: String,
    pub authorize_endpoint: String,
    pub token_endpoint: String,
    pub revoke_endpoint: Option<String>,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
    pub authorize_params: Vec<(&'static str, &'static str)>,
    /// Microsoft's v2 endpoint wants `scope` on token requests; Google does not.
    pub scope_on_token_requests: bool,
    pub loopback_host: &'static str,
    pub loopback_path: &'static str,
}

impl ProviderConfig {
    pub fn microsoft(client_id: &str, authority: &str) -> Self {
        let authority = authority.trim_end_matches('/').to_string();
        Self {
            provider: "microsoft",
            authorize_endpoint: format!("{authority}/oauth2/v2.0/authorize"),
            token_endpoint: format!("{authority}/oauth2/v2.0/token"),
            revoke_endpoint: None,
            authority,
            client_id: client_id.trim().to_string(),
            client_secret: None,
            scopes: MICROSOFT_MAIL_SCOPES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            authorize_params: vec![("response_mode", "query"), ("prompt", "select_account")],
            scope_on_token_requests: true,
            loopback_host: "localhost",
            loopback_path: MICROSOFT_LOOPBACK_PATH,
        }
    }

    pub fn google(client_id: &str, client_secret: &str) -> Self {
        Self {
            provider: "google",
            authority: GOOGLE_AUTHORITY.to_string(),
            authorize_endpoint: GOOGLE_AUTHORIZE_ENDPOINT.to_string(),
            token_endpoint: GOOGLE_TOKEN_ENDPOINT.to_string(),
            revoke_endpoint: Some(GOOGLE_REVOKE_ENDPOINT.to_string()),
            client_id: client_id.trim().to_string(),
            client_secret: Some(client_secret.trim().to_string()),
            scopes: GOOGLE_MAIL_SCOPES.iter().map(|s| s.to_string()).collect(),
            // `prompt=consent` makes Google show the scope checkboxes again on
            // reauth, so a user who unticked Gmail access can grant it.
            authorize_params: vec![("access_type", "offline"), ("prompt", "consent")],
            scope_on_token_requests: false,
            // Google recommends the IP literal over `localhost` for loopback.
            loopback_host: "127.0.0.1",
            loopback_path: GOOGLE_LOOPBACK_PATH,
        }
    }
}

/// PKCE pair (RFC 7636, S256).
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Self {
        Self::from_verifier(&random_token())
    }

    pub fn from_verifier(verifier: &str) -> Self {
        Self {
            verifier: verifier.to_string(),
            challenge: URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        }
    }
}

/// Unguessable value binding a redirect to the sign-in that started it.
pub fn new_state() -> String {
    random_token()
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub struct TokenSet {
    pub access_token: String,
    /// Microsoft rotates refresh tokens; `None` means keep the stored one.
    pub refresh_token: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub scope: String,
    /// OpenID Connect ID token, present when `openid` was requested.
    pub id_token: Option<String>,
}

impl fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &"[redacted]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[redacted]"),
            )
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .field("id_token", &self.id_token.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// Who signed in, from the ID token's claims.
#[derive(Debug, PartialEq)]
pub struct Identity {
    pub email: String,
    pub email_verified: bool,
}

impl TokenSet {
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scope.split_whitespace().any(|s| s == scope)
    }

    /// Reads the `email` claims from the ID token. The token came straight
    /// from the provider's token endpoint over TLS, which Google documents as
    /// sufficient without checking the signature.
    pub fn identity(&self) -> Result<Identity, OAuthError> {
        let token = self.id_token.as_deref().ok_or_else(|| {
            OAuthError::MalformedResponse("no ID token; was `openid` requested?".into())
        })?;
        let payload = token
            .split('.')
            .nth(1)
            .ok_or_else(|| OAuthError::MalformedResponse("ID token is not a JWT".into()))?;
        let bytes = URL_SAFE_NO_PAD
            .decode(payload.trim_end_matches('='))
            .map_err(|e| OAuthError::MalformedResponse(format!("ID token payload: {e}")))?;
        let claims: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| OAuthError::MalformedResponse(format!("ID token claims: {e}")))?;
        let email = claims["email"]
            .as_str()
            .ok_or_else(|| {
                OAuthError::MalformedResponse(
                    "ID token has no email; was `email` requested?".into(),
                )
            })?
            .to_string();
        let email_verified = match &claims["email_verified"] {
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::String(s) => s == "true",
            _ => false,
        };
        Ok(Identity {
            email,
            email_verified,
        })
    }
}

pub struct OAuthClient {
    config: ProviderConfig,
    authorize_endpoint: Url,
    token_endpoint: Url,
    revoke_endpoint: Option<Url>,
}

/// Endpoints must be https; plain http only on loopback, for local mocks.
fn endpoint(name: &str, raw: &str) -> Result<Url, OAuthError> {
    let url = Url::parse(raw)
        .map_err(|e| OAuthError::InvalidConfig(format!("{name} {raw:?} is not a URL: {e}")))?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(OAuthError::InvalidConfig(format!(
            "{name} {raw:?} must use https"
        )));
    }
    Ok(url)
}

impl OAuthClient {
    pub fn new(config: ProviderConfig) -> Result<Self, OAuthError> {
        if config.client_id.is_empty() {
            return Err(OAuthError::InvalidConfig(format!(
                "{} client id is empty",
                config.provider
            )));
        }
        if config
            .client_secret
            .as_deref()
            .is_some_and(|secret| secret.is_empty())
        {
            return Err(OAuthError::InvalidConfig(format!(
                "{} client secret is empty",
                config.provider
            )));
        }
        let authorize_endpoint = endpoint("authorize endpoint", &config.authorize_endpoint)?;
        let token_endpoint = endpoint("token endpoint", &config.token_endpoint)?;
        let revoke_endpoint = config
            .revoke_endpoint
            .as_deref()
            .map(|raw| endpoint("revoke endpoint", raw))
            .transpose()?;
        Ok(Self {
            config,
            authorize_endpoint,
            token_endpoint,
            revoke_endpoint,
        })
    }

    pub fn config(&self) -> &ProviderConfig {
        &self.config
    }

    pub fn scope(&self) -> String {
        self.config.scopes.join(" ")
    }

    pub fn authorize_url(
        &self,
        redirect_uri: &str,
        state: &str,
        pkce: &Pkce,
        login_hint: Option<&str>,
    ) -> Url {
        let mut url = self.authorize_endpoint.clone();
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("client_id", &self.config.client_id)
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", redirect_uri)
                .append_pair("scope", &self.scope())
                .append_pair("state", state)
                .append_pair("code_challenge", &pkce.challenge)
                .append_pair("code_challenge_method", "S256");
            for (key, value) in &self.config.authorize_params {
                query.append_pair(key, value);
            }
            if let Some(hint) = login_hint {
                query.append_pair("login_hint", hint);
            }
        }
        url
    }

    pub async fn exchange_code(
        &self,
        http: &reqwest::Client,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<TokenSet, OAuthError> {
        self.token_request(
            http,
            vec![
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("code_verifier", verifier),
            ],
        )
        .await
    }

    pub async fn refresh(
        &self,
        http: &reqwest::Client,
        refresh_token: &str,
    ) -> Result<TokenSet, OAuthError> {
        self.token_request(
            http,
            vec![
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
            ],
        )
        .await
    }

    /// Best-effort revocation of a refresh or access token. `Ok(false)` when
    /// the provider has no revocation endpoint.
    pub async fn revoke(&self, http: &reqwest::Client, token: &str) -> Result<bool, OAuthError> {
        let Some(revoke) = self.revoke_endpoint.clone() else {
            return Ok(false);
        };
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("token", token)
            .finish();
        let resp = http
            .post(revoke)
            .timeout(REQUEST_TIMEOUT)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| OAuthError::Http(transport_error_summary(&e)))?;
        if resp.status().is_success() {
            Ok(true)
        } else {
            Err(OAuthError::Http(format!(
                "revocation returned HTTP {}",
                resp.status().as_u16()
            )))
        }
    }

    async fn token_request(
        &self,
        http: &reqwest::Client,
        mut form: Vec<(&str, &str)>,
    ) -> Result<TokenSet, OAuthError> {
        let scope = self.scope();
        form.push(("client_id", &self.config.client_id));
        if let Some(secret) = &self.config.client_secret {
            form.push(("client_secret", secret));
        }
        if self.config.scope_on_token_requests {
            form.push(("scope", &scope));
        }
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(&form)
            .finish();
        // reqwest error strings never include the request body, so the code,
        // verifier, secret and refresh token cannot leak through `Http`.
        let resp = http
            .post(self.token_endpoint.clone())
            .timeout(REQUEST_TIMEOUT)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| OAuthError::Http(transport_error_summary(&e)))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| OAuthError::Http(transport_error_summary(&e)))?;

        if status.is_success() {
            let parsed: TokenResponse = serde_json::from_str(&text).map_err(|e| {
                OAuthError::MalformedResponse(format!("token response did not parse: {e}"))
            })?;
            return Ok(TokenSet {
                access_token: parsed.access_token,
                refresh_token: parsed.refresh_token,
                expires_at: Utc::now() + chrono::Duration::seconds(parsed.expires_in),
                scope: parsed.scope.unwrap_or_default(),
                id_token: parsed.id_token,
            });
        }

        match serde_json::from_str::<ErrorResponse>(&text) {
            Ok(err) => Err(classify(
                err.error,
                err.error_description.unwrap_or_default(),
                &err.error_codes.unwrap_or_default(),
            )),
            Err(_) => Err(OAuthError::Http(format!(
                "token endpoint returned HTTP {}",
                status.as_u16()
            ))),
        }
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
    scope: Option<String>,
    id_token: Option<String>,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
    error_description: Option<String>,
    error_codes: Option<Vec<u64>>,
}

fn classify(error: String, description: String, codes: &[u64]) -> OAuthError {
    let consent = error == "consent_required"
        || codes.contains(&AADSTS_CONSENT_REQUIRED)
        || description.contains("AADSTS65001");
    if consent {
        OAuthError::ConsentRequired { description }
    } else if matches!(
        error.as_str(),
        "invalid_grant" | "interaction_required" | "login_required"
    ) {
        OAuthError::ReauthRequired { error, description }
    } else {
        OAuthError::Provider { error, description }
    }
}

fn transport_error_summary(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "request timed out".to_string()
    } else if e.is_connect() {
        "connection failed".to_string()
    } else {
        "transport error".to_string()
    }
}

/// The SASL XOAUTH2 initial client response (before base64), shared by IMAP
/// and SMTP: `user=<address>^Aauth=Bearer <token>^A^A`.
pub fn xoauth2_initial_response(user: &str, access_token: &str) -> String {
    format!("user={user}\x01auth=Bearer {access_token}\x01\x01")
}

/// Reads the authorization code out of a redirect URL: the loopback request
/// target, or the address a user pastes back from another device.
pub fn parse_redirect(redirect: &str, expected_state: &str) -> Result<String, OAuthError> {
    let url = Url::parse(redirect.trim()).map_err(|_| {
        OAuthError::MalformedResponse(
            "expected the full address of the page the sign-in sent you to".into(),
        )
    })?;
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    if param("state").as_deref() != Some(expected_state) {
        return Err(OAuthError::StateMismatch);
    }
    if let Some(error) = param("error") {
        return Err(classify(
            error,
            param("error_description").unwrap_or_default(),
            &[],
        ));
    }
    match param("code") {
        Some(code) if !code.is_empty() => Ok(code),
        _ => Err(OAuthError::MissingCode),
    }
}

const PAGE_DONE: &str = "<!doctype html><title>Envelope</title><p>Signed in. You can close this tab and go back to Envelope.</p>";
const PAGE_FAILED: &str = "<!doctype html><title>Envelope</title><p>Sign-in did not complete. Go back to Envelope for the reason.</p>";

/// One-shot HTTP listener on 127.0.0.1 that catches the browser's redirect.
pub struct LoopbackListener {
    listener: TcpListener,
    port: u16,
    host: &'static str,
    path: &'static str,
}

impl LoopbackListener {
    /// `host` is what the redirect URI names (`localhost` for Microsoft,
    /// `127.0.0.1` for Google); the socket is always IPv4 loopback.
    pub async fn bind(host: &'static str, path: &'static str) -> Result<Self, OAuthError> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| OAuthError::Loopback(format!("bind 127.0.0.1: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| OAuthError::Loopback(e.to_string()))?
            .port();
        Ok(Self {
            listener,
            port,
            host,
            path,
        })
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://{}:{}{}", self.host, self.port, self.path)
    }

    /// Serves requests until the callback arrives, then returns its code.
    /// Other paths (the browser's favicon fetch) get a 404 and are ignored.
    pub async fn wait_for_code(
        self,
        expected_state: &str,
        timeout: Duration,
    ) -> Result<String, OAuthError> {
        tokio::time::timeout(timeout, self.serve_until_callback(expected_state))
            .await
            .map_err(|_| OAuthError::Timeout)?
    }

    async fn serve_until_callback(&self, expected_state: &str) -> Result<String, OAuthError> {
        loop {
            let (mut stream, _) = self
                .listener
                .accept()
                .await
                .map_err(|e| OAuthError::Loopback(format!("accept: {e}")))?;
            let Some(target) = read_get_target(&mut stream).await else {
                respond(&mut stream, "404 Not Found", "").await;
                continue;
            };
            if target.split('?').next() != Some(self.path) {
                respond(&mut stream, "404 Not Found", "").await;
                continue;
            }
            let result = parse_redirect(&format!("http://localhost{target}"), expected_state);
            match &result {
                Ok(_) => respond(&mut stream, "200 OK", PAGE_DONE).await,
                Err(_) => respond(&mut stream, "400 Bad Request", PAGE_FAILED).await,
            }
            return result;
        }
    }
}

/// Request target of a GET, or `None` for anything else.
async fn read_get_target(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 || head.len() + n > MAX_REQUEST_HEAD {
            return None;
        }
        head.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8(head).ok()?;
    let mut parts = head.lines().next()?.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) if target.starts_with('/') => Some(target.to_string()),
        _ => None,
    }
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    // The browser tab is the only reader; a failed write changes nothing
    // about the code already parsed.
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn microsoft() -> OAuthClient {
        OAuthClient::new(ProviderConfig::microsoft("client-123", MICROSOFT_AUTHORITY)).unwrap()
    }

    fn google() -> OAuthClient {
        OAuthClient::new(ProviderConfig::google(
            "1234567890-x.apps.googleusercontent.com",
            "secret",
        ))
        .unwrap()
    }

    fn token_with_id(claims: &str) -> TokenSet {
        TokenSet {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: Utc::now(),
            scope: "https://mail.google.com/ openid https://www.googleapis.com/auth/userinfo.email"
                .into(),
            id_token: Some(format!(
                "header.{}.sig",
                URL_SAFE_NO_PAD.encode(claims.as_bytes())
            )),
        }
    }

    #[test]
    fn pkce_matches_rfc7636_appendix_b() {
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(
            pkce.challenge,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn states_are_unique() {
        assert_ne!(new_state(), new_state());
        assert_eq!(new_state().len(), 43);
    }

    #[test]
    fn microsoft_authorize_url_carries_pkce_and_scopes() {
        let pkce = Pkce::from_verifier("v");
        let url = microsoft().authorize_url(
            "http://localhost:4000/oauth/microsoft/callback",
            "S1",
            &pkce,
            Some("you@outlook.com"),
        );
        assert_eq!(
            url.as_str().split('?').next(),
            Some("https://login.microsoftonline.com/common/oauth2/v2.0/authorize")
        );
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "client-123");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["response_mode"], "query");
        assert_eq!(q["state"], "S1");
        assert_eq!(q["code_challenge"], pkce.challenge);
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["login_hint"], "you@outlook.com");
        assert_eq!(
            q["scope"],
            "offline_access User.Read Mail.ReadWrite Mail.Send MailboxSettings.Read"
        );
    }

    #[test]
    fn google_authorize_url_asks_for_gmail_and_offline_access() {
        let pkce = Pkce::from_verifier("v");
        let url = google().authorize_url(
            "http://127.0.0.1:4000/oauth/google/callback",
            "S1",
            &pkce,
            Some("you@gmail.com"),
        );
        assert_eq!(
            url.as_str().split('?').next(),
            Some("https://accounts.google.com/o/oauth2/v2/auth")
        );
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["scope"], "https://mail.google.com/ openid email");
        assert_eq!(q["access_type"], "offline");
        assert_eq!(q["prompt"], "consent");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["login_hint"], "you@gmail.com");
        assert!(!q.contains_key("client_secret"), "never in a browser URL");
    }

    #[test]
    fn endpoints_must_be_https_off_loopback() {
        let mut config = ProviderConfig::google("c", "s");
        config.token_endpoint = "http://oauth2.example/token".into();
        assert!(matches!(
            OAuthClient::new(config),
            Err(OAuthError::InvalidConfig(_))
        ));
        let mut config = ProviderConfig::google("c", "s");
        config.token_endpoint = "http://127.0.0.1:9/token".into();
        assert!(OAuthClient::new(config).is_ok());
        assert!(matches!(
            OAuthClient::new(ProviderConfig::google("  ", "s")),
            Err(OAuthError::InvalidConfig(_))
        ));
        assert!(matches!(
            OAuthClient::new(ProviderConfig::google("c", " ")),
            Err(OAuthError::InvalidConfig(_))
        ));
    }

    #[test]
    fn identity_reads_email_claims_from_the_id_token() {
        let tokens = token_with_id(r#"{"email":"You@Gmail.com","email_verified":true}"#);
        assert_eq!(
            tokens.identity().unwrap(),
            Identity {
                email: "You@Gmail.com".into(),
                email_verified: true
            }
        );
        assert!(tokens.has_scope(GMAIL_SCOPE));
        assert!(!tokens.has_scope("https://mail.google.com"));
        let unverified = token_with_id(r#"{"email":"x@gmail.com","email_verified":"false"}"#);
        assert!(!unverified.identity().unwrap().email_verified);
        let no_email = token_with_id(r#"{"sub":"1"}"#);
        assert!(matches!(
            no_email.identity(),
            Err(OAuthError::MalformedResponse(_))
        ));
    }

    #[test]
    fn xoauth2_initial_response_matches_the_sasl_format() {
        assert_eq!(
            xoauth2_initial_response("you@gmail.com", "ya29.tok"),
            "user=you@gmail.com\x01auth=Bearer ya29.tok\x01\x01"
        );
    }

    #[test]
    fn parse_redirect_accepts_a_pasted_address() {
        let pasted =
            "  http://127.0.0.1:53817/oauth/google/callback?state=S1&code=4/0Abc&scope=email\n";
        assert_eq!(parse_redirect(pasted, "S1").unwrap(), "4/0Abc");
    }

    #[test]
    fn parse_redirect_checks_state_before_anything_else() {
        let url = "https://x.test/cb?error=access_denied&state=other";
        assert!(matches!(
            parse_redirect(url, "S1"),
            Err(OAuthError::StateMismatch)
        ));
        assert!(matches!(
            parse_redirect("https://x.test/cb?code=abc", "S1"),
            Err(OAuthError::StateMismatch)
        ));
    }

    #[test]
    fn parse_redirect_surfaces_provider_errors() {
        let denied =
            "https://x.test/cb?state=S1&error=access_denied&error_description=user+declined";
        match parse_redirect(denied, "S1") {
            Err(OAuthError::Provider { error, description }) => {
                assert_eq!(error, "access_denied");
                assert_eq!(description, "user declined");
            }
            other => panic!("expected Provider, got {other:?}"),
        }
        assert!(matches!(
            parse_redirect("https://x.test/cb?state=S1&error=consent_required", "S1"),
            Err(OAuthError::ConsentRequired { .. })
        ));
        assert!(matches!(
            parse_redirect("https://x.test/cb?state=S1", "S1"),
            Err(OAuthError::MissingCode)
        ));
        assert!(matches!(
            parse_redirect("not a url", "S1"),
            Err(OAuthError::MalformedResponse(_))
        ));
    }
}
