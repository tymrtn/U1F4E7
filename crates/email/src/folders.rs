// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! IMAP folder auto-detection and classification.
//!
//! Uses the provider detection system ([`crate::provider`]) to resolve canonical
//! folder names. When the provider is already known, folder resolution is instant
//! (no IMAP round-trip). When unknown, it falls back to trying known candidates
//! and auto-detects the provider for future use.

use envelope_email_store::Database;
use tracing::{debug, info, warn};

use crate::errors::ImapError;
use crate::imap;
use crate::provider::{self, ProviderType, canonical};

/// Detect and store the provider type for an account if not already known.
///
/// Lists IMAP folders, detects the provider, stores it, and caches all
/// canonical folder mappings in the `detected_folders` table.
///
/// Returns the detected provider type.
pub async fn detect_and_store_provider(
    client: &mut imap::ImapClient,
    db: &Database,
    account_id: &str,
) -> Result<ProviderType, ImapError> {
    let folders = imap::list_folders(client).await?;
    let provider = provider::detect_provider(&folders);

    info!("detected provider type: {provider} for account {account_id}");

    // Store the provider type
    if let Err(e) = db.set_provider_type(account_id, provider.as_str()) {
        warn!("failed to store provider type: {e}");
    }

    // Cache all canonical folder mappings using the detected provider
    let folder_set: std::collections::HashSet<&str> = folders.iter().map(|s| s.as_str()).collect();

    for canonical_type in &[
        canonical::DRAFTS,
        canonical::SENT,
        canonical::TRASH,
        canonical::SPAM,
        canonical::ARCHIVE,
    ] {
        let resolved = provider::resolve_folder(provider, canonical_type);
        if folder_set.contains(resolved)
            && let Err(e) = db.set_detected_folder(account_id, canonical_type, resolved)
        {
            warn!("failed to cache {canonical_type} folder: {e}");
        }
    }

    Ok(provider)
}

/// Get the provider type for an account, detecting on first use.
///
/// 1. Check the stored `provider_type` in the accounts table
/// 2. If NULL, detect from IMAP folder list and store it
/// 3. Return the provider type
pub async fn get_or_detect_provider(
    client: &mut imap::ImapClient,
    db: &Database,
    account_id: &str,
) -> Result<ProviderType, ImapError> {
    // Check stored value first
    if let Ok(Some(stored)) = db.get_provider_type(account_id) {
        let provider = ProviderType::from_str_value(&stored);
        if provider != ProviderType::Unknown {
            debug!("using stored provider type: {provider}");
            return Ok(provider);
        }
    }

    // Not stored yet — detect and store
    detect_and_store_provider(client, db, account_id).await
}

/// Detect the drafts folder for an account.
///
/// Uses provider-aware resolution when the provider is known, falling back
/// to the try-all-candidates approach for unknown providers.
/// Caches the result in the `detected_folders` table.
pub async fn detect_drafts_folder(
    client: &mut imap::ImapClient,
    db: &Database,
    account_id: &str,
) -> Result<Option<String>, ImapError> {
    // Check cached value first
    if let Ok(Some(cached)) = db.get_drafts_folder(account_id) {
        debug!("using cached drafts folder: {cached}");
        return Ok(Some(cached));
    }

    // Try provider-aware resolution
    let provider = get_or_detect_provider(client, db, account_id).await?;
    if provider != ProviderType::Unknown {
        let resolved = provider::resolve_folder(provider, canonical::DRAFTS).to_string();

        // Verify the folder actually exists on the server
        let folders = imap::list_folders(client).await?;
        if folders.iter().any(|f| f == &resolved) {
            info!("resolved drafts folder via provider ({provider}): {resolved}");
            if let Err(e) = db.set_detected_folder(account_id, "drafts", &resolved) {
                warn!("failed to cache drafts folder: {e}");
            }
            return Ok(Some(resolved));
        }
        // Provider resolution didn't match — fall through to candidate search
        warn!(
            "provider {provider} resolved drafts to '{resolved}' but folder not found, trying candidates"
        );
    }

    // Fallback: try all known candidates
    let folders = imap::list_folders(client).await?;
    let folder_set: std::collections::HashSet<&str> = folders.iter().map(|s| s.as_str()).collect();

    for candidate in provider::all_candidates_for(canonical::DRAFTS) {
        if folder_set.contains(*candidate) {
            info!("detected drafts folder (candidate match): {candidate}");
            if let Err(e) = db.set_detected_folder(account_id, "drafts", candidate) {
                warn!("failed to cache drafts folder: {e}");
            }
            return Ok(Some(candidate.to_string()));
        }
    }

    // Case-insensitive fuzzy fallback
    for folder in &folders {
        let lower = folder.to_lowercase();
        if lower.contains("draft") {
            info!("detected drafts folder (fuzzy): {folder}");
            if let Err(e) = db.set_detected_folder(account_id, "drafts", folder) {
                warn!("failed to cache drafts folder: {e}");
            }
            return Ok(Some(folder.clone()));
        }
    }

    warn!("no drafts folder detected for account {account_id}");
    Ok(None)
}

/// Detect the sent folder for an account.
///
/// Uses provider-aware resolution when the provider is known, falling back
/// to the try-all-candidates approach for unknown providers.
/// Caches the result in the `detected_folders` table.
pub async fn detect_sent_folder(
    client: &mut imap::ImapClient,
    db: &Database,
    account_id: &str,
) -> Result<Option<String>, ImapError> {
    // Check cached value first
    if let Ok(Some(cached)) = db.get_sent_folder(account_id) {
        debug!("using cached sent folder: {cached}");
        return Ok(Some(cached));
    }

    // Try provider-aware resolution
    let provider = get_or_detect_provider(client, db, account_id).await?;
    if provider != ProviderType::Unknown {
        let resolved = provider::resolve_folder(provider, canonical::SENT).to_string();

        // Verify the folder actually exists on the server
        let folders = imap::list_folders(client).await?;
        if folders.iter().any(|f| f == &resolved) {
            info!("resolved sent folder via provider ({provider}): {resolved}");
            if let Err(e) = db.set_detected_folder(account_id, "sent", &resolved) {
                warn!("failed to cache sent folder: {e}");
            }
            return Ok(Some(resolved));
        }
        warn!(
            "provider {provider} resolved sent to '{resolved}' but folder not found, trying candidates"
        );
    }

    // Fallback: try all known candidates
    let folders = imap::list_folders(client).await?;
    let folder_set: std::collections::HashSet<&str> = folders.iter().map(|s| s.as_str()).collect();

    for candidate in provider::all_candidates_for(canonical::SENT) {
        if folder_set.contains(*candidate) {
            info!("detected sent folder (candidate match): {candidate}");
            if let Err(e) = db.set_detected_folder(account_id, "sent", candidate) {
                warn!("failed to cache sent folder: {e}");
            }
            return Ok(Some(candidate.to_string()));
        }
    }

    // Case-insensitive fuzzy fallback
    for folder in &folders {
        let lower = folder.to_lowercase();
        if lower.contains("sent") && !lower.contains("unsent") {
            info!("detected sent folder (fuzzy): {folder}");
            if let Err(e) = db.set_detected_folder(account_id, "sent", folder) {
                warn!("failed to cache sent folder: {e}");
            }
            return Ok(Some(folder.clone()));
        }
    }

    warn!("no sent folder detected for account {account_id}");
    Ok(None)
}

/// Detect a folder by its canonical type name.
///
/// Generic version of `detect_drafts_folder` / `detect_sent_folder` that works
/// for any canonical folder type (trash, spam, archive, etc.).
pub async fn detect_folder(
    client: &mut imap::ImapClient,
    db: &Database,
    account_id: &str,
    canonical_type: &str,
) -> Result<Option<String>, ImapError> {
    // Check cached value first
    if let Ok(folders) = db.get_detected_folders(account_id) {
        for (ftype, fname) in &folders {
            if ftype == canonical_type {
                debug!("using cached {canonical_type} folder: {fname}");
                return Ok(Some(fname.clone()));
            }
        }
    }

    // Try provider-aware resolution
    let provider = get_or_detect_provider(client, db, account_id).await?;
    if provider != ProviderType::Unknown {
        let resolved = provider::resolve_folder(provider, canonical_type).to_string();

        // Verify the folder exists
        let folders = imap::list_folders(client).await?;
        if folders.iter().any(|f| f == &resolved) {
            info!("resolved {canonical_type} folder via provider ({provider}): {resolved}");
            if let Err(e) = db.set_detected_folder(account_id, canonical_type, &resolved) {
                warn!("failed to cache {canonical_type} folder: {e}");
            }
            return Ok(Some(resolved));
        }
    }

    // Fallback: try all known candidates for this type
    let folders = imap::list_folders(client).await?;
    let folder_set: std::collections::HashSet<&str> = folders.iter().map(|s| s.as_str()).collect();

    for candidate in provider::all_candidates_for(canonical_type) {
        if folder_set.contains(*candidate) {
            info!("detected {canonical_type} folder (candidate match): {candidate}");
            if let Err(e) = db.set_detected_folder(account_id, canonical_type, candidate) {
                warn!("failed to cache {canonical_type} folder: {e}");
            }
            return Ok(Some(candidate.to_string()));
        }
    }

    warn!("no {canonical_type} folder detected for account {account_id}");
    Ok(None)
}

/// Classify all folders for an account and cache the results.
///
/// Uses the provider detection system for accurate classification.
pub async fn classify_folders(
    client: &mut imap::ImapClient,
    db: &Database,
    account_id: &str,
) -> Result<Vec<FolderInfo>, ImapError> {
    let provider = get_or_detect_provider(client, db, account_id).await?;
    let folders = imap::list_folders(client).await?;
    let mut results = Vec::new();

    for folder in &folders {
        let folder_type = provider::classify_folder(folder)
            .map(|s| s.to_string())
            .unwrap_or_else(|| "other".to_string());

        // Cache known types
        if folder_type != "other"
            && let Err(e) = db.set_detected_folder(account_id, &folder_type, folder)
        {
            warn!("failed to cache folder type: {e}");
        }

        results.push(FolderInfo {
            name: folder.clone(),
            folder_type,
            provider_type: provider.as_str().to_string(),
        });
    }

    Ok(results)
}

/// Folder with its detected type and provider context.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FolderInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub folder_type: String,
    pub provider_type: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_drafts_folder_db_cache() {
        let db = Database::open_memory().unwrap();

        // No drafts folder cached yet
        assert!(db.get_drafts_folder("acct1").unwrap().is_none());

        // Cache a standard drafts folder
        db.set_detected_folder("acct1", "drafts", "Drafts").unwrap();
        assert_eq!(
            db.get_drafts_folder("acct1").unwrap().as_deref(),
            Some("Drafts")
        );

        // Gmail-style drafts folder overwrites the cached value
        db.set_detected_folder("acct1", "drafts", "[Gmail]/Drafts")
            .unwrap();
        assert_eq!(
            db.get_drafts_folder("acct1").unwrap().as_deref(),
            Some("[Gmail]/Drafts")
        );

        // Different accounts have independent caches
        assert!(db.get_drafts_folder("acct2").unwrap().is_none());
        db.set_detected_folder("acct2", "drafts", "INBOX.Drafts")
            .unwrap();
        assert_eq!(
            db.get_drafts_folder("acct2").unwrap().as_deref(),
            Some("INBOX.Drafts")
        );
        // acct1 still has Gmail drafts
        assert_eq!(
            db.get_drafts_folder("acct1").unwrap().as_deref(),
            Some("[Gmail]/Drafts")
        );
    }

    #[test]
    fn test_sent_folder_db_cache() {
        let db = Database::open_memory().unwrap();

        assert!(db.get_sent_folder("acct1").unwrap().is_none());

        db.set_detected_folder("acct1", "sent", "Sent").unwrap();
        assert_eq!(
            db.get_sent_folder("acct1").unwrap().as_deref(),
            Some("Sent")
        );

        // Gmail-style
        db.set_detected_folder("acct1", "sent", "[Gmail]/Sent Mail")
            .unwrap();
        assert_eq!(
            db.get_sent_folder("acct1").unwrap().as_deref(),
            Some("[Gmail]/Sent Mail")
        );
    }

    #[test]
    fn test_provider_type_db_roundtrip() {
        let db = Database::open_memory().unwrap();
        let passphrase = "test";

        // Create a test account
        let acct = db
            .create_account(
                "Test Gmail",
                "test@gmail.com",
                "pw",
                "smtp.gmail.com",
                587,
                "imap.gmail.com",
                993,
                passphrase,
            )
            .unwrap();

        // No provider type initially
        assert!(db.get_provider_type(&acct.id).unwrap().is_none());

        // Store provider type
        db.set_provider_type(&acct.id, "gmail").unwrap();
        assert_eq!(
            db.get_provider_type(&acct.id).unwrap().as_deref(),
            Some("gmail")
        );

        // Update provider type
        db.set_provider_type(&acct.id, "standard").unwrap();
        assert_eq!(
            db.get_provider_type(&acct.id).unwrap().as_deref(),
            Some("standard")
        );
    }

    #[test]
    fn test_all_candidates_for_includes_provider_resolved() {
        // Verify that the candidate lists in provider.rs cover all the names
        // that resolve_folder returns for each provider
        let drafts_candidates = provider::all_candidates_for("drafts");
        assert!(drafts_candidates.contains(&"Drafts")); // Standard
        assert!(drafts_candidates.contains(&"[Gmail]/Drafts")); // Gmail
        assert!(drafts_candidates.contains(&"INBOX.Drafts")); // Dovecot

        let sent_candidates = provider::all_candidates_for("sent");
        assert!(sent_candidates.contains(&"Sent")); // Standard
        assert!(sent_candidates.contains(&"[Gmail]/Sent Mail")); // Gmail
        assert!(sent_candidates.contains(&"Sent Items")); // Exchange
        assert!(sent_candidates.contains(&"INBOX.Sent")); // Dovecot
    }
}
