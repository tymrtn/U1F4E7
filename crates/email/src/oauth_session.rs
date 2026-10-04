// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Access tokens for stored OAuth grants.
//!
//! Every clone of a grant shares one cache, so the IMAP connection, the IDLE
//! session and the SMTP submit path for an account refresh once per token
//! lifetime, not once per connection. Nothing is written back to the
//! database: Google does not rotate refresh tokens.

use std::time::Duration;

use chrono::Utc;
use envelope_email_store::OAuthGrant;

use crate::oauth::{OAuthClient, OAuthError, ProviderConfig};

/// Refresh this long before expiry so a connection never starts on a token
/// that dies mid-session.
const REFRESH_MARGIN: chrono::Duration = chrono::Duration::minutes(5);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// Google client credentials: the run-time environment wins over the pair
/// compiled in at release-build time. Neither is ever in the repository.
pub fn google_client_credentials() -> Option<(String, String)> {
    let runtime = (
        std::env::var("ENVELOPE_GOOGLE_CLIENT_ID").ok(),
        std::env::var("ENVELOPE_GOOGLE_CLIENT_SECRET").ok(),
    );
    if let (Some(id), Some(secret)) = runtime {
        return Some((id, secret));
    }
    match (
        option_env!("ENVELOPE_GOOGLE_CLIENT_ID"),
        option_env!("ENVELOPE_GOOGLE_CLIENT_SECRET"),
    ) {
        (Some(id), Some(secret)) => Some((id.to_string(), secret.to_string())),
        _ => None,
    }
}

/// The provider client for a stored grant. A Google refresh must use the
/// secret of the client that issued the grant, not whichever is configured.
pub fn client_for_grant(grant: &OAuthGrant) -> Result<OAuthClient, OAuthError> {
    let config = match grant.provider.as_str() {
        "google" => {
            let (id, secret) = google_client_credentials()
                .filter(|(id, _)| *id == grant.client_id)
                .ok_or_else(|| {
                    OAuthError::InvalidConfig(format!(
                        "no client secret for Google client {}; set ENVELOPE_GOOGLE_CLIENT_ID and ENVELOPE_GOOGLE_CLIENT_SECRET",
                        grant.client_id
                    ))
                })?;
            ProviderConfig::google(&id, &secret)
        }
        "microsoft" => ProviderConfig::microsoft(&grant.client_id, &grant.authority),
        other => {
            return Err(OAuthError::InvalidConfig(format!(
                "unknown OAuth provider {other:?}"
            )));
        }
    };
    OAuthClient::new(config)
}

fn cached(grant: &OAuthGrant) -> Option<String> {
    let cache = grant.cache.lock().ok()?;
    match (&cache.access_token, cache.expires_at) {
        (Some(token), Some(expires)) if expires > Utc::now() + REFRESH_MARGIN => {
            Some(token.clone())
        }
        _ => None,
    }
}

/// A usable access token for `grant`, refreshing it if the cached one is
/// missing or about to expire.
pub async fn access_token(grant: &OAuthGrant) -> Result<String, OAuthError> {
    if let Some(token) = cached(grant) {
        return Ok(token);
    }
    access_token_using(grant, &client_for_grant(grant)?).await
}

/// [`access_token`] with an explicit provider client.
pub async fn access_token_using(
    grant: &OAuthGrant,
    client: &OAuthClient,
) -> Result<String, OAuthError> {
    if let Some(token) = cached(grant) {
        return Ok(token);
    }
    // A fresh client per refresh: the dashboard runs several runtimes, and a
    // pooled client used from the wrong one can hang.
    let http = reqwest::Client::builder()
        .timeout(REFRESH_TIMEOUT)
        .build()
        .map_err(|e| OAuthError::Http(format!("building HTTP client: {e}")))?;
    let tokens = client.refresh(&http, &grant.refresh_token).await?;
    if let Ok(mut cache) = grant.cache.lock() {
        cache.access_token = Some(tokens.access_token.clone());
        cache.expires_at = Some(tokens.expires_at);
    }
    Ok(tokens.access_token)
}

/// The message every OAuth sign-in failure carries: what to run to fix it.
pub fn reauth_hint(username: &str) -> String {
    format!("run `envelope accounts reauth {username}` to sign in again")
}
