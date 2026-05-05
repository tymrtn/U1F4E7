// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::Result;
use envelope_email_store::{CredentialBackend, app_data_dir, credential_file_path, database_path};
use serde::Serialize;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PathsReport {
    pub credential_backend: String,
    pub credential_file_in_use: bool,
    pub database_path: String,
    pub credential_file_path: String,
    pub app_data_dir: String,
    pub home: Option<String>,
    pub warnings: Vec<String>,
}

pub fn run(json: bool, backend: CredentialBackend) -> Result<()> {
    let report = collect_report(backend);

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("Credential backend: {}", report.credential_backend);
    println!("Database path:      {}", report.database_path);
    println!("File credential path: {}", report.credential_file_path);
    if !report.credential_file_in_use {
        println!("  note: file credential path is inactive with --credential-store keychain");
    }
    println!("Config/app-data dir: {}", report.app_data_dir);
    println!(
        "Current HOME:       {}",
        report.home.as_deref().unwrap_or("(not set)")
    );

    if report.warnings.is_empty() {
        println!("Warnings:           none");
    } else {
        println!("Warnings:");
        for warning in &report.warnings {
            println!("  - {warning}");
        }
    }

    Ok(())
}

pub fn collect_report(backend: CredentialBackend) -> PathsReport {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let db_path = database_path();
    let file_credential_path = credential_file_path();
    let app_dir = app_data_dir();

    PathsReport {
        credential_backend: backend.to_string(),
        credential_file_in_use: backend == CredentialBackend::File,
        database_path: display_path(&db_path),
        credential_file_path: display_path(&file_credential_path),
        app_data_dir: display_path(&app_dir),
        home: home.as_deref().map(display_path),
        warnings: build_warnings(
            home.as_deref(),
            &app_dir,
            &db_path,
            Some(&file_credential_path),
        ),
    }
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

fn build_warnings(
    home: Option<&Path>,
    app_dir: &Path,
    db_path: &Path,
    credential_path: Option<&Path>,
) -> Vec<String> {
    let mut warnings = Vec::new();

    if let Some(home) = home {
        push_warning(&mut warnings, "HOME", home);
    }
    push_warning(&mut warnings, "config/app-data dir", app_dir);
    push_warning(&mut warnings, "database path", db_path);
    if let Some(credential_path) = credential_path {
        push_warning(&mut warnings, "file credential path", credential_path);
    }

    warnings
}

fn push_warning(warnings: &mut Vec<String>, label: &str, path: &Path) {
    if let Some(marker) = detect_agent_harness_marker(path) {
        warnings.push(format!(
            "{label} is under `{marker}`; agent HOME drift can make Envelope state appear missing between runs"
        ));
    }
}

fn detect_agent_harness_marker(path: &Path) -> Option<&'static str> {
    for prefix in ["/private/tmp", "/tmp", "/var/folders"] {
        if path.starts_with(Path::new(prefix)) {
            return Some(prefix);
        }
    }

    if path
        .components()
        .any(|component| component.as_os_str() == OsStr::new(".codex"))
    {
        return Some(".codex");
    }

    if path
        .components()
        .any(|component| component.as_os_str() == OsStr::new(".agents"))
    {
        return Some(".agents");
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_temp_harness_prefixes() {
        let path = Path::new("/private/tmp/envelope-issue-6/.config/envelope-email/envelope.db");
        assert_eq!(detect_agent_harness_marker(path), Some("/private/tmp"));
    }

    #[test]
    fn detects_codex_component() {
        let path = Path::new("/Users/test/.codex/worktree/.config/envelope-email");
        assert_eq!(detect_agent_harness_marker(path), Some(".codex"));
    }

    #[test]
    fn builds_warning_messages_for_affected_paths() {
        let warnings = build_warnings(
            Some(Path::new("/tmp/agent-home")),
            Path::new("/tmp/agent-home/.config/envelope-email"),
            Path::new("/tmp/agent-home/.config/envelope-email/envelope.db"),
            Some(Path::new(
                "/tmp/agent-home/.config/envelope-email/credentials.json",
            )),
        );

        assert_eq!(warnings.len(), 4);
        assert!(warnings.iter().any(|warning| warning.contains("HOME")));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("database path"))
        );
    }

    #[test]
    fn ignores_stable_user_paths() {
        let warnings = build_warnings(
            Some(Path::new("/Users/tester")),
            Path::new("/Users/tester/.config/envelope-email"),
            Path::new("/Users/tester/.config/envelope-email/envelope.db"),
            Some(Path::new(
                "/Users/tester/.config/envelope-email/credentials.json",
            )),
        );

        assert!(warnings.is_empty());
    }
}
