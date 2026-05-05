// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::path::PathBuf;

const APP_DIR_NAME: &str = "envelope-email";
const DATABASE_FILE_NAME: &str = "envelope.db";
const CREDENTIAL_FILE_NAME: &str = "credentials.json";

/// Platform-resolved config directory, or `.config` as a local fallback.
pub fn config_root_dir() -> PathBuf {
    dirs_next::config_dir().unwrap_or_else(|| PathBuf::from(".config"))
}

/// Envelope's config/app-data directory.
pub fn app_data_dir() -> PathBuf {
    config_root_dir().join(APP_DIR_NAME)
}

/// Default SQLite database path.
pub fn database_path() -> PathBuf {
    app_data_dir().join(DATABASE_FILE_NAME)
}

/// File-backed credential store path.
pub fn credential_file_path() -> PathBuf {
    app_data_dir().join(CREDENTIAL_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_path_uses_app_dir() {
        assert_eq!(database_path(), app_data_dir().join("envelope.db"));
    }

    #[test]
    fn credential_file_path_uses_app_dir() {
        assert_eq!(
            credential_file_path(),
            app_data_dir().join("credentials.json")
        );
    }

    #[test]
    fn app_data_dir_includes_product_name() {
        assert!(app_data_dir().ends_with("envelope-email"));
    }
}
