// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! `envelope doctor` — structured auth/state diagnosis and bounded, safe repair.
//!
//! Issue #34, Slice A (local auto-repair MVP). This goes beyond the read-only
//! `envelope paths` report: it classifies *why* mailbox operations might fail
//! even when account metadata is readable (the classic
//! `credential_decrypt_failed` / `decrypted_but_imap_auth_failed` confusion),
//! and offers a backup-before-mutation repair that never deletes originals and
//! never prints secrets.
//!
//! Safety invariants (see CLAUDE.md):
//! - No secret material in stdout/stderr/JSON. The decrypt test only reports
//!   success/failure, never the plaintext.
//! - `--repair` without `--dry-run` performs the backup step only; riskier
//!   repairs (credential restore, provider reset, recreate) are reported as
//!   `not_available` rather than faked.
//! - Dangerous-by-default: any repair plan is shown in dry-run first.
//! - No email is ever sent. `--check-auth` is an IMAP login probe only.

use anyhow::Result;
use envelope_email_store::{BuildInfo, CredentialBackend, Database, credential_store};
use serde::Serialize;
use std::path::PathBuf;
use std::time::Instant;

use super::paths::{PathsReport, collect_report};

/// Machine-readable classification of Envelope auth/state health.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Ok,
    HomeDriftWarning,
    MissingDb,
    MissingCredentialFile,
    CredentialDecryptFailed,
    DecryptedButImapAuthFailed,
    AccountMissing,
    NoAccounts,
    /// Reserved for unexpected states; part of the documented classification set.
    #[allow(dead_code)]
    Unknown,
}

impl DoctorStatus {
    fn severity(self) -> &'static str {
        match self {
            DoctorStatus::Ok => "ok",
            DoctorStatus::HomeDriftWarning => "warning",
            DoctorStatus::MissingDb
            | DoctorStatus::MissingCredentialFile
            | DoctorStatus::CredentialDecryptFailed
            | DoctorStatus::DecryptedButImapAuthFailed
            | DoctorStatus::AccountMissing
            | DoctorStatus::NoAccounts => "error",
            DoctorStatus::Unknown => "error",
        }
    }

    fn safe_next_action(self) -> &'static str {
        match self {
            DoctorStatus::Ok => "No action needed.",
            DoctorStatus::HomeDriftWarning => {
                "Pin HOME so agents/shells resolve the same Envelope state; see the paths warnings."
            }
            DoctorStatus::MissingDb => {
                "No Envelope database in the active HOME. Add an account or fix HOME drift."
            }
            DoctorStatus::MissingCredentialFile => {
                "Credential file is absent for the file backend; re-import credentials for the account."
            }
            DoctorStatus::CredentialDecryptFailed => {
                "DB and credential store do not match. Restore a matching credential backup or re-import recovery credentials, then re-run with --check-auth."
            }
            DoctorStatus::DecryptedButImapAuthFailed => {
                "Credentials decrypt but IMAP login failed; verify password/app-password, IMAP enablement, or provider cooldown."
            }
            DoctorStatus::AccountMissing => {
                "Selected account was not found in the active store; check --account or run `envelope accounts list`."
            }
            DoctorStatus::NoAccounts => "No accounts configured in the active store.",
            DoctorStatus::Unknown => "Unexpected state; inspect the report fields.",
        }
    }

    fn repair_available(self) -> bool {
        matches!(
            self,
            DoctorStatus::CredentialDecryptFailed | DoctorStatus::MissingCredentialFile
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthProbe {
    pub account: String,
    pub status: String, // "ok" | "auth_failed" | "skipped"
    pub code: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannedAction {
    pub action: String,
    pub detail: String,
    pub mutates_state: bool,
    pub available: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepairOutcome {
    pub dry_run: bool,
    pub planned_actions: Vec<PlannedAction>,
    /// Executed actions (empty in dry-run). Backup paths only — never secrets.
    pub executed_actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub status: DoctorStatus,
    pub severity: String,
    pub safe_next_action: String,
    pub repair_available: bool,
    pub version: String,
    pub binary_path: Option<String>,
    pub paths: PathsReport,
    pub account_count: usize,
    pub selected_account: Option<String>,
    pub credential_decrypt_ok: Option<bool>,
    pub auth_probe: Option<AuthProbe>,
    pub repair: Option<RepairOutcome>,
}

pub struct DoctorOptions<'a> {
    pub json: bool,
    pub backend: CredentialBackend,
    pub account: Option<&'a str>,
    pub check_auth: bool,
    pub repair: bool,
    pub dry_run: bool,
    pub backup_dir: Option<&'a str>,
    pub timeout_secs: u64,
}

pub fn run(opts: DoctorOptions<'_>) -> Result<()> {
    let report = diagnose(&opts);

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human(&report);
    }
    Ok(())
}

fn diagnose(opts: &DoctorOptions<'_>) -> DoctorReport {
    let build = BuildInfo::current();
    let paths = collect_report(opts.backend);

    // Open the store read-only; never create directories or DB files here.
    let db = Database::open_default_readonly_existing();

    let mut status = DoctorStatus::Ok;
    let mut account_count = 0usize;
    let mut selected_account: Option<String> = None;
    let mut credential_decrypt_ok: Option<bool> = None;
    let mut auth_probe: Option<AuthProbe> = None;

    let mut decrypted_creds = None;

    match db {
        Ok(None) => {
            status = DoctorStatus::MissingDb;
        }
        Err(_) => {
            status = DoctorStatus::MissingDb;
        }
        Ok(Some(db)) => {
            let accounts = db.list_accounts().unwrap_or_default();
            account_count = accounts.len();
            if accounts.is_empty() {
                status = DoctorStatus::NoAccounts;
            } else {
                // Resolve selected account (explicit --account or default).
                let chosen = resolve_account(&db, &accounts, opts.account);
                match chosen {
                    None => status = DoctorStatus::AccountMissing,
                    Some(acct) => {
                        selected_account = Some(acct.username.clone());
                        // Decrypt test: read the passphrase without mutating,
                        // then attempt to decrypt the selected account's secret.
                        match credential_store::get_passphrase(opts.backend) {
                            Err(_) => {
                                credential_decrypt_ok = Some(false);
                                status = if opts.backend == CredentialBackend::File {
                                    DoctorStatus::MissingCredentialFile
                                } else {
                                    DoctorStatus::CredentialDecryptFailed
                                };
                            }
                            Ok(passphrase) => {
                                match db.get_account_with_credentials(&acct.id, &passphrase) {
                                    Ok(creds) => {
                                        credential_decrypt_ok = Some(true);
                                        decrypted_creds = Some(creds);
                                    }
                                    Err(_) => {
                                        credential_decrypt_ok = Some(false);
                                        status = DoctorStatus::CredentialDecryptFailed;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Optional IMAP login probe (no mailbox mutation, no email send).
    if opts.check_auth {
        match &decrypted_creds {
            None => {
                auth_probe = Some(AuthProbe {
                    account: selected_account.clone().unwrap_or_default(),
                    status: "skipped".to_string(),
                    code: Some("credentials_unavailable".to_string()),
                    message: Some(
                        "Skipped IMAP probe because credentials did not decrypt.".to_string(),
                    ),
                });
            }
            Some(creds) => {
                let probe = probe_imap_auth(creds, opts.timeout_secs);
                if probe.status == "auth_failed" && status == DoctorStatus::Ok {
                    status = DoctorStatus::DecryptedButImapAuthFailed;
                }
                auth_probe = Some(probe);
            }
        }
    }

    // Promote to a HOME-drift warning only when state is otherwise healthy but
    // the paths layer flagged an unstable HOME.
    if status == DoctorStatus::Ok && !paths.warnings.is_empty() {
        status = DoctorStatus::HomeDriftWarning;
    }

    let repair = if opts.repair {
        Some(plan_and_maybe_repair(opts, status, &paths))
    } else {
        None
    };

    DoctorReport {
        status,
        severity: status.severity().to_string(),
        safe_next_action: status.safe_next_action().to_string(),
        repair_available: status.repair_available(),
        version: build.version,
        binary_path: build.binary_path,
        paths,
        account_count,
        selected_account,
        credential_decrypt_ok,
        auth_probe,
        repair,
    }
}

fn resolve_account(
    db: &Database,
    accounts: &[envelope_email_store::Account],
    requested: Option<&str>,
) -> Option<envelope_email_store::Account> {
    match requested {
        None => db.default_account().ok().flatten(),
        Some(want) => {
            if let Ok(Some(a)) = db.get_account(want) {
                return Some(a);
            }
            if let Ok(Some(a)) = db.find_account_by_email(want) {
                return Some(a);
            }
            // Fall back to a case-insensitive username match within the list.
            accounts
                .iter()
                .find(|a| a.username.eq_ignore_ascii_case(want))
                .cloned()
        }
    }
}

fn probe_imap_auth(
    creds: &envelope_email_store::models::AccountWithCredentials,
    timeout_secs: u64,
) -> AuthProbe {
    let account = creds.account.username.clone();
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            return AuthProbe {
                account,
                status: "auth_failed".to_string(),
                code: Some("runtime_error".to_string()),
                message: Some(sanitize(&e.to_string())),
            };
        }
    };

    let timeout = std::time::Duration::from_secs(timeout_secs.clamp(1, 60));
    let started = Instant::now();
    let result = rt.block_on(async {
        tokio::time::timeout(timeout, envelope_email_transport::imap::connect(creds)).await
    });
    let _ = started;

    match result {
        Ok(Ok(_client)) => AuthProbe {
            account,
            status: "ok".to_string(),
            code: None,
            message: None,
        },
        Ok(Err(e)) => AuthProbe {
            account,
            status: "auth_failed".to_string(),
            code: Some("imap_auth_failed".to_string()),
            message: Some(sanitize(&e.to_string())),
        },
        Err(_) => AuthProbe {
            account,
            status: "auth_failed".to_string(),
            code: Some("imap_timeout".to_string()),
            message: Some("IMAP login probe timed out.".to_string()),
        },
    }
}

/// Build the repair plan and, when not a dry-run, execute the safe backup step.
fn plan_and_maybe_repair(
    opts: &DoctorOptions<'_>,
    status: DoctorStatus,
    paths: &PathsReport,
) -> RepairOutcome {
    let mut planned = Vec::new();
    let backup_dir = backup_dir_path(opts.backup_dir);

    // Step 1: always-safe backup of DB + credential file before any mutation.
    planned.push(PlannedAction {
        action: "backup_state".to_string(),
        detail: format!(
            "Copy database and credential file to {} with SHA-256 checksums (originals preserved).",
            backup_dir.display()
        ),
        mutates_state: false,
        available: true,
    });

    // Step 2+: riskier repairs are intentionally not auto-executed in this slice.
    if matches!(
        status,
        DoctorStatus::CredentialDecryptFailed | DoctorStatus::MissingCredentialFile
    ) {
        planned.push(PlannedAction {
            action: "restore_matching_credential_backup".to_string(),
            detail: "Test credential backups against the current DB; restore only if exactly one decrypts the selected account. Not yet executed by doctor.".to_string(),
            mutates_state: true,
            available: false,
        });
        planned.push(PlannedAction {
            action: "import_recovery_credentials".to_string(),
            detail: "Re-encrypt supplied recovery credentials into the current store (requires --credential-source). Not yet executed by doctor.".to_string(),
            mutates_state: true,
            available: false,
        });
    }

    let mut executed = Vec::new();
    if !opts.dry_run {
        // Execute only the always-safe backup step.
        match execute_backup(paths, &backup_dir) {
            Ok(written) => executed = written,
            Err(e) => executed.push(format!("backup_failed: {}", sanitize(&e.to_string()))),
        }
    }

    RepairOutcome {
        dry_run: opts.dry_run,
        planned_actions: planned,
        executed_actions: executed,
    }
}

fn backup_dir_path(requested: Option<&str>) -> PathBuf {
    if let Some(dir) = requested {
        return PathBuf::from(dir);
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    envelope_email_store::app_data_dir().join(format!("doctor-backup-{stamp}"))
}

/// Copy DB + credential file into `backup_dir`, writing `<name>.sha256` files.
/// Never deletes originals. Returns the list of backup paths written.
fn execute_backup(paths: &PathsReport, backup_dir: &std::path::Path) -> Result<Vec<String>> {
    std::fs::create_dir_all(backup_dir)?;
    let mut written = Vec::new();

    let mut copy_with_checksum = |src: &str| -> Result<()> {
        let src_path = std::path::Path::new(src);
        if !src_path.exists() {
            return Ok(());
        }
        let file_name = src_path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "state.bin".to_string());
        let dest = backup_dir.join(&file_name);
        std::fs::copy(src_path, &dest)?;
        let sha = envelope_email_transport::backup::sha256_hex_file(&dest)?;
        let sha_path = backup_dir.join(format!("{file_name}.sha256"));
        std::fs::write(&sha_path, format!("{sha}  {file_name}\n"))?;
        written.push(dest.display().to_string());
        written.push(sha_path.display().to_string());
        Ok(())
    };

    copy_with_checksum(&paths.database_path)?;
    if paths.credential_file_in_use {
        copy_with_checksum(&paths.credential_file_path)?;
    }
    Ok(written)
}

/// Strip anything that could carry secret material out of error text. Errors in
/// this codebase do not embed secrets, but we defensively cap length.
fn sanitize(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() > 300 {
        format!("{}…", &trimmed[..300])
    } else {
        trimmed.to_string()
    }
}

fn print_human(report: &DoctorReport) {
    println!("Envelope doctor");
    println!("  version:        {}", report.version);
    if let Some(bin) = &report.binary_path {
        println!("  binary:         {bin}");
    }
    println!(
        "  status:         {:?} ({})",
        report.status, report.severity
    );
    println!("  accounts:       {}", report.account_count);
    if let Some(acct) = &report.selected_account {
        println!("  account:        {acct}");
    }
    if let Some(ok) = report.credential_decrypt_ok {
        println!("  decrypt test:   {}", if ok { "ok" } else { "FAILED" });
    }
    if let Some(probe) = &report.auth_probe {
        println!("  imap auth:      {}", probe.status);
    }
    println!("  next action:    {}", report.safe_next_action);
    if let Some(repair) = &report.repair {
        println!(
            "  repair ({}):",
            if repair.dry_run {
                "dry-run"
            } else {
                "executed"
            }
        );
        for a in &repair.planned_actions {
            println!(
                "    - {} [{}{}]: {}",
                a.action,
                if a.mutates_state { "mutates" } else { "safe" },
                if a.available { "" } else { ", not-available" },
                a.detail
            );
        }
        for e in &repair.executed_actions {
            println!("    wrote: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serializes_snake_case() {
        let v = serde_json::to_value(DoctorStatus::CredentialDecryptFailed).unwrap();
        assert_eq!(v, "credential_decrypt_failed");
        let v = serde_json::to_value(DoctorStatus::DecryptedButImapAuthFailed).unwrap();
        assert_eq!(v, "decrypted_but_imap_auth_failed");
    }

    #[test]
    fn missing_credential_file_offers_repair() {
        assert!(DoctorStatus::CredentialDecryptFailed.repair_available());
        assert!(DoctorStatus::MissingCredentialFile.repair_available());
        assert!(!DoctorStatus::Ok.repair_available());
    }

    #[test]
    fn severity_classification() {
        assert_eq!(DoctorStatus::Ok.severity(), "ok");
        assert_eq!(DoctorStatus::HomeDriftWarning.severity(), "warning");
        assert_eq!(DoctorStatus::MissingDb.severity(), "error");
    }

    #[test]
    fn dry_run_plan_does_not_execute() {
        let opts = DoctorOptions {
            json: true,
            backend: CredentialBackend::File,
            account: None,
            check_auth: false,
            repair: true,
            dry_run: true,
            backup_dir: Some("/tmp/envelope-doctor-test-should-not-be-written"),
            timeout_secs: 5,
        };
        let paths = PathsReport {
            credential_backend: "file".to_string(),
            credential_file_in_use: true,
            database_path: "/tmp/nonexistent-envelope.db".to_string(),
            credential_file_path: "/tmp/nonexistent-credentials.json".to_string(),
            app_data_dir: "/tmp".to_string(),
            home: None,
            warnings: vec![],
        };
        let outcome = plan_and_maybe_repair(&opts, DoctorStatus::CredentialDecryptFailed, &paths);
        assert!(outcome.dry_run);
        assert!(outcome.executed_actions.is_empty());
        assert!(
            outcome
                .planned_actions
                .iter()
                .any(|a| a.action == "backup_state" && !a.mutates_state)
        );
        // Riskier repairs are planned but flagged unavailable in this slice.
        assert!(
            outcome
                .planned_actions
                .iter()
                .any(|a| a.action == "restore_matching_credential_backup" && !a.available)
        );
        assert!(!std::path::Path::new("/tmp/envelope-doctor-test-should-not-be-written").exists());
    }

    #[test]
    fn backup_copies_existing_files_with_checksums() {
        let tmp = std::env::temp_dir().join(format!(
            "envelope-doctor-backup-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src_db = tmp.join("envelope.db");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(&src_db, b"fake-db-bytes").unwrap();
        let backup_dir = tmp.join("backup");

        let paths = PathsReport {
            credential_backend: "file".to_string(),
            credential_file_in_use: false,
            database_path: src_db.display().to_string(),
            credential_file_path: "/tmp/none".to_string(),
            app_data_dir: tmp.display().to_string(),
            home: None,
            warnings: vec![],
        };
        let written = execute_backup(&paths, &backup_dir).unwrap();
        // original preserved
        assert!(src_db.exists());
        // backup + checksum written
        assert!(backup_dir.join("envelope.db").exists());
        assert!(backup_dir.join("envelope.db.sha256").exists());
        assert!(written.iter().any(|w| w.ends_with("envelope.db")));
        std::fs::remove_dir_all(&tmp).ok();
    }
}
