// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::time::Instant;

use anyhow::Result;
use envelope_email_store::credential_store::{self, CredentialBackend};
use envelope_email_store::{Account, Database};
use serde::Serialize;

use super::paths;

pub const SCHEMA: &str = "envelope.quickstart.v1";
pub const MAX_PEEK_LIMIT: u32 = 25;
pub const MAX_TIMEOUT_SECS: u64 = 60;

const NEXT_STEPS: [&str; 4] = [
    "envelope mcp --config",
    "Paste the Claude Code, Codex, or Hermes snippet from envelopeAgentSetup; HOME/env and command path are included.",
    "Agent send/reply tools default to draft-only; review drafts before sending.",
    "envelope inbox --limit 10",
];

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PhaseName {
    Paths,
    Account,
    ImapAuth,
    InboxPeek,
}

impl PhaseName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Paths => "paths",
            Self::Account => "account",
            Self::ImapAuth => "imap_auth",
            Self::InboxPeek => "inbox_peek",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum PhaseStatus {
    Ok,
    Skipped,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct QuickstartError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remediation: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct QuickstartPhase {
    pub name: PhaseName,
    pub status: PhaseStatus,
    pub elapsed_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<QuickstartError>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct QuickstartReport {
    pub schema: &'static str,
    pub ok: bool,
    pub elapsed_ms: u128,
    pub failed_phase: Option<PhaseName>,
    pub phases: Vec<QuickstartPhase>,
    pub next_steps: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct QuickstartOptions<'a> {
    pub account: Option<&'a str>,
    pub folder: &'a str,
    pub peek_limit: u32,
    pub timeout_secs: u64,
    pub skip_network: bool,
    pub backend: CredentialBackend,
}

pub fn run(
    json: bool,
    account: Option<&str>,
    folder: &str,
    peek_limit: u32,
    timeout_secs: u64,
    skip_network: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let options = QuickstartOptions {
        account,
        folder,
        peek_limit: peek_limit.min(MAX_PEEK_LIMIT),
        timeout_secs: timeout_secs.min(MAX_TIMEOUT_SECS),
        skip_network,
        backend,
    };

    let report = run_report(&options);
    let exit = exit_code(&report);

    if json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        print_text(&report);
    }

    if exit != 0 {
        std::process::exit(exit);
    }
    Ok(())
}

pub fn run_report(options: &QuickstartOptions<'_>) -> QuickstartReport {
    let started = Instant::now();
    let mut phases = Vec::new();
    let mut failed_phase = None;

    let paths_phase = phase_paths(options.backend);
    if paths_phase.status == PhaseStatus::Error {
        failed_phase = Some(PhaseName::Paths);
        phases.push(paths_phase);
        return report(started, phases, failed_phase);
    }
    phases.push(paths_phase);

    let account_result = phase_account(options.account);
    let account = match account_result {
        Ok((phase, account)) => {
            phases.push(phase);
            account
        }
        Err(phase) => {
            failed_phase = Some(PhaseName::Account);
            phases.push(phase);
            return report(started, phases, failed_phase);
        }
    };

    if options.skip_network {
        return report(started, phases, failed_phase);
    }

    let network = run_network_phases(options, &account);
    for phase in network {
        if phase.status == PhaseStatus::Error && failed_phase.is_none() {
            failed_phase = Some(phase.name);
            phases.push(phase);
            break;
        }
        phases.push(phase);
    }

    report(started, phases, failed_phase)
}

fn report(
    started: Instant,
    phases: Vec<QuickstartPhase>,
    failed_phase: Option<PhaseName>,
) -> QuickstartReport {
    let ok = failed_phase.is_none();
    QuickstartReport {
        schema: SCHEMA,
        ok,
        elapsed_ms: started.elapsed().as_millis(),
        failed_phase,
        phases,
        next_steps: if ok {
            NEXT_STEPS.iter().map(|s| s.to_string()).collect()
        } else {
            Vec::new()
        },
    }
}

fn phase_paths(backend: CredentialBackend) -> QuickstartPhase {
    let started = Instant::now();
    let report = paths::collect_report(backend);
    let (status, error) = if report.home.is_none() {
        (
            PhaseStatus::Error,
            Some(QuickstartError {
                code: "home_missing".to_string(),
                message: "HOME is not set; Envelope cannot resolve its local state paths."
                    .to_string(),
                remediation: vec![
                    "Run with HOME set to the user profile that owns Envelope state.".to_string(),
                ],
            }),
        )
    } else if report.warnings.is_empty() {
        (PhaseStatus::Ok, None)
    } else {
        (PhaseStatus::Warn, None)
    };

    QuickstartPhase {
        name: PhaseName::Paths,
        status,
        elapsed_ms: started.elapsed().as_millis(),
        details: Some(serde_json::to_value(report).expect("paths report serializes")),
        error,
    }
}

fn phase_account(
    account_arg: Option<&str>,
) -> std::result::Result<(QuickstartPhase, Account), QuickstartPhase> {
    let started = Instant::now();
    let db = match Database::open_default_readonly_existing() {
        Ok(Some(db)) => db,
        Ok(None) if account_arg.is_some() => {
            return Err(error_phase(
                PhaseName::Account,
                started,
                "account_not_found",
                "No configured account database exists in Envelope HOME.".to_string(),
                vec!["Run `envelope accounts list` to choose a configured account.".to_string()],
            ));
        }
        Ok(None) => {
            return Err(error_phase(
                PhaseName::Account,
                started,
                "no_account_configured",
                "No accounts found in shared Envelope HOME.".to_string(),
                account_remediation(),
            ));
        }
        Err(e) => {
            return Err(error_phase(
                PhaseName::Account,
                started,
                "account_store_error",
                sanitize_error(e.to_string()),
                vec![],
            ));
        }
    };

    let source = if account_arg.is_some() {
        "explicit"
    } else {
        "default"
    };
    let account = match account_arg {
        Some(id_or_email) => db
            .get_account(id_or_email)
            .ok()
            .flatten()
            .or_else(|| db.find_account_by_email(id_or_email).ok().flatten())
            .ok_or_else(|| {
                error_phase(
                    PhaseName::Account,
                    started,
                    "account_not_found",
                    format!("No configured account matched `{}`.", redact(id_or_email)),
                    vec![
                        "Run `envelope accounts list` to choose a configured account.".to_string(),
                    ],
                )
            })?,
        None => db
            .default_account()
            .map_err(|e| {
                error_phase(
                    PhaseName::Account,
                    started,
                    "account_store_error",
                    sanitize_error(e.to_string()),
                    vec![],
                )
            })?
            .ok_or_else(|| {
                error_phase(
                    PhaseName::Account,
                    started,
                    "no_account_configured",
                    "No accounts found in shared Envelope HOME.".to_string(),
                    account_remediation(),
                )
            })?,
    };

    let details = serde_json::json!({
        "id": account.id,
        "email": account.username,
        "imap_host": account.imap_host,
        "imap_port": account.imap_port,
        "smtp_host": account.smtp_host,
        "smtp_port": account.smtp_port,
        "source": source,
    });
    Ok((ok_phase(PhaseName::Account, started, details), account))
}

fn run_network_phases(options: &QuickstartOptions<'_>, account: &Account) -> Vec<QuickstartPhase> {
    let timeout = std::time::Duration::from_secs(options.timeout_secs);
    let account_id = account.id.clone();
    let folder = options.folder.to_string();
    let peek_limit = options.peek_limit.min(MAX_PEEK_LIMIT);
    let backend = options.backend;
    let account_snapshot = account.clone();

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            return vec![error_phase(
                PhaseName::ImapAuth,
                Instant::now(),
                "imap_auth_failed",
                sanitize_error(e.to_string()),
                vec![],
            )];
        }
    };

    rt.block_on(async move {
        let auth_started = Instant::now();
        let db = match Database::open_default_readonly_existing() {
            Ok(Some(db)) => db,
            Ok(None) => return vec![error_phase(PhaseName::ImapAuth, auth_started, "imap_auth_failed", "No configured account database exists in Envelope HOME.".to_string(), vec![])],
            Err(e) => return vec![error_phase(PhaseName::ImapAuth, auth_started, "imap_auth_failed", sanitize_error(e.to_string()), vec![])],
        };
        let passphrase = match credential_store::get_passphrase(backend) {
            Ok(p) => p,
            Err(e) => return vec![error_phase(PhaseName::ImapAuth, auth_started, "imap_auth_failed", sanitize_error(e.to_string()), vec!["Verify the configured credential store contains the Envelope master key.".to_string()])],
        };
        let creds = match db.get_account_with_credentials(&account_id, &passphrase) {
            Ok(c) => c,
            Err(e) => return vec![error_phase(PhaseName::ImapAuth, auth_started, "imap_auth_failed", sanitize_error(e.to_string()), vec![])],
        };

        let connect = tokio::time::timeout(timeout, envelope_email_transport::imap::connect(&creds)).await;
        let mut client = match connect {
            Ok(Ok(client)) => client,
            Ok(Err(e)) => return vec![classify_imap_error(PhaseName::ImapAuth, auth_started, &account_snapshot.username, e)],
            Err(_) => return vec![error_phase(PhaseName::ImapAuth, auth_started, "imap_timeout", "IMAP authentication timed out.".to_string(), vec![])],
        };

        let auth_phase = ok_phase(
            PhaseName::ImapAuth,
            auth_started,
            serde_json::json!({
                "host": account_snapshot.imap_host,
                "port": account_snapshot.imap_port,
                "tls": true,
            }),
        );

        let peek_started = Instant::now();
        let peek = tokio::time::timeout(
            timeout,
            envelope_email_transport::imap::peek_folder_headers_read_only(&mut client, &folder, peek_limit),
        )
        .await;
        let peek_phase = match peek {
            Ok(Ok(messages)) => {
                let newest = messages.first();
                ok_phase(
                    PhaseName::InboxPeek,
                    peek_started,
                    serde_json::json!({
                        "folder": folder,
                        "message_count": messages.len(),
                        "newest_date": newest.and_then(|m| m.date.clone()),
                        "newest_from_domain": newest.and_then(|m| from_domain(m.from_addr.as_deref().unwrap_or_default())),
                    }),
                )
            }
            Ok(Err(e)) => error_phase(PhaseName::InboxPeek, peek_started, "inbox_peek_failed", sanitize_error(e.to_string()), vec![]),
            Err(_) => error_phase(PhaseName::InboxPeek, peek_started, "inbox_peek_failed", "IMAP inbox peek timed out.".to_string(), vec![]),
        };
        vec![auth_phase, peek_phase]
    })
}

fn ok_phase(name: PhaseName, started: Instant, details: serde_json::Value) -> QuickstartPhase {
    QuickstartPhase {
        name,
        status: PhaseStatus::Ok,
        elapsed_ms: started.elapsed().as_millis(),
        details: Some(details),
        error: None,
    }
}

fn error_phase(
    name: PhaseName,
    started: Instant,
    code: &str,
    message: String,
    remediation: Vec<String>,
) -> QuickstartPhase {
    QuickstartPhase {
        name,
        status: PhaseStatus::Error,
        elapsed_ms: started.elapsed().as_millis(),
        details: None,
        error: Some(QuickstartError {
            code: code.to_string(),
            message,
            remediation,
        }),
    }
}

fn classify_imap_error(
    name: PhaseName,
    started: Instant,
    account_email: &str,
    err: envelope_email_transport::errors::ImapError,
) -> QuickstartPhase {
    let text = sanitize_error(err.to_string());
    let (code, remediation) = match &err {
        envelope_email_transport::errors::ImapError::Auth(raw) => (
            "imap_auth_failed",
            auth_remediation(Some(account_email), raw),
        ),
        envelope_email_transport::errors::ImapError::Connection(_)
            if text.to_lowercase().contains("tls") =>
        {
            ("imap_tls_failed", vec![])
        }
        envelope_email_transport::errors::ImapError::Connection(_)
            if text.to_lowercase().contains("dns") =>
        {
            ("imap_dns_failed", vec![])
        }
        envelope_email_transport::errors::ImapError::Connection(_) => ("imap_dns_failed", vec![]),
        envelope_email_transport::errors::ImapError::Protocol(_)
        | envelope_email_transport::errors::ImapError::NotFound(_) => ("inbox_peek_failed", vec![]),
    };
    error_phase(name, started, code, text, remediation)
}

/// Providers the README provider table lists as working with a password or
/// app password. Kept to that table so remediation never promises more.
const WORKING_PROVIDERS: &str = "Providers that work today with a password or app password: Fastmail, iCloud Mail, Migadu, self-hosted Dovecot, and most IMAP hosts (see the provider table in the README).";

/// Outlook.com-family domains, including country variants (hotmail.co.uk).
/// Microsoft 365 mailboxes on custom domains are caught by the server text.
fn is_microsoft_consumer_domain(domain: &str) -> bool {
    let label = domain.split('.').next().unwrap_or_default();
    matches!(label, "outlook" | "hotmail" | "live" | "msn")
}

/// Provider-specific guidance for an IMAP authentication failure.
///
/// `account` is the account email (or bare domain); `server_text` is the IMAP
/// server's error. The server text catches cases the domain cannot, such as a
/// Google Workspace mailbox on a custom domain.
pub fn auth_remediation(account: Option<&str>, server_text: &str) -> Vec<String> {
    let domain = account
        .map(|a| a.rsplit_once('@').map(|(_, d)| d).unwrap_or(a))
        .unwrap_or("")
        .to_lowercase();
    let text = server_text.to_lowercase();

    if is_microsoft_consumer_domain(&domain)
        || text.contains("authentication mechanism is not supported")
    {
        return vec![
            "Microsoft mailboxes (Outlook.com, Hotmail, Live, MSN, and Microsoft 365) only accept Microsoft's OAuth sign-in over IMAP, and Envelope does not support OAuth sign-in yet. These accounts cannot be added today, with or without an app password.".to_string(),
            WORKING_PROVIDERS.to_string(),
        ];
    }
    if domain == "gmail.com"
        || domain == "googlemail.com"
        || text.contains("application-specific password required")
    {
        return vec![
            "Envelope does not support Google sign-in (OAuth) yet, so Gmail needs an app password, not your Google account password.".to_string(),
            "1. Turn on 2-Step Verification: https://myaccount.google.com/security".to_string(),
            "2. Create an app password: https://myaccount.google.com/apppasswords".to_string(),
            "3. Re-run: envelope accounts add --email you@gmail.com".to_string(),
            "If Google won't let you create an app password for your account, Gmail can't connect to Envelope yet (OAuth sign-in isn't supported).".to_string(),
            WORKING_PROVIDERS.to_string(),
        ];
    }
    match domain.as_str() {
        d if d == "fastmail.com" || d.ends_with(".fastmail.com") => vec![
            "Fastmail requires an app password, not your Fastmail login password.".to_string(),
            "Create one at: https://app.fastmail.com/settings/security/devicekeys".to_string(),
            "Re-run: envelope accounts add --email you@fastmail.com".to_string(),
        ],
        d if d == "icloud.com" || d == "me.com" || d == "mac.com" => vec![
            "iCloud Mail requires an app-specific password, not your Apple ID password.".to_string(),
            "Create one at: https://appleid.apple.com (Sign-In and Security → App-Specific Passwords).".to_string(),
            "2FA must be enabled on your Apple ID first.".to_string(),
            "Re-run: envelope accounts add --email you@icloud.com".to_string(),
        ],
        _ => vec![
            "Check the password. Many providers require an app password when two-factor sign-in is on.".to_string(),
            "Generate one in your provider's security settings, then re-run:".to_string(),
            "  envelope accounts add --email you@example.com".to_string(),
        ],
    }
}

pub fn exit_code(report: &QuickstartReport) -> i32 {
    match report.failed_phase {
        None => 0,
        Some(PhaseName::Paths) => 1,
        Some(PhaseName::Account) => 2,
        Some(PhaseName::ImapAuth) => 3,
        Some(PhaseName::InboxPeek) => 4,
    }
}

fn print_text(report: &QuickstartReport) {
    println!("Envelope quickstart");
    println!("─────────────────────────");
    for (idx, phase) in report.phases.iter().enumerate() {
        let status = match phase.status {
            PhaseStatus::Ok => "ok",
            PhaseStatus::Skipped => "skipped",
            PhaseStatus::Warn => "warn",
            PhaseStatus::Error => "error",
        };
        let detail = text_detail(phase);
        println!(
            "[{}/4] {:<10} {:<7} {}",
            idx + 1,
            phase.name.as_str(),
            status,
            detail
        );
    }
    if report.ok {
        println!("\nReady. Try:");
        for step in &report.next_steps {
            println!("  {step}");
        }
    } else if let Some(phase) = report
        .phases
        .iter()
        .find(|p| p.status == PhaseStatus::Error)
    {
        if let Some(error) = &phase.error {
            println!("\n{}", error.message);
            if !error.remediation.is_empty() {
                println!("Choose one path:");
                for item in &error.remediation {
                    println!("  {item}");
                }
                println!("Re-run `envelope quickstart` after resolving this phase.");
            }
        }
    }
}

fn text_detail(phase: &QuickstartPhase) -> String {
    let Some(details) = &phase.details else {
        return phase
            .error
            .as_ref()
            .map(|e| e.code.clone())
            .unwrap_or_default();
    };
    match phase.name {
        PhaseName::Paths => details
            .get("app_data_dir")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        PhaseName::Account => {
            let email = details
                .get("email")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let id = details
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            format!("{email}  (id {id})")
        }
        PhaseName::ImapAuth => {
            let host = details
                .get("host")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let port = details
                .get("port")
                .and_then(|v| v.as_u64())
                .unwrap_or_default();
            format!("{host}:{port}")
        }
        PhaseName::InboxPeek => {
            let count = details
                .get("message_count")
                .and_then(|v| v.as_u64())
                .unwrap_or_default();
            let date = details
                .get("newest_date")
                .and_then(|v| v.as_str())
                .unwrap_or("none");
            let domain = details
                .get("newest_from_domain")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            format!("{count} messages, newest {date} from {domain}")
        }
    }
}

fn account_remediation() -> Vec<String> {
    vec![
        "envelope accounts import-keychain --email you@example.com --confirm-read --import"
            .to_string(),
        "envelope accounts add --email you@example.com".to_string(),
    ]
}

fn from_domain(from: &str) -> Option<String> {
    let addr = from
        .rsplit('<')
        .next()
        .unwrap_or(from)
        .trim_end_matches('>')
        .trim();
    addr.rsplit_once('@').map(|(_, d)| d.trim().to_lowercase())
}

fn redact(value: &str) -> String {
    if let Some((prefix, domain)) = value.split_once('@') {
        let first = prefix.chars().next().unwrap_or('*');
        format!("{first}***@{domain}")
    } else {
        value.to_string()
    }
}

pub fn sanitize_error(message: String) -> String {
    let lower = message.to_lowercase();
    if lower.contains("password")
        || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("hunter2")
    {
        "Operation failed; sensitive details redacted.".to_string()
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use envelope_email_store::credential_store::CredentialBackend;

    #[test]
    fn schema_field_pinned() {
        let report = QuickstartReport {
            schema: SCHEMA,
            ok: true,
            elapsed_ms: 0,
            failed_phase: None,
            phases: vec![],
            next_steps: vec![],
        };
        assert_eq!(
            serde_json::to_value(report).unwrap()["schema"],
            "envelope.quickstart.v1"
        );
    }

    #[test]
    fn phases_serialize_in_order() {
        let names: Vec<_> = [
            PhaseName::Paths,
            PhaseName::Account,
            PhaseName::ImapAuth,
            PhaseName::InboxPeek,
        ]
        .into_iter()
        .map(|p| serde_json::to_value(p).unwrap())
        .collect();
        assert_eq!(names, vec!["paths", "account", "imap_auth", "inbox_peek"]);
    }

    #[test]
    fn failure_short_circuits() {
        let phases = vec![error_phase(
            PhaseName::Account,
            Instant::now(),
            "no_account_configured",
            "No accounts found".to_string(),
            vec![],
        )];
        let report = report(Instant::now(), phases, Some(PhaseName::Account));
        assert!(!report.ok);
        assert_eq!(report.failed_phase, Some(PhaseName::Account));
        assert_eq!(report.phases.len(), 1);
        assert!(report.next_steps.is_empty());
    }

    #[test]
    fn success_emits_next_steps() {
        let report = report(Instant::now(), vec![], None);
        assert!(report.ok);
        assert!(!report.next_steps.is_empty());
    }

    #[test]
    fn error_redacts_secrets() {
        let sanitized = sanitize_error("imap rejected password=hunter2 token=abc".to_string());
        assert!(!sanitized.contains("hunter2"));
        assert!(!sanitized.contains("abc"));
    }

    #[test]
    fn exit_code_for_failed_phase() {
        for (phase, code) in [
            (PhaseName::Paths, 1),
            (PhaseName::Account, 2),
            (PhaseName::ImapAuth, 3),
            (PhaseName::InboxPeek, 4),
        ] {
            let report = QuickstartReport {
                schema: SCHEMA,
                ok: false,
                elapsed_ms: 0,
                failed_phase: Some(phase),
                phases: vec![],
                next_steps: vec![],
            };
            assert_eq!(exit_code(&report), code);
        }
    }

    #[test]
    fn auth_remediation_gmail_domain_present() {
        let joined = auth_remediation(Some("you@gmail.com"), "Invalid credentials").join(" ");
        assert!(joined.contains("myaccount.google.com/apppasswords"));
        assert!(joined.contains("OAuth"));
        assert!(joined.contains("Gmail can't connect to Envelope yet"));
    }

    #[test]
    fn auth_remediation_detects_google_workspace_by_server_text() {
        let joined = auth_remediation(
            Some("me@company.example"),
            "[ALERT] Application-specific password required: https://support.google.com/accounts/answer/185833 (Failure)",
        )
        .join(" ");
        assert!(joined.contains("myaccount.google.com/apppasswords"));
    }

    #[test]
    fn auth_remediation_unknown_domain_generic() {
        let joined = auth_remediation(Some("example.com"), "").join(" ");
        assert!(joined.contains("app password"));
    }

    #[test]
    fn auth_remediation_none_domain_generic() {
        assert!(!auth_remediation(None, "").is_empty());
    }

    #[test]
    fn auth_remediation_fastmail() {
        let joined = auth_remediation(Some("fastmail.com"), "").join(" ");
        assert!(joined.contains("fastmail.com/settings/security"));
    }

    #[test]
    fn auth_remediation_icloud() {
        let joined = auth_remediation(Some("you@icloud.com"), "").join(" ");
        assert!(joined.contains("appleid.apple.com"));
    }

    #[test]
    fn auth_remediation_outlook_says_oauth_is_unsupported() {
        for account in [
            "you@outlook.com",
            "you@hotmail.co.uk",
            "you@live.com",
            "you@msn.com",
        ] {
            let joined = auth_remediation(Some(account), "").join(" ");
            assert!(
                joined.contains("does not support OAuth"),
                "{account}: {joined}"
            );
            assert!(joined.contains("cannot be added today"), "{account}");
            assert!(
                !joined.contains("account.microsoft.com"),
                "no app-password promise"
            );
        }
    }

    #[test]
    fn auth_remediation_detects_microsoft_by_server_text() {
        let joined = auth_remediation(
            Some("me@custom.example"),
            "AUTHENTICATE failed. Provided authentication mechanism is not supported.",
        )
        .join(" ");
        assert!(joined.contains("does not support OAuth"));
    }

    #[test]
    fn classify_imap_error_uses_the_account_domain() {
        let phase = classify_imap_error(
            PhaseName::ImapAuth,
            Instant::now(),
            "probe@outlook.com",
            envelope_email_transport::errors::ImapError::Auth(
                "AUTHENTICATE failed. Provided authentication mechanism is not supported."
                    .to_string(),
            ),
        );
        let error = phase.error.unwrap();
        assert_eq!(error.code, "imap_auth_failed");
        assert!(
            error
                .remediation
                .join(" ")
                .contains("does not support OAuth")
        );
    }

    #[test]
    fn caps_limits() {
        let opts = QuickstartOptions {
            account: None,
            folder: "INBOX",
            peek_limit: 999,
            timeout_secs: 999,
            skip_network: true,
            backend: CredentialBackend::File,
        };
        assert_eq!(opts.peek_limit.min(MAX_PEEK_LIMIT), 25);
        assert_eq!(opts.timeout_secs.min(MAX_TIMEOUT_SECS), 60);
    }
}
