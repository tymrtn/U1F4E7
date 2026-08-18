// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use thiserror::Error;

#[derive(Error, Debug)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("account not found: {0}")]
    AccountNotFound(String),

    #[error("draft not found: {0}")]
    DraftNotFound(String),

    #[error("draft not editable (status: {0})")]
    DraftNotEditable(String),

    #[error("draft modified concurrently, approval not recorded: {0}")]
    DraftModifiedConcurrently(String),

    /// A hold/unqueue was asked of a draft that carries no `send_after`.
    /// Distinct from [`StoreError::DraftNotEditable`]: the row is perfectly
    /// editable, it simply was never queued, so reporting success would claim
    /// a schedule was cleared that never existed.
    #[error("draft is not scheduled (no send_after set): {0}")]
    DraftNotScheduled(String),

    #[error("encryption error: {0}")]
    Encryption(String),

    #[error("decryption error: {0}")]
    Decryption(String),

    #[error("keyring error: {0}")]
    Keyring(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("migration error: {0}")]
    Migration(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;
