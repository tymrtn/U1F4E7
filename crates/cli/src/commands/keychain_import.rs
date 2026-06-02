// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result, bail};
use envelope_email_store::models::{Account, AccountWithCredentials};
use envelope_email_store::{CredentialBackend, Database, credential_store};
use serde::Serialize;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusCode {
    FoundCandidate,
    NoCandidate,
    OauthOrTokenOnly,
    AuthVerified,
    AuthFailed,
    Imported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MailProtocol {
    Imap,
    Smtp,
}

impl MailProtocol {
    fn security_value(self) -> &'static str {
        match self {
            Self::Imap => "imap",
            Self::Smtp => "smtp",
        }
    }

    fn default_host(self, email: &str) -> Result<String> {
        let domain = email
            .split('@')
            .nth(1)
            .context("invalid email address — missing @")?;
        Ok(match self {
            Self::Imap => format!("imap.{domain}"),
            Self::Smtp => format!("smtp.{domain}"),
        })
    }

    fn default_port(self) -> u16 {
        match self {
            Self::Imap => 993,
            Self::Smtp => 587,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct KeychainCandidate {
    pub protocol: MailProtocol,
    pub server: String,
    pub account: String,
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keychain: Option<String>,
    #[serde(rename = "credential_readable")]
    pub secret_available: bool,
}

#[derive(Debug, Serialize)]
pub struct ImportStatus {
    pub status: StatusCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<KeychainCandidate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imap_verified: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smtp_verified: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl ImportStatus {
    pub fn found_candidate(candidates: Vec<KeychainCandidate>) -> Self {
        Self {
            status: StatusCode::FoundCandidate,
            message: Some(
                "metadata-only discovery; rerun with --confirm-read to verify credentials"
                    .to_string(),
            ),
            candidates,
            imap_verified: None,
            smtp_verified: None,
            account_id: None,
        }
    }

    fn simple(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: Some(message.into()),
            candidates: Vec::new(),
            imap_verified: None,
            smtp_verified: None,
            account_id: None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    email: &str,
    name: Option<String>,
    imap_host: Option<String>,
    smtp_host: Option<String>,
    imap_port: Option<u16>,
    smtp_port: Option<u16>,
    confirm_read: bool,
    import: bool,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let status = run_import(ImportRequest {
        email,
        name: name.as_deref(),
        imap_host: imap_host.as_deref(),
        smtp_host: smtp_host.as_deref(),
        imap_port,
        smtp_port,
        confirm_read,
        import,
        backend,
    })?;
    print_status(&status, json)
}

struct ImportRequest<'a> {
    email: &'a str,
    name: Option<&'a str>,
    imap_host: Option<&'a str>,
    smtp_host: Option<&'a str>,
    imap_port: Option<u16>,
    smtp_port: Option<u16>,
    confirm_read: bool,
    import: bool,
    backend: CredentialBackend,
}

#[tokio::main]
async fn run_import(req: ImportRequest<'_>) -> Result<ImportStatus> {
    let imap_hosts = candidate_hosts(req.email, req.imap_host, MailProtocol::Imap)?;
    let smtp_hosts = candidate_hosts(req.email, req.smtp_host, MailProtocol::Smtp)?;

    let mut candidates = Vec::new();
    for host in &imap_hosts {
        let args = security_find_internet_password_args(req.email, host, MailProtocol::Imap, false);
        if let Some(item) = discover_candidate_with_args(&args, MailProtocol::Imap)? {
            candidates.push(item);
        }
    }
    for host in &smtp_hosts {
        let args = security_find_internet_password_args(req.email, host, MailProtocol::Smtp, false);
        if let Some(item) = discover_candidate_with_args(&args, MailProtocol::Smtp)? {
            candidates.push(item);
        }
    }

    let discovery = classify_discovery(req.email, &candidates);
    if !matches!(discovery.status, StatusCode::FoundCandidate) || !req.confirm_read {
        return Ok(discovery);
    }

    let imap_candidate = candidates.iter().find(|c| c.protocol == MailProtocol::Imap);
    let smtp_candidate = candidates.iter().find(|c| c.protocol == MailProtocol::Smtp);
    let (Some(imap_candidate), Some(smtp_candidate)) = (imap_candidate, smtp_candidate) else {
        return Ok(ImportStatus {
            status: StatusCode::AuthFailed,
            message: Some(
                "both IMAP and SMTP internet-password candidates are required before reading or verifying credentials"
                    .into(),
            ),
            candidates,
            imap_verified: Some(false),
            smtp_verified: Some(false),
            account_id: None,
        });
    };
    let imap_host = imap_candidate.server.clone();
    let smtp_host = smtp_candidate.server.clone();
    let imap_port = req
        .imap_port
        .or(imap_candidate.port)
        .unwrap_or(MailProtocol::Imap.default_port());
    let smtp_port = req
        .smtp_port
        .or(smtp_candidate.port)
        .unwrap_or(MailProtocol::Smtp.default_port());

    let imap_password = match read_keychain_password(req.email, &imap_host, MailProtocol::Imap) {
        Ok(password) => password,
        Err(_) => {
            return Ok(ImportStatus {
                status: StatusCode::AuthFailed,
                message: Some("IMAP keychain credential could not be read".into()),
                candidates,
                imap_verified: Some(false),
                smtp_verified: None,
                account_id: None,
            });
        }
    };
    let smtp_password = match read_keychain_password(req.email, &smtp_host, MailProtocol::Smtp) {
        Ok(password) => password,
        Err(_) => {
            return Ok(ImportStatus {
                status: StatusCode::AuthFailed,
                message: Some("SMTP keychain credential could not be read".into()),
                candidates,
                imap_verified: None,
                smtp_verified: Some(false),
                account_id: None,
            });
        }
    };

    let temp_account = account_with_credentials(
        req.email,
        &imap_host,
        imap_port,
        &smtp_host,
        smtp_port,
        &imap_password,
        &smtp_password,
    );

    let imap_verified = verify_imap(&temp_account).await.is_ok();
    let smtp_verified = verify_smtp(&temp_account).await.is_ok();

    if !imap_verified || !smtp_verified {
        return Ok(ImportStatus {
            status: StatusCode::AuthFailed,
            message: Some(
                "keychain credential read succeeded, but IMAP/SMTP auth did not verify".into(),
            ),
            candidates,
            imap_verified: Some(imap_verified),
            smtp_verified: Some(smtp_verified),
            account_id: None,
        });
    }

    if !req.import {
        return Ok(ImportStatus {
            status: StatusCode::AuthVerified,
            message: Some(
                "credentials verified; rerun with --import to store/update Envelope".into(),
            ),
            candidates,
            imap_verified: Some(true),
            smtp_verified: Some(true),
            account_id: None,
        });
    }

    let passphrase = credential_store::get_or_create_passphrase(req.backend)
        .context("failed to access Envelope credential store")?;
    let db = Database::open_default().context("failed to open database")?;
    let account = db.upsert_account_credentials(
        req.name.unwrap_or(req.email),
        req.email,
        &imap_password,
        Some(&smtp_password),
        &smtp_host,
        smtp_port,
        &imap_host,
        imap_port,
        &passphrase,
    )?;

    Ok(ImportStatus {
        status: StatusCode::Imported,
        message: Some("Envelope account stored/updated from verified keychain credentials".into()),
        candidates,
        imap_verified: Some(true),
        smtp_verified: Some(true),
        account_id: Some(account.id),
    })
}

fn print_status(status: &ImportStatus, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(status)?);
        return Ok(());
    }

    println!("status: {:?}", status.status);
    if let Some(message) = &status.message {
        println!("{message}");
    }
    for candidate in &status.candidates {
        println!(
            "candidate: {:?} {} account={} port={}",
            candidate.protocol,
            candidate.server,
            candidate.account,
            candidate
                .port
                .map(|p| p.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        );
    }
    if let Some(id) = &status.account_id {
        println!("account_id: {id}");
    }
    Ok(())
}

fn classify_discovery(email: &str, candidates: &[KeychainCandidate]) -> ImportStatus {
    if !candidates.is_empty() {
        return ImportStatus::found_candidate(candidates.to_vec());
    }

    let domain = email
        .rsplit_once('@')
        .map(|(_, domain)| domain.to_lowercase());
    if matches!(
        domain.as_deref(),
        Some("gmail.com" | "googlemail.com" | "icloud.com" | "me.com" | "mac.com")
    ) {
        return ImportStatus::simple(
            StatusCode::OauthOrTokenOnly,
            "no internet-password candidate found; Mail.app may be using OAuth/token storage for this provider. Use an app password if IMAP/SMTP password auth is available.",
        );
    }

    ImportStatus::simple(
        StatusCode::NoCandidate,
        "no matching internet-password candidate found",
    )
}

fn candidate_hosts(
    email: &str,
    override_host: Option<&str>,
    protocol: MailProtocol,
) -> Result<Vec<String>> {
    if let Some(host) = override_host {
        return Ok(vec![host.to_string()]);
    }

    let mut hosts = vec![protocol.default_host(email)?];
    let migadu = match protocol {
        MailProtocol::Imap => "imap.migadu.com",
        MailProtocol::Smtp => "smtp.migadu.com",
    };
    if !hosts.iter().any(|host| host == migadu) {
        hosts.push(migadu.to_string());
    }
    Ok(hosts)
}

fn security_find_internet_password_args(
    email: &str,
    server: &str,
    protocol: MailProtocol,
    read_password: bool,
) -> Vec<String> {
    let mut args = vec!["find-internet-password".to_string()];
    if read_password {
        args.push("-w".to_string());
    }
    args.extend([
        "-a".to_string(),
        email.to_string(),
        "-s".to_string(),
        server.to_string(),
        "-r".to_string(),
        protocol.security_value().to_string(),
    ]);
    args
}

fn discover_candidate_with_args(
    args: &[String],
    protocol: MailProtocol,
) -> Result<Option<KeychainCandidate>> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("security")
            .args(args)
            .output()
            .with_context(|| "failed to execute macOS security tool")?;

        if !output.status.success() {
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(parse_security_metadata(&text, protocol))
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (args, protocol);
        Ok(None)
    }
}

fn parse_security_metadata(output: &str, protocol: MailProtocol) -> Option<KeychainCandidate> {
    let account = parse_quoted_attr(output, "acct")?;
    let server = parse_quoted_attr(output, "srvr")?;
    let port = parse_port(output);
    let keychain = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("keychain: "))
        .map(trim_security_value);

    Some(KeychainCandidate {
        protocol,
        server,
        account,
        port,
        keychain,
        secret_available: false,
    })
}

fn parse_quoted_attr(output: &str, name: &str) -> Option<String> {
    let needle = format!("\"{name}\"");
    output.lines().find_map(|line| {
        let line = line.trim();
        if !line.contains(&needle) {
            return None;
        }
        line.rsplit_once('=')
            .map(|(_, value)| trim_security_value(value))
    })
}

fn parse_port(output: &str) -> Option<u16> {
    parse_quoted_attr(output, "port").and_then(|value| {
        if let Some(hex) = value.strip_prefix("0x") {
            u16::from_str_radix(hex, 16).ok()
        } else {
            value.parse::<u16>().ok()
        }
    })
}

fn trim_security_value(value: &str) -> String {
    let value = value.trim();
    if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn read_keychain_password(email: &str, server: &str, protocol: MailProtocol) -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        let args = security_find_internet_password_args(email, server, protocol, true);
        let output = Command::new("security")
            .args(&args)
            .output()
            .with_context(|| "failed to execute macOS security tool")?;
        if !output.status.success() {
            bail!("no readable keychain password for {email} on {server}");
        }
        let password = String::from_utf8(output.stdout)
            .context("security output was not valid utf-8")?
            .trim_end_matches(['\r', '\n'])
            .to_string();
        if password.is_empty() {
            bail!("empty keychain password for {email} on {server}");
        }
        Ok(password)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (email, server, protocol);
        bail!("keychain import is only supported on macOS")
    }
}

fn account_with_credentials(
    email: &str,
    imap_host: &str,
    imap_port: u16,
    smtp_host: &str,
    smtp_port: u16,
    imap_password: &str,
    smtp_password: &str,
) -> AccountWithCredentials {
    let domain = email
        .rsplit_once('@')
        .map(|(_, domain)| domain.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    AccountWithCredentials {
        account: Account {
            id: "keychain-import-probe".to_string(),
            name: email.to_string(),
            username: email.to_string(),
            domain,
            smtp_host: smtp_host.to_string(),
            smtp_port,
            imap_host: imap_host.to_string(),
            imap_port,
            smtp_username: None,
            imap_username: None,
            display_name: None,
            signature_text: None,
            signature_html: None,
            created_at: String::new(),
        },
        password: imap_password.to_string(),
        smtp_password: Some(smtp_password.to_string()),
        imap_password: None,
    }
}

async fn verify_imap(account: &AccountWithCredentials) -> Result<()> {
    let mut client = envelope_email_transport::imap::connect(account).await?;
    let _folders = envelope_email_transport::imap::list_folders(&mut client).await?;
    Ok(())
}

async fn verify_smtp(account: &AccountWithCredentials) -> Result<()> {
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{AsyncSmtpTransport, Tokio1Executor};

    let smtp_host = &account.account.smtp_host;
    let smtp_port = account.account.smtp_port;
    let creds = Credentials::new(
        account.effective_smtp_username().to_string(),
        account.effective_smtp_password().to_string(),
    );
    let transport: AsyncSmtpTransport<Tokio1Executor> = match smtp_port {
        465 => AsyncSmtpTransport::<Tokio1Executor>::relay(smtp_host)?,
        _ => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(smtp_host)?,
    }
    .port(smtp_port)
    .credentials(creds)
    .build();
    transport.test_connection().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_SENTINEL: &str = "fixture-sensitive-value";

    #[test]
    fn parses_security_metadata_without_secret_values() {
        let output = r#"keychain: "/Users/example/Library/Keychains/login.keychain-db"
class: "inet"
attributes:
    "acct"<blob>="user@example.com"
    "ptcl"<uint32>="imap"
    "srvr"<blob>="imap.migadu.com"
    "port"<uint32>=0x000003e1
"#;

        let item = parse_security_metadata(output, MailProtocol::Imap).unwrap();

        assert_eq!(item.account, "user@example.com");
        assert_eq!(item.server, "imap.migadu.com");
        assert_eq!(item.protocol, MailProtocol::Imap);
        assert_eq!(item.port, Some(993));
        assert!(
            !serde_json::to_string(&item)
                .unwrap()
                .contains(FIXTURE_SENTINEL)
        );
    }

    #[test]
    fn redacts_secret_like_text_from_status_json() {
        let status = ImportStatus::found_candidate(vec![KeychainCandidate {
            protocol: MailProtocol::Smtp,
            server: "smtp.migadu.com".to_string(),
            account: "user@example.com".to_string(),
            port: Some(587),
            keychain: Some("/Users/example/Library/Keychains/login.keychain-db".to_string()),
            secret_available: false,
        }]);

        let json = serde_json::to_string_pretty(&status).unwrap();

        assert!(json.contains("found_candidate"));
        assert!(!json.contains("password"));
        assert!(!json.contains("secret"));
        assert!(!json.contains(FIXTURE_SENTINEL));
    }

    #[test]
    fn includes_migadu_hosts_by_default_for_custom_domains() {
        let imap_hosts = candidate_hosts("user@custom.example", None, MailProtocol::Imap).unwrap();
        let smtp_hosts = candidate_hosts("user@custom.example", None, MailProtocol::Smtp).unwrap();

        assert!(imap_hosts.contains(&"imap.custom.example".to_string()));
        assert!(imap_hosts.contains(&"imap.migadu.com".to_string()));
        assert!(smtp_hosts.contains(&"smtp.custom.example".to_string()));
        assert!(smtp_hosts.contains(&"smtp.migadu.com".to_string()));
    }

    #[test]
    fn override_host_disables_guessing() {
        let hosts = candidate_hosts(
            "user@custom.example",
            Some("mail.example.net"),
            MailProtocol::Imap,
        )
        .unwrap();

        assert_eq!(hosts, vec!["mail.example.net".to_string()]);
    }

    #[test]
    fn security_args_only_read_password_when_confirmed() {
        let discover_args = security_find_internet_password_args(
            "user@example.com",
            "imap.example.com",
            MailProtocol::Imap,
            false,
        );
        let read_args = security_find_internet_password_args(
            "user@example.com",
            "imap.example.com",
            MailProtocol::Imap,
            true,
        );

        assert!(!discover_args.contains(&"-w".to_string()));
        assert!(read_args.contains(&"-w".to_string()));
        assert!(
            discover_args
                .windows(2)
                .any(|pair| pair == ["-a", "user@example.com"])
        );
        assert!(
            discover_args
                .windows(2)
                .any(|pair| pair == ["-s", "imap.example.com"])
        );
        assert!(discover_args.windows(2).any(|pair| pair == ["-r", "imap"]));
    }

    #[test]
    fn auth_failed_status_json_does_not_include_secret_material() {
        let status = ImportStatus {
            status: StatusCode::AuthFailed,
            message: Some("auth failed without echoing credential".to_string()),
            candidates: vec![KeychainCandidate {
                protocol: MailProtocol::Imap,
                server: "imap.example.com".to_string(),
                account: "user@example.com".to_string(),
                port: Some(993),
                keychain: None,
                secret_available: false,
            }],
            imap_verified: Some(false),
            smtp_verified: Some(false),
            account_id: None,
        };

        let json = serde_json::to_string_pretty(&status).unwrap();
        assert!(!json.contains(FIXTURE_SENTINEL));
        assert!(!json.contains("password"));
    }

    #[test]
    fn classifies_google_oauth_without_password_candidate_as_token_only() {
        let status = classify_discovery("user@gmail.com", &[]);

        assert_eq!(status.status, StatusCode::OauthOrTokenOnly);
        assert!(status.message.unwrap().contains("app password"));
    }

    #[test]
    fn no_candidate_for_non_oauth_domains() {
        let status = classify_discovery("user@example.com", &[]);

        assert_eq!(status.status, StatusCode::NoCandidate);
    }
}
