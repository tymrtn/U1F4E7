// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Shared build/runtime identity so the CLI and dashboard can report the same
//! version and binary path. This is the foundation for detecting installed
//! dashboard binary / service drift (issue #46): if the dashboard reports a
//! different `version`/`binary_path` than the CLI the operator is running,
//! mailbox errors are likely stale-binary drift, not bad credentials.

use serde::Serialize;
use std::path::PathBuf;

/// The Envelope crate/package version, embedded at compile time. The workspace
/// pins a single version, so CLI and dashboard built from the same tree agree.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build/runtime identity for the currently executing Envelope binary.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BuildInfo {
    /// Compile-time crate version (e.g. `0.11.0`).
    pub version: String,
    /// Absolute path of the running binary, if resolvable. Operators compare
    /// this against the installed launchd binary path to spot drift.
    pub binary_path: Option<String>,
}

impl BuildInfo {
    /// Collect build/runtime identity for the current process. Never fails;
    /// `binary_path` is `None` if the OS cannot resolve `current_exe`.
    pub fn current() -> Self {
        let binary_path = std::env::current_exe().ok().map(|p| canonicalize_lossy(&p));
        Self {
            version: VERSION.to_string(),
            binary_path,
        }
    }
}

fn canonicalize_lossy(path: &PathBuf) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.clone())
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_non_empty() {
        assert!(!VERSION.is_empty());
        assert_eq!(BuildInfo::current().version, VERSION);
    }

    #[test]
    fn build_info_serializes_version_field() {
        let info = BuildInfo {
            version: "9.9.9".to_string(),
            binary_path: Some("/tmp/envelope".to_string()),
        };
        let value = serde_json::to_value(&info).unwrap();
        assert_eq!(value["version"], "9.9.9");
        assert_eq!(value["binary_path"], "/tmp/envelope");
    }
}
