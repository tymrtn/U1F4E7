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
    /// Bare Message-IDs this draft previously carried on the provider, oldest
    /// first. Each edit re-APPENDs under a new identity; if the pre-APPEND
    /// delete failed or was interrupted, that older copy is still in the folder
    /// and only these identities can find it. Verified exactly like the current
    /// one — a retained identity is a lead, never a licence to delete.
    pub superseded_message_ids: Vec<String>,
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
    let superseded = draft
        .superseded_message_ids()
        .iter()
        .filter_map(|id| imap::normalize_message_id(id))
        .filter(|id| *id != message_id)
        .collect();
    Ok(DraftCleanupTarget {
        folder,
        message_id,
        superseded_message_ids: superseded,
    })
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

/// Result of clearing provider copies before replacing an edited draft.
#[derive(Debug, PartialEq, Eq)]
pub enum ProviderDraftReplaceCleanup {
    /// Every exact copy of the logical draft was deleted and expunged.
    Deleted { uids: Vec<u32> },
    /// No exact copy remains. This is idempotent success: a prior attempt may
    /// already have removed the old copy before failing later in the edit.
    AlreadyAbsent,
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
    // Sweep the identities this draft has previously worn first. Each edit
    // re-APPENDs under a new Message-ID after deleting the old copy; when that
    // delete failed or was interrupted, the older copy stays in the folder
    // forever, because the row only ever names the newest identity. Removing
    // them here is what stops one logical draft leaving a copy per revision.
    //
    // Every candidate is header-verified and must be the UNIQUE exact match —
    // the same fail-closed rule as the current identity. A superseded identity
    // that is ambiguous or absent is simply skipped.
    let mut superseded_uids = Vec::new();
    for stale in &target.superseded_message_ids {
        match imap::find_unique_uid_by_exact_message_id(client, &target.folder, stale).await {
            Ok(Some(uid)) => {
                imap::delete_message(client, &target.folder, uid).await?;
                superseded_uids.push(uid);
            }
            // Absent or ambiguous: nothing safely identifiable to remove.
            Ok(None) => {}
            // A lookup failure on a stale identity must not abort cleanup of
            // the current copy, which is the one that definitely exists.
            Err(e) => {
                tracing::warn!(
                    "draft cleanup: superseded copy lookup failed in {}: {e}",
                    target.folder
                );
            }
        }
    }

    match imap::find_unique_uid_by_exact_message_id(client, &target.folder, &target.message_id)
        .await?
    {
        Some(uid) => {
            imap::delete_message(client, &target.folder, uid).await?;
            Ok(ProviderDraftCleanup::Deleted { uid })
        }
        None if !superseded_uids.is_empty() => {
            // The current identity is gone but stale copies were removed. That
            // is a real cleanup, not a skip.
            Ok(ProviderDraftCleanup::Deleted {
                uid: superseded_uids[superseded_uids.len() - 1],
            })
        }
        None => Ok(ProviderDraftCleanup::Skipped(
            "provider draft copy not uniquely identified by exact Message-ID",
        )),
    }
}

/// Clear every exact provider copy before APPENDing an edited replacement.
///
/// Unlike post-send cleanup, replacement must recover from duplicates created
/// by an interrupted older edit. Multiple UIDs are safe to remove only after
/// each candidate's Message-ID header has been fetched and verified as an exact
/// match for the persisted logical draft identity. Similar/substring matches
/// are never returned by `find_uids_by_exact_message_id` and are untouched.
pub async fn clear_provider_draft_copies_for_replace(
    client: &mut ImapClient,
    target: &DraftCleanupTarget,
) -> Result<ProviderDraftReplaceCleanup, ImapError> {
    let uids =
        imap::find_uids_by_exact_message_id(client, &target.folder, &target.message_id).await?;
    if uids.is_empty() {
        return Ok(ProviderDraftReplaceCleanup::AlreadyAbsent);
    }

    for uid in &uids {
        imap::delete_message(client, &target.folder, *uid).await?;
    }
    Ok(ProviderDraftReplaceCleanup::Deleted { uids })
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

    /// Identities retained across re-APPENDs reach the cleanup target, and the
    /// current one is never duplicated among them. Without this, a draft that
    /// was edited leaves one provider copy per revision: the row names only the
    /// newest identity, so nothing can locate the older copies again.
    #[test]
    fn resolve_target_carries_superseded_identities() {
        let db = seeded_db();
        db.set_detected_folder("gmail1", "drafts", "[Gmail]/Drafts")
            .unwrap();
        let draft = db
            .create_draft(
                "gmail1", "to@test.com", Some("S"), Some("B"), None, None, None, None, Some("cli"),
            )
            .unwrap();
        db.mark_draft_message_id(&draft.id, "<current@mac.lan>")
            .unwrap();
        db.set_draft_metadata(
            &draft.id,
            &serde_json::json!({
                envelope_email_store::drafts::SUPERSEDED_MESSAGE_IDS:
                    ["<older@mac.lan>", "<current@mac.lan>"]
            }),
        )
        .unwrap();

        let draft = db.get_draft(&draft.id).unwrap().unwrap();
        let target = resolve_draft_cleanup_target(&db, &draft).unwrap();

        assert_eq!(target.message_id, "current@mac.lan");
        assert_eq!(
            target.superseded_message_ids,
            vec!["older@mac.lan".to_string()],
            "normalized, and the current identity is not swept twice"
        );
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
                // A draft that has never been re-appended wears one identity.
                superseded_message_ids: Vec::new(),
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
