// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Interactive Google sign-in for `accounts add --provider google` and
//! `accounts reauth`. The browser does the signing in; Envelope only ever
//! sees the authorization code and the tokens it is exchanged for.

use std::io::{self, BufRead, Write};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use envelope_email_store::oauth_grants::{NewOAuthGrant, TRANSPORT_IMAP_XOAUTH2};
use envelope_email_transport::carddav::GOOGLE_CARDDAV_SCOPE;
use envelope_email_transport::oauth::{
    GMAIL_SCOPE, GOOGLE_AUTHORITY, GOOGLE_LOOPBACK_PATH, LoopbackListener, OAuthClient, Pkce,
    ProviderConfig, TokenSet, new_state, parse_redirect,
};
use envelope_email_transport::oauth_session::google_client_credentials;

pub const GMAIL_IMAP_HOST: &str = "imap.gmail.com";
pub const GMAIL_IMAP_PORT: u16 = 993;
pub const GMAIL_SMTP_HOST: &str = "smtp.gmail.com";
pub const GMAIL_SMTP_PORT: u16 = 465;

const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

/// A completed Google sign-in for one address.
pub struct GoogleSignIn {
    pub client_id: String,
    pub tokens: TokenSet,
    pub refresh_token: String,
}

impl GoogleSignIn {
    pub fn grant(&self) -> NewOAuthGrant<'_> {
        NewOAuthGrant {
            provider: "google",
            transport: TRANSPORT_IMAP_XOAUTH2,
            client_id: &self.client_id,
            authority: GOOGLE_AUTHORITY,
            scopes: &self.tokens.scope,
            refresh_token: &self.refresh_token,
            access_token: Some(&self.tokens.access_token),
            access_expires_at: Some(self.tokens.expires_at),
        }
    }
}

/// Signs `email` in with Google in the browser. With `paste`, nothing
/// listens locally: the user opens the link on any device and pastes back
/// the address of the page it ends on. `contacts` also asks for the CardDAV
/// scope, so a Google contact source can sync with the same grant.
pub async fn google_sign_in(email: &str, paste: bool, contacts: bool) -> Result<GoogleSignIn> {
    let (client_id, secret) = google_client_credentials().ok_or_else(|| {
        anyhow!(
            "Google sign-in isn't configured in this build: set ENVELOPE_GOOGLE_CLIENT_ID and ENVELOPE_GOOGLE_CLIENT_SECRET"
        )
    })?;
    let mut config = ProviderConfig::google(&client_id, &secret);
    if contacts {
        config.scopes.push(GOOGLE_CARDDAV_SCOPE.to_string());
    }
    let client = OAuthClient::new(config)?;
    let pkce = Pkce::generate();
    let state = new_state();

    let (code, redirect_uri) = if paste {
        // Port 1 is never listening, so the browser stops on an error page
        // whose address carries the code.
        let redirect_uri = format!("http://127.0.0.1:1{GOOGLE_LOOPBACK_PATH}");
        let url = client.authorize_url(&redirect_uri, &state, &pkce, Some(email));
        eprintln!("Open this link on any device and sign in to Google as {email}:\n\n{url}\n");
        eprintln!(
            "The browser then shows a \"can't connect\" page. Copy that page's full address and paste it here:"
        );
        let mut line = String::new();
        io::stdin()
            .lock()
            .read_line(&mut line)
            .context("reading the pasted address")?;
        (parse_redirect(&line, &state)?, redirect_uri)
    } else {
        let config = client.config();
        let listener = LoopbackListener::bind(config.loopback_host, config.loopback_path).await?;
        let redirect_uri = listener.redirect_uri();
        let url = client.authorize_url(&redirect_uri, &state, &pkce, Some(email));
        eprintln!("Sign in to Google as {email} in your browser:\n\n{url}\n");
        open_in_browser(url.as_str());
        eprintln!(
            "Waiting for the sign-in to finish (up to {} minutes). On another device? Re-run with --paste.",
            SIGN_IN_TIMEOUT.as_secs() / 60
        );
        let _ = io::stderr().flush();
        (
            listener.wait_for_code(&state, SIGN_IN_TIMEOUT).await?,
            redirect_uri,
        )
    };

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let tokens = client
        .exchange_code(&http, &code, &redirect_uri, &pkce.verifier)
        .await?;
    check_google_tokens(email, &tokens)?;
    if contacts && !tokens.has_scope(GOOGLE_CARDDAV_SCOPE) {
        eprintln!(
            "Google didn't grant contacts access (the box was unticked), so Google contacts won't sync. Mail works; run `envelope accounts reauth {email} --contacts` to try again."
        );
    }
    let refresh_token = tokens.refresh_token.clone().ok_or_else(|| {
        anyhow!("Google returned no refresh token, so the sign-in could not be kept; run the command again")
    })?;
    Ok(GoogleSignIn {
        client_id,
        tokens,
        refresh_token,
    })
}

/// Google lets the user untick Gmail access on the consent screen and sign
/// in as a different account than asked; both must stop the add.
pub fn check_google_tokens(email: &str, tokens: &TokenSet) -> Result<()> {
    if !tokens.has_scope(GMAIL_SCOPE) {
        bail!(
            "Google didn't grant Gmail access: the box for reading, composing and sending mail was unticked. Run the command again and leave it ticked."
        );
    }
    let identity = tokens.identity()?;
    if !identity.email_verified {
        bail!(
            "Google reports {} as unverified, so Envelope won't use it",
            identity.email
        );
    }
    if !identity.email.eq_ignore_ascii_case(email) {
        bail!(
            "You signed in to Google as {}, not {email}. Run the command again and choose {email}, or add {} instead.",
            identity.email,
            identity.email
        );
    }
    Ok(())
}

/// Best effort: the link is printed either way.
fn open_in_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn tokens(scope: &str, claims: &str) -> TokenSet {
        TokenSet {
            access_token: "a".into(),
            refresh_token: Some("r".into()),
            expires_at: chrono::Utc::now(),
            scope: scope.into(),
            id_token: Some(format!("h.{}.s", URL_SAFE_NO_PAD.encode(claims))),
        }
    }

    #[test]
    fn a_matching_verified_gmail_grant_passes() {
        let t = tokens(
            "openid https://mail.google.com/ email",
            r#"{"email":"You@Gmail.com","email_verified":true}"#,
        );
        check_google_tokens("you@gmail.com", &t).unwrap();
    }

    #[test]
    fn an_unticked_gmail_scope_is_refused() {
        let t = tokens(
            "openid email",
            r#"{"email":"you@gmail.com","email_verified":true}"#,
        );
        let err = check_google_tokens("you@gmail.com", &t).unwrap_err();
        assert!(err.to_string().contains("unticked"), "{err}");
    }

    #[test]
    fn a_different_google_account_is_refused_by_name() {
        let t = tokens(
            "https://mail.google.com/",
            r#"{"email":"other@gmail.com","email_verified":true}"#,
        );
        let err = check_google_tokens("you@gmail.com", &t).unwrap_err();
        assert!(err.to_string().contains("other@gmail.com"), "{err}");
    }

    #[test]
    fn an_unverified_address_is_refused() {
        let t = tokens(
            "https://mail.google.com/",
            r#"{"email":"you@gmail.com","email_verified":false}"#,
        );
        assert!(check_google_tokens("you@gmail.com", &t).is_err());
    }
}
