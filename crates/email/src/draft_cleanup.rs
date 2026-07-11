// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Identity-safe provider draft cleanup, shared by every actual-send surface
//! (scheduled sweep, CLI `draft send`, MCP `send_draft`) and by the modify
//! provider-sync replace.
//!
//! IMAP UIDs are only meaningful per folder and can go stale (UIDVALIDITY),
//! and `SEARCH HEADER` is substring-based — so deleting "the draft copy" by a
//! raw UID in a guessed folder can remove an unrelated message. Cleanup here
//! is fail-closed by construction:
//!
//! 1. The folder comes ONLY from the detected-folder cache (never a guessed
//!    `Drafts` fallback; a cache miss or read error skips cleanup).
//! 2. The copy is re-located on the server by the draft's persisted
//!    Message-ID, header-verified per candidate, and deleted only when
//!    **exactly one** exact match exists ([`imap::find_unique_uid_by_exact_message_id`]).
//! 3. Callers run this strictly AFTER SMTP acceptance and durable sent-state
//!    persistence (send paths), or while holding the exclusive `syncing`
//!    claim (modify replace). A skip is reported, never claimed as done.

use envelope_email_store::{Database, Draft};

use crate::errors::ImapError;
use crate::imap::{self, ImapClient};

/// Identity facts required before a provider draft copy may be deleted.
#[derive(Debug, PartialEq, Eq)]
pub struct DraftCleanupTarget {
    /// Exact detected Drafts folder for the account (e.g. `[Gmail]/Drafts`).
    pub folder: String,
    /// Bare Message-ID (angle brackets stripped) persisted at APPEND time,
    /// used to locate/verify the copy before deletion.
    pub message_id: String,
}

/// Decide whether the provider draft copy can be identified safely enough to
/// delete. Fail-closed: cleanup requires BOTH the exact detected Drafts
/// folder from the cache (no fallback on miss or read error — never
/// hard-code a provider layout) and the draft's persisted Message-ID for
/// in-folder identity verification. Any missing fact skips cleanup with the
/// returned reason.
pub fn resolve_draft_cleanup_target(
    db: &Database,
    draft: &Draft,
) -> Result<DraftCleanupTarget, &'static str> {
    let folder = match db.get_drafts_folder(&draft.account_id) {
        Ok(Some(folder)) => folder,
        Ok(None) => return Err("no detected drafts folder cached; refusing to guess one"),
        Err(_) => return Err("detected-folder cache read failed"),
    };
    let message_id = draft
        .message_id
        .as_deref()
        .and_then(imap::normalize_message_id);
    let Some(message_id) = message_id else {
        return Err("no persisted Message-ID to verify draft identity");
    };
    Ok(DraftCleanupTarget { folder, message_id })
}

/// Outcome of an exact-verified provider draft deletion attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum ProviderDraftCleanup {
    /// The uniquely-verified copy was deleted (server-reported UID).
    Deleted { uid: u32 },
    /// Identity could not be established unambiguously (zero or multiple
    /// exact Message-ID matches) — nothing was deleted.
    Skipped(&'static str),
}

/// Delete the provider draft copy identified by `target`, verifying identity
/// on the server first: the deleted UID is the **single** message in the
/// exact detected folder whose Message-ID header exactly equals the
/// persisted one. Zero or multiple exact matches skip (fail closed). The
/// caller supplies a connected client and owns retry/eviction policy.
pub async fn delete_provider_draft_exact(
    client: &mut ImapClient,
    target: &DraftCleanupTarget,
) -> Result<ProviderDraftCleanup, ImapError> {
    match imap::find_unique_uid_by_exact_message_id(client, &target.folder, &target.message_id)
        .await?
    {
        Some(uid) => {
            imap::delete_message(client, &target.folder, uid).await?;
            Ok(ProviderDraftCleanup::Deleted { uid })
        }
        None => Ok(ProviderDraftCleanup::Skipped(
            "provider draft copy not uniquely identified by exact Message-ID",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded_db() -> Database {
        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('gmail1', 'Gmail', 'tyler@gmail.com', 'gmail.com',
                         'smtp.gmail.com', 587, 'imap.gmail.com', 993, 'encrypted')",
                [],
            )
            .unwrap();
        db
    }

    /// Shared-resolution regression: exact detected folder + normalized
    /// Message-ID, fail-closed on cache miss/error and missing Message-ID.
    /// Pure DB lookups — no mailbox or network access.
    #[test]
    fn resolve_target_is_identity_safe_and_fail_closed() {
        let db = seeded_db();
        let draft = db
            .create_draft(
                "gmail1",
                "to@example.net",
                Some("Queued"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();

        // Cache miss: refuse to guess a folder.
        let no_cache = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(resolve_draft_cleanup_target(&db, &no_cache).is_err());

        db.set_detected_folder("gmail1", "drafts", "[Gmail]/Drafts")
            .unwrap();
        // Missing Message-ID: identity unverifiable.
        let no_mid = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(
            resolve_draft_cleanup_target(&db, &no_mid).unwrap_err(),
            "no persisted Message-ID to verify draft identity"
        );

        db.mark_draft_message_id(&draft.id, "<queued-1@martin.fm>")
            .unwrap();
        let ready = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(
            resolve_draft_cleanup_target(&db, &ready).unwrap(),
            DraftCleanupTarget {
                folder: "[Gmail]/Drafts".to_string(),
                message_id: "queued-1@martin.fm".to_string(),
            }
        );

        // Cache read error (not just a miss) also fails closed.
        db.conn()
            .execute("DROP TABLE detected_folders", [])
            .unwrap();
        assert_eq!(
            resolve_draft_cleanup_target(&db, &ready).unwrap_err(),
            "detected-folder cache read failed"
        );
    }
}
