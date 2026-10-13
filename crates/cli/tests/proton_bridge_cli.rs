// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)
//
// Adding an account served by a local bridge with its own certificate, as
// Proton Mail Bridge does: IMAP over STARTTLS on a high port, and one
// trusted certificate. Everything runs against a loopback fake server in an
// isolated ENVELOPE_HOME; nothing reaches a real mailbox.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use envelope_email_transport::tls::testing::{self, Mode, ServerCert};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, SanType};
use serde_json::Value;

/// Self-signed, a CA certificate, naming 127.0.0.1: the shape of Proton
/// Mail Bridge's certificate.
fn bridge_like_cert() -> (ServerCert, String) {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params
        .distinguished_name
        .push(DnType::CommonName, "127.0.0.1");
    params.subject_alt_names = vec![SanType::IpAddress([127, 0, 0, 1].into())];
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let key = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    (
        ServerCert {
            cert_der: cert.der().to_vec(),
            key_der: key.serialize_der(),
        },
        cert.pem(),
    )
}

fn envelope(home: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_envelope"))
        .args(args)
        .env("ENVELOPE_MASTER_KEY", "test-master-key-proton-bridge")
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
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
    drop(child.stdin.take());
    child.wait_with_output().expect("wait for envelope")
}

async fn run(home: &Path, args: Vec<String>, stdin: Option<&'static str>) -> Output {
    let home = home.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        envelope(&home, &args, stdin)
    })
    .await
    .unwrap()
}

fn add_args(imap_port: u16, cert_file: &Path) -> Vec<String> {
    [
        "accounts",
        "add",
        "--email",
        "you@proton.me",
        "--password-stdin",
        "--imap-host",
        "127.0.0.1",
        "--imap-port",
        &imap_port.to_string(),
        "--imap-security",
        "starttls",
        "--smtp-host",
        "127.0.0.1",
        "--smtp-port",
        "1025",
        "--trust-cert",
        cert_file.to_str().unwrap(),
    ]
    .map(String::from)
    .to_vec()
}

fn listed(home: &Path) -> Value {
    let out = envelope(home, &["--json", "accounts", "list"], None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn fingerprint(cert: &ServerCert) -> String {
    envelope_email_store::server_tls::sha256_fingerprint(&cert.cert_der)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bridge_account_logs_in_over_starttls_with_its_certificate_and_keeps_it() {
    let home = tempfile::tempdir().unwrap();
    let (cert, pem) = bridge_like_cert();
    let cert_file = home.path().join("cert.pem");
    std::fs::write(&cert_file, pem).unwrap();
    let server = testing::imap(Mode::Starttls, cert.clone()).await;

    let out = run(
        home.path(),
        add_args(server.port, &cert_file),
        Some("bridge-password"),
    )
    .await;

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    assert!(stderr.contains("IMAP login ok."), "stderr: {stderr}");
    assert_eq!(
        server.transcript.plaintext_verbs(),
        vec!["CAPABILITY", "STARTTLS"]
    );
    assert_eq!(server.transcript.tls_verbs(), vec!["LOGIN"]);

    let accounts = listed(home.path());
    let account = &accounts[0];
    assert_eq!(account["tls"]["imap_security"], "STARTTLS");
    assert_eq!(account["tls"]["pinned_cert_sha256"], fingerprint(&cert));
    let id = account["id"].as_str().unwrap();
    let settings_file = home
        .path()
        .join("envelope-email/server-tls")
        .join(format!("{id}.json"));
    assert!(settings_file.exists(), "{}", settings_file.display());

    let out = envelope(home.path(), &["accounts", "remove", "you@proton.me"], None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!settings_file.exists(), "remove deletes the TLS settings");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_certificate_is_refused_and_can_be_trusted_again() {
    let home = tempfile::tempdir().unwrap();
    let (old_cert, old_pem) = bridge_like_cert();
    let (new_cert, new_pem) = bridge_like_cert();
    let old_file = home.path().join("old.pem");
    let new_file = home.path().join("new.pem");
    std::fs::write(&old_file, old_pem).unwrap();
    std::fs::write(&new_file, new_pem).unwrap();
    let server = testing::imap(Mode::Starttls, new_cert.clone()).await;

    let out = run(
        home.path(),
        add_args(server.port, &old_file),
        Some("bridge-password"),
    )
    .await;

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a changed certificate must be refused"
    );
    assert!(stderr.contains(&fingerprint(&old_cert)), "stderr: {stderr}");
    assert!(stderr.contains(&fingerprint(&new_cert)), "stderr: {stderr}");
    assert!(
        stderr.contains("envelope accounts tls --account you@proton.me --trust-cert"),
        "stderr: {stderr}"
    );
    assert!(
        server.transcript.tls_verbs().is_empty(),
        "no LOGIN was sent"
    );
    assert_eq!(
        listed(home.path()),
        Value::Array(vec![]),
        "nothing is saved"
    );

    // Saved offline with the old certificate, then trusted again.
    let mut offline = add_args(1, &old_file);
    offline.push("--skip-login-check".into());
    let out = run(home.path(), offline, Some("bridge-password")).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = envelope(
        home.path(),
        &[
            "--json",
            "accounts",
            "tls",
            "--account",
            "you@proton.me",
            "--trust-cert",
            new_file.to_str().unwrap(),
        ],
        None,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let shown: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(shown["pinned_cert_sha256"], fingerprint(&new_cert));
    assert_eq!(shown["imap_security"], "STARTTLS");
    assert_eq!(
        listed(home.path())[0]["tls"]["pinned_cert_sha256"],
        fingerprint(&new_cert)
    );
}

#[test]
fn trust_cert_refuses_a_file_without_a_certificate() {
    let home = tempfile::tempdir().unwrap();
    let key_file = home.path().join("key.pem");
    std::fs::write(&key_file, KeyPair::generate().unwrap().serialize_pem()).unwrap();
    let args = add_args(1, &key_file);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    let out = envelope(home.path(), &args, Some("bridge-password"));

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("--trust-cert"), "stderr: {stderr}");
    assert!(stderr.contains("has no certificate"), "stderr: {stderr}");
}
