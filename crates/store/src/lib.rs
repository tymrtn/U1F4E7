// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

pub mod accounts;
pub mod action_log;
pub mod build_info;
pub mod contacts;
pub mod credential_store;
pub mod crypto;
pub mod db;
pub mod drafts;
pub mod errors;
pub mod event_routes;
pub mod events;
pub mod license_store;
pub mod message_index;
pub mod migration;
pub mod migrations;
pub mod models;
pub mod ops_primitives;
pub mod paths;
pub mod rule_store;
pub mod snoozed;
pub mod tag_store;
pub mod threads;

pub use build_info::{BuildInfo, VERSION};
pub use credential_store::CredentialBackend;
pub use db::Database;
pub use errors::StoreError;
pub use models::*;
pub use ops_primitives::{RuleRunAuditInput, WatchUpsert};
pub use paths::{app_data_dir, config_root_dir, credential_file_path, database_path};
pub use threads::ThreadContext;
