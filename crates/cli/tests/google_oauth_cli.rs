// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Google sign-in accounts through the real binary, offline: the guards that
//! must refuse before any browser or network step, and removal clearing the
//! stored grant. The grant's client ID matches no configured Google client,
//! so the best-effort revoke on removal fails locally and never reaches
//! Google.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use envelope_email_store::oauth_grants::{NewOAuthGrant, TRANSPORT_IMAP_XOAUTH2};
use envelope_email_store::{CredentialBackend, Database, credential_store};

const GMAIL: &str = "me@gmail.test";
const PASSWORD_ACCOUNT: &str = "pw@corp.test";

fn db_path(home: &Path) -> PathBuf {
    home.join("envelope-email/envelope.db")
}

fn cli(home: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_envelope"))
        .args(args)
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
        .env_remove("ENVELOPE_AGENT_TOKEN")
        .env_remove("ENVELOPE_GOOGLE_CLIENT_ID")
        .env_remove("ENVELOPE_GOOGLE_CLIENT_SECRET")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn envelope");
    if let Some(input) = stdin {
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(format!("{input}\n").as_bytes())
            .unwrap();
    }
    child.wait_with_output().expect("wait envelope")
}

fn ok(out: &Output) {
    assert!(
        out.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn refused(out: &Output) -> String {
    assert!(!out.status.success(), "expected a refusal");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn add_password_account(home: &Path, email: &str) {
    ok(&cli(
        home,
        &[
            "accounts",
            "add",
            "--skip-login-check",
            "--email",
            email,
            "--password-stdin",
            "--smtp-host",
            "smtp.corp.test",
            "--smtp-port",
            "465",
            "--imap-host",
            "imap.corp.test",
            "--imap-port",
            "993",
            "--insecure-machine-key",
        ],
        Some("pw"),
    ));
}

// One test, because it points this process's credential store at the temp
// home through the environment.
#[test]
fn google_accounts_are_guarded_offline() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    // SAFETY: the only test in this binary; nothing else reads the
    // environment concurrently.
    unsafe {
        std::env::set_var("HOME", home);
        std::env::set_var("ENVELOPE_HOME", home);
    }

    add_password_account(home, GMAIL);
    add_password_account(home, PASSWORD_ACCOUNT);
    let db = Database::open(&db_path(home)).unwrap();
    let gmail = db.find_account_by_email(GMAIL).unwrap().unwrap();
    let passphrase = credential_store::get_passphrase(CredentialBackend::File).unwrap();
    db.set_oauth_grant(
        &gmail.id,
        &NewOAuthGrant {
            provider: "google",
            transport: TRANSPORT_IMAP_XOAUTH2,
            client_id: "unconfigured-test-client.apps.googleusercontent.com",
            authority: "https://accounts.google.com",
            scopes: "https://mail.google.com/ openid email",
            refresh_token: "refresh-token-never-sent",
            access_token: None,
            access_expires_at: None,
        },
        &passphrase,
    )
    .unwrap();

    // No password exists to copy, and the sign-in marker is never handed out.
    let err = refused(&cli(
        home,
        &["accounts", "copy-password", "--account", GMAIL],
        None,
    ));
    assert!(
        err.contains("signs in with Google and has no password to copy"),
        "{err}"
    );

    // Adding an address that already exists points at reauth instead.
    let err = refused(&cli(
        home,
        &["accounts", "add", "--provider", "google", "--email", GMAIL],
        None,
    ));
    assert!(
        err.contains(&format!(
            "envelope accounts reauth {GMAIL} --provider google"
        )),
        "{err}"
    );

    // Password flags make no sense with provider sign-in.
    let err = refused(&cli(
        home,
        &[
            "accounts",
            "add",
            "--provider",
            "google",
            "--email",
            "new@gmail.test",
            "--password-stdin",
        ],
        None,
    ));
    assert!(err.contains("cannot be used with"), "{err}");

    // A password account needs --provider to switch.
    let err = refused(&cli(home, &["accounts", "reauth", PASSWORD_ACCOUNT], None));
    assert!(err.contains("signs in with a password"), "{err}");

    // Removal deletes the grant with the account; the revoke fails locally.
    let out = cli(home, &["accounts", "remove", GMAIL], None);
    ok(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Couldn't revoke"), "{stderr}");
    assert!(!db.has_oauth_grant(&gmail.id).unwrap());
    assert!(db.find_account_by_email(GMAIL).unwrap().is_none());
}
