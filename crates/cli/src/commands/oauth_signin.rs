// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Interactive Google sign-in for `accounts add --provider google` and
//! `accounts reauth`. The browser does the signing in; Envelope only ever
//! sees the authorization code and the tokens it is exchanged for.

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use envelope_email_store::oauth_grants::{NewOAuthGrant, TRANSPORT_IMAP_XOAUTH2};
use envelope_email_store::paths;
use envelope_email_transport::oauth::{
    GMAIL_SCOPE, GOOGLE_AUTHORITY, GOOGLE_LOOPBACK_PATH, LoopbackListener, OAuthClient, OAuthError,
    Pkce, ProviderConfig, TokenSet, find_redirect, new_state,
};
use envelope_email_transport::oauth_session::google_client_credentials;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const GMAIL_IMAP_HOST: &str = "imap.gmail.com";
pub const GMAIL_IMAP_PORT: u16 = 993;
pub const GMAIL_SMTP_HOST: &str = "smtp.gmail.com";
pub const GMAIL_SMTP_PORT: u16 = 465;
/// Google's CardDAV scope. This line has no contacts sync, but Envelope v2
/// does and can share the database, so reauth keeps the scope when the old
/// grant had it.
pub const GOOGLE_CARDDAV_SCOPE: &str = "https://www.googleapis.com/auth/carddav";

const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a pasted sign-in stays usable, at the prompt or saved by
/// `--no-wait`. Google's authorization codes don't last longer.
const PASTE_WINDOW: Duration = Duration::from_secs(600);

const PASTE_PROMPT: &str = "After you approve, the browser stops on a \"can't connect\" page. Copy its address (on a phone: tap the address bar, Select All, Copy) and paste it here; pasting the whole page works too:";
const NO_ADDRESS: &str = "No sign-in address in what was pasted. Copy the full address of the page the sign-in ended on; it starts with http://127.0.0.1:1/";

/// How `accounts add` and `accounts reauth` get Google's authorization code.
pub enum SignInMode {
    /// Catch the code on this computer, or with `paste` read the address the
    /// browser ended on from the terminal.
    Interactive { paste: bool },
    /// Print the link, save the sign-in and exit (`--paste --no-wait`).
    Start,
    /// Finish a saved sign-in with the pasted text; `-` reads stdin.
    Finish(String),
}

pub enum SignInOutcome {
    Done(GoogleSignIn),
    Started {
        url: String,
        expires_at: DateTime<Utc>,
    },
}

/// A sign-in waiting for Google's code. The PKCE verifier is useless without
/// that code, but the saved copy is still owner-only and short-lived.
#[derive(Serialize, Deserialize)]
struct PendingSignIn {
    email: String,
    client_id: String,
    state: String,
    verifier: String,
    redirect_uri: String,
    keep_contacts: bool,
    created_at: DateTime<Utc>,
}

impl PendingSignIn {
    fn expires_at(&self) -> DateTime<Utc> {
        self.created_at + chrono::Duration::from_std(PASTE_WINDOW).expect("ten minutes fits")
    }
}

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

/// Signs `email` in with Google in the browser. Pasting means nothing
/// listens locally: the user opens the link on any device and pastes back
/// the address of the page it ends on, now or, after `Start`, in a later
/// `Finish`. `keep_contacts` asks again for the CardDAV scope an earlier
/// grant had; a `Finish` uses what its `Start` asked for.
pub async fn google_sign_in(
    email: &str,
    mode: SignInMode,
    keep_contacts: bool,
) -> Result<SignInOutcome> {
    let dir = pending_dir();
    match mode {
        SignInMode::Finish(pasted) => {
            let pending = load_pending(&dir, email, Utc::now())?;
            let code = if pasted == "-" {
                eprintln!("Paste the address the sign-in ended on (or the whole page):");
                let left = (pending.expires_at() - Utc::now())
                    .to_std()
                    .unwrap_or_default();
                wait_for_paste(io::BufReader::new(io::stdin()), &pending.state, left).await?
            } else {
                read_pasted(&pasted, &pending.state)?.ok_or_else(|| anyhow!(NO_ADDRESS))?
            };
            // Google takes a code once, so this sign-in can't be retried.
            delete_pending(&dir, email)?;
            finish(&pending, &code).await.map(SignInOutcome::Done)
        }
        SignInMode::Start => {
            let (client, client_id) = google_client(keep_contacts)?;
            let (pending, url) = begin(
                &client,
                client_id,
                email,
                keep_contacts,
                paste_redirect_uri(),
            );
            save_pending(&dir, &pending)?;
            Ok(SignInOutcome::Started {
                url,
                expires_at: pending.expires_at(),
            })
        }
        SignInMode::Interactive { paste } => {
            let paste = paste
                || {
                    let elsewhere = browser_is_elsewhere(cfg!(target_os = "linux"), |name| {
                        std::env::var(name).ok()
                    });
                    if elsewhere {
                        eprintln!(
                            "This looks like a remote session, so a browser opened on this computer can't finish the sign-in. Switching to paste."
                        );
                    }
                    elsewhere
                };
            let (client, client_id) = google_client(keep_contacts)?;
            let (pending, code) = if paste {
                let (pending, url) = begin(
                    &client,
                    client_id,
                    email,
                    keep_contacts,
                    paste_redirect_uri(),
                );
                eprintln!(
                    "Open this link on any device and sign in to Google as {email}:\n\n{url}\n"
                );
                eprintln!("{PASTE_PROMPT}");
                let code = wait_for_paste(
                    io::BufReader::new(io::stdin()),
                    &pending.state,
                    PASTE_WINDOW,
                )
                .await?;
                (pending, code)
            } else {
                let config = client.config();
                let listener =
                    LoopbackListener::bind(config.loopback_host, config.loopback_path).await?;
                let (pending, url) = begin(
                    &client,
                    client_id,
                    email,
                    keep_contacts,
                    listener.redirect_uri(),
                );
                eprintln!("Sign in to Google as {email} in your browser:\n\n{url}\n");
                open_in_browser(&url);
                eprintln!(
                    "Waiting for the sign-in to finish (up to {} minutes). On another device? Re-run with --paste.",
                    SIGN_IN_TIMEOUT.as_secs() / 60
                );
                let _ = io::stderr().flush();
                let code = listener
                    .wait_for_code(&pending.state, SIGN_IN_TIMEOUT)
                    .await?;
                (pending, code)
            };
            finish(&pending, &code).await.map(SignInOutcome::Done)
        }
    }
}

fn google_client(keep_contacts: bool) -> Result<(OAuthClient, String)> {
    let (client_id, secret) = google_client_credentials().ok_or_else(|| {
        anyhow!(
            "Google sign-in isn't configured in this build: set ENVELOPE_GOOGLE_CLIENT_ID and ENVELOPE_GOOGLE_CLIENT_SECRET"
        )
    })?;
    let mut config = ProviderConfig::google(&client_id, &secret);
    if keep_contacts {
        config.scopes.push(GOOGLE_CARDDAV_SCOPE.to_string());
    }
    Ok((OAuthClient::new(config)?, client_id))
}

/// Port 1 is never listening, so the browser stops on an error page whose
/// address carries the code.
fn paste_redirect_uri() -> String {
    format!("http://127.0.0.1:1{GOOGLE_LOOPBACK_PATH}")
}

fn begin(
    client: &OAuthClient,
    client_id: String,
    email: &str,
    keep_contacts: bool,
    redirect_uri: String,
) -> (PendingSignIn, String) {
    let pkce = Pkce::generate();
    let state = new_state();
    let url = client
        .authorize_url(&redirect_uri, &state, &pkce, Some(email))
        .to_string();
    let pending = PendingSignIn {
        email: email.to_string(),
        client_id,
        state,
        verifier: pkce.verifier,
        redirect_uri,
        keep_contacts,
        created_at: Utc::now(),
    };
    (pending, url)
}

async fn finish(pending: &PendingSignIn, code: &str) -> Result<GoogleSignIn> {
    let (client, client_id) = google_client(pending.keep_contacts)?;
    if client_id != pending.client_id {
        bail!(
            "This sign-in was started by an Envelope build with a different Google client. Start it again with this one."
        );
    }
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let tokens = client
        .exchange_code(&http, code, &pending.redirect_uri, &pending.verifier)
        .await?;
    check_google_tokens(&pending.email, &tokens)?;
    if pending.keep_contacts && !tokens.has_scope(GOOGLE_CARDDAV_SCOPE) {
        eprintln!(
            "Google didn't grant contacts access this time (the box was unticked), so Envelope v2 can no longer sync this account's Google contacts. Mail works."
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
            "Google didn't grant Gmail access (it granted only: {}). Run the command again and, on Google's consent page, tick the box to read, compose, send and delete your Gmail email, or tick Select all.",
            tokens.scope
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

/// Reads pasted lines until one carries the sign-in address, for at most
/// `limit`. The reading runs on its own thread because a blocked stdin read
/// can't be cancelled.
async fn wait_for_paste(
    reader: impl BufRead + Send + 'static,
    state: &str,
    limit: Duration,
) -> Result<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let state = state.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(scan_paste(reader, &state));
    });
    match tokio::time::timeout(limit, rx).await {
        Ok(Ok(found)) => found,
        Ok(Err(_)) => bail!("reading the pasted address stopped unexpectedly"),
        Err(_) => bail!(
            "No sign-in address was pasted before the sign-in expired. Run the command again for a fresh link."
        ),
    }
}

fn scan_paste(reader: impl BufRead, state: &str) -> Result<String> {
    for line in reader.lines() {
        let line = line.context("reading the pasted address")?;
        if let Some(code) = read_pasted(&line, state)? {
            return Ok(code);
        }
    }
    bail!(NO_ADDRESS)
}

/// The sign-in code in pasted text, if there is one. An address from an
/// older link is told apart from a paste with no address at all.
fn read_pasted(text: &str, state: &str) -> Result<Option<String>> {
    find_redirect(text, state).map_err(|e| match e {
        OAuthError::StateMismatch => anyhow!(
            "That address is from a different sign-in. Paste the one the newest link ended on."
        ),
        other => other.into(),
    })
}

/// Over SSH, or on Linux with no display, a browser opened here isn't the one
/// the user is looking at, so the sign-in can't come back to this computer.
fn browser_is_elsewhere(linux: bool, var: impl Fn(&str) -> Option<String>) -> bool {
    let set = |name: &str| var(name).is_some_and(|v| !v.is_empty());
    set("SSH_CONNECTION") || set("SSH_TTY") || (linux && !set("DISPLAY") && !set("WAYLAND_DISPLAY"))
}

fn pending_dir() -> PathBuf {
    paths::app_data_dir().join("pending-sign-in")
}

fn pending_path(dir: &Path, email: &str) -> PathBuf {
    let digest = Sha256::digest(email.to_ascii_lowercase().as_bytes());
    dir.join(format!("{digest:x}.json"))
}

fn save_pending(dir: &Path, pending: &PendingSignIn) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = pending_path(dir, &pending.email);
    // A file's mode is set only when it is created, so replace, never truncate.
    match fs::remove_file(&path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("replacing {}", path.display()));
        }
        _ => {}
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("saving the sign-in to {}", path.display()))?;
    file.write_all(&serde_json::to_vec(pending)?)
        .with_context(|| format!("saving the sign-in to {}", path.display()))
}

/// The saved sign-in for `email`. An expired one is removed and refused; a
/// live one stays until it is finished, so a bad paste can be retried.
fn load_pending(dir: &Path, email: &str, now: DateTime<Utc>) -> Result<PendingSignIn> {
    let path = pending_path(dir, email);
    let raw = match fs::read(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => bail!(
            "No sign-in for {email} is waiting to be finished. Start one with --paste --no-wait."
        ),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let pending: PendingSignIn = serde_json::from_slice(&raw)
        .with_context(|| format!("{} is unreadable; start the sign-in again", path.display()))?;
    if now > pending.expires_at() {
        delete_pending(dir, email)?;
        bail!(
            "The sign-in for {email} started more than {} minutes ago and has expired. Start it again for a fresh link.",
            PASTE_WINDOW.as_secs() / 60
        );
    }
    Ok(pending)
}

fn delete_pending(dir: &Path, email: &str) -> Result<()> {
    let path = pending_path(dir, email);
    fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))
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
    use std::io::Read;

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
        assert!(
            err.to_string().contains("granted only: openid email"),
            "{err}"
        );
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

    const PAGE: &str = "This site can\u{2019}t be reached\nThe webpage at http://127.0.0.1:1/oauth/google/callback?state=S1&code=4/0Abc&scope=email might be temporarily down.\nERR_UNSAFE_PORT\n";

    fn pending(email: &str, created_at: DateTime<Utc>) -> PendingSignIn {
        PendingSignIn {
            email: email.into(),
            client_id: "client".into(),
            state: "S1".into(),
            verifier: "v".into(),
            redirect_uri: paste_redirect_uri(),
            keep_contacts: false,
            created_at,
        }
    }

    #[tokio::test]
    async fn a_pasted_page_is_read_up_to_the_line_with_the_address() {
        // The reader would block after the page; finding the address on the
        // second line must not wait for more input.
        let reader = io::Cursor::new(PAGE.as_bytes().to_vec()).chain(io::repeat(b'x'));
        let code = wait_for_paste(io::BufReader::new(reader), "S1", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(code, "4/0Abc");
    }

    #[tokio::test]
    async fn a_paste_without_the_address_says_so() {
        let reader = io::Cursor::new(b"This site can't be reached\nERR_UNSAFE_PORT\n".to_vec());
        let err = wait_for_paste(reader, "S1", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("No sign-in address"), "{err}");
    }

    #[tokio::test]
    async fn waiting_for_a_paste_times_out() {
        struct Silent;
        impl io::Read for Silent {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                std::thread::sleep(Duration::from_secs(3600));
                Ok(0)
            }
        }
        let err = wait_for_paste(io::BufReader::new(Silent), "S1", Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("expired"), "{err}");
    }

    #[test]
    fn remote_sessions_switch_to_paste() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(browser_is_elsewhere(
            false,
            env(&[("SSH_CONNECTION", "1 2 3 4")])
        ));
        assert!(browser_is_elsewhere(
            false,
            env(&[("SSH_TTY", "/dev/ttys001")])
        ));
        assert!(browser_is_elsewhere(true, env(&[])));
        assert!(!browser_is_elsewhere(
            true,
            env(&[("WAYLAND_DISPLAY", "wayland-0")])
        ));
        assert!(!browser_is_elsewhere(false, env(&[])));
        assert!(!browser_is_elsewhere(false, env(&[("SSH_TTY", "")])));
    }

    #[test]
    fn a_saved_sign_in_is_owner_only_and_found_by_address() {
        let dir = tempfile::tempdir().unwrap();
        let now = Utc::now();
        save_pending(dir.path(), &pending("You@Gmail.com", now)).unwrap();
        let path = pending_path(dir.path(), "you@gmail.com");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let loaded = load_pending(dir.path(), "you@gmail.com", now).unwrap();
        assert_eq!(loaded.state, "S1");
        // Loading leaves it for another try; finishing deletes it.
        assert!(path.exists());
        delete_pending(dir.path(), "you@gmail.com").unwrap();
        let err = load_pending(dir.path(), "you@gmail.com", now)
            .err()
            .unwrap();
        assert!(err.to_string().contains("No sign-in for"), "{err}");
    }

    #[test]
    fn a_new_start_replaces_the_saved_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let now = Utc::now();
        save_pending(dir.path(), &pending("you@gmail.com", now)).unwrap();
        let mut newer = pending("you@gmail.com", now);
        newer.state = "S2".into();
        save_pending(dir.path(), &newer).unwrap();
        let loaded = load_pending(dir.path(), "you@gmail.com", now).unwrap();
        assert_eq!(loaded.state, "S2");
    }

    #[test]
    fn an_expired_sign_in_is_refused_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let started = Utc::now() - chrono::Duration::minutes(11);
        save_pending(dir.path(), &pending("you@gmail.com", started)).unwrap();
        let err = load_pending(dir.path(), "you@gmail.com", Utc::now())
            .err()
            .unwrap();
        assert!(err.to_string().contains("expired"), "{err}");
        assert!(!pending_path(dir.path(), "you@gmail.com").exists());
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
