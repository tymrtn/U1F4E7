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
//!
//! After a send, the copy to remove is named by the row's durable
//! `provider_draft_cleanup` record, written in the commit that marks it
//! `sent`. Every attempt settles that record
//! ([`settle_provider_draft_cleanup`]): a sender that stops before removing
//! the copy, or whose attempt fails, leaves it pending and `envelope serve`'s
//! cleanup retry tries again.

use std::future::Future;

use envelope_email_store::{Database, Draft, ProviderCleanupRetry};
use tracing::{error, info, warn};

use crate::errors::ImapError;
use crate::imap::{self, ImapClient};

/// The two mailbox operations post-send cleanup performs.
pub trait DraftsMailbox {
    /// Every UID in `folder` whose Message-ID header exactly equals
    /// `message_id` after normalization.
    fn exact_message_id_uids(
        &mut self,
        folder: &str,
        message_id: &str,
    ) -> impl Future<Output = Result<Vec<u32>, ImapError>> + Send;

    /// Delete one message by UID (mark `\Deleted`, expunge that UID).
    fn delete_uid(
        &mut self,
        folder: &str,
        uid: u32,
    ) -> impl Future<Output = Result<(), ImapError>> + Send;
}

impl DraftsMailbox for ImapClient {
    async fn exact_message_id_uids(
        &mut self,
        folder: &str,
        message_id: &str,
    ) -> Result<Vec<u32>, ImapError> {
        imap::find_uids_by_exact_message_id(self, folder, message_id).await
    }

    async fn delete_uid(&mut self, folder: &str, uid: u32) -> Result<(), ImapError> {
        imap::delete_message(self, folder, uid).await
    }
}

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
    cleanup_target(db, draft, draft.message_id.as_deref())
}

/// [`resolve_draft_cleanup_target`] for a `sent` row: its Message-ID column
/// now holds the transmitted message, so the Drafts copy's identity comes
/// from the pending cleanup record written with the send.
pub fn resolve_sent_cleanup_target(
    db: &Database,
    draft: &Draft,
) -> Result<DraftCleanupTarget, &'static str> {
    let Some(copy) = draft.pending_provider_draft_cleanup() else {
        return Err("no provider Drafts cleanup pending");
    };
    cleanup_target(db, draft, Some(copy))
}

fn cleanup_target(
    db: &Database,
    draft: &Draft,
    identity: Option<&str>,
) -> Result<DraftCleanupTarget, &'static str> {
    let folder = match db.get_drafts_folder(&draft.account_id) {
        Ok(Some(folder)) => folder,
        Ok(None) => return Err("no detected drafts folder cached; refusing to guess one"),
        Err(_) => return Err("detected-folder cache read failed"),
    };
    let Some(message_id) = identity.and_then(imap::normalize_message_id) else {
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
    /// No message in the folder carries the copy's Message-ID: it is already
    /// gone. Nothing was deleted.
    Absent,
    /// Identity could not be established unambiguously (more than one exact
    /// Message-ID match) — nothing was deleted.
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
/// persisted one. No match is [`ProviderDraftCleanup::Absent`]; several skip
/// (fail closed). A message at the draft's old UID that carries another
/// Message-ID is never touched. The caller supplies a connected client and
/// owns retry/eviction policy.
pub async fn delete_provider_draft_exact<M: DraftsMailbox>(
    client: &mut M,
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
        match client.exact_message_id_uids(&target.folder, stale).await {
            Ok(uids) if uids.len() == 1 => {
                client.delete_uid(&target.folder, uids[0]).await?;
                superseded_uids.push(uids[0]);
            }
            // Absent or ambiguous: nothing safely identifiable to remove.
            Ok(_) => {}
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

    match client
        .exact_message_id_uids(&target.folder, &target.message_id)
        .await?
        .as_slice()
    {
        [uid] => {
            client.delete_uid(&target.folder, *uid).await?;
            Ok(ProviderDraftCleanup::Deleted { uid: *uid })
        }
        [] if !superseded_uids.is_empty() => {
            // The current identity is gone but stale copies were removed. That
            // is a real cleanup, not a skip.
            Ok(ProviderDraftCleanup::Deleted {
                uid: superseded_uids[superseded_uids.len() - 1],
            })
        }
        [] => Ok(ProviderDraftCleanup::Absent),
        _ => Ok(ProviderDraftCleanup::Skipped(
            "provider draft copy not uniquely identified by exact Message-ID",
        )),
    }
}

/// Record one post-send cleanup attempt on the sent row's pending record,
/// and log it (draft id, folder and UID only). A deletion, an absent copy or
/// an ambiguous one closes the record; an error counts against the retry
/// bound, so a later sweep tries again. Returns true when a copy was
/// deleted.
pub fn settle_provider_draft_cleanup(
    db: &Database,
    draft_id: &str,
    folder: Option<&str>,
    result: Result<ProviderDraftCleanup, String>,
) -> bool {
    let folder = folder.unwrap_or("the Drafts folder");
    let (closed, deleted) = match &result {
        Ok(ProviderDraftCleanup::Deleted { uid }) => {
            info!("draft {draft_id}: removed its provider Drafts copy (UID {uid} in {folder})");
            (db.finish_provider_draft_cleanup(draft_id, "deleted"), true)
        }
        Ok(ProviderDraftCleanup::Absent) => {
            info!("draft {draft_id}: its provider Drafts copy is already gone from {folder}");
            (db.finish_provider_draft_cleanup(draft_id, "absent"), false)
        }
        Ok(ProviderDraftCleanup::Skipped(reason)) => {
            warn!("draft {draft_id}: provider Drafts copy left in {folder}: {reason}");
            (
                db.finish_provider_draft_cleanup(draft_id, "ambiguous"),
                false,
            )
        }
        Err(e) => {
            match db.fail_provider_draft_cleanup(draft_id, e) {
                Ok(Some(ProviderCleanupRetry::Pending { attempts })) => warn!(
                    "draft {draft_id}: provider Drafts cleanup failed in {folder} (attempt \
                     {attempts} of {}); the next sweep retries: {e}",
                    envelope_email_store::send_attempts::MAX_PROVIDER_DRAFT_CLEANUP_ATTEMPTS
                ),
                Ok(Some(ProviderCleanupRetry::Abandoned { attempts })) => error!(
                    "draft {draft_id}: provider Drafts cleanup failed {attempts} times; \
                     giving up, so the sent message's copy stays in {folder}: {e}"
                ),
                Ok(None) => {
                    warn!("draft {draft_id}: provider Drafts cleanup failed in {folder}: {e}")
                }
                Err(store) => error!(
                    "draft {draft_id}: provider Drafts cleanup failed in {folder} ({e}), and \
                     the failure could not be recorded: {store}"
                ),
            }
            return false;
        }
    };
    if let Err(e) = closed {
        error!("draft {draft_id}: could not record the provider Drafts cleanup: {e}");
    }
    deleted
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
                "gmail1",
                "to@test.com",
                Some("S"),
                Some("B"),
                None,
                None,
                None,
                None,
                Some("cli"),
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

    // ── Post-send cleanup against a scripted Drafts folder ──────────────

    /// A Drafts folder of `(uid, Message-ID)` pairs. `failing_deletes`
    /// deletes fail the way a dropped connection does before any succeeds.
    #[derive(Default)]
    struct ScriptedDrafts {
        messages: Vec<(u32, String)>,
        deleted: Vec<u32>,
        failing_deletes: usize,
    }

    impl DraftsMailbox for ScriptedDrafts {
        async fn exact_message_id_uids(
            &mut self,
            _folder: &str,
            message_id: &str,
        ) -> Result<Vec<u32>, ImapError> {
            let wanted = imap::normalize_message_id(message_id);
            Ok(self
                .messages
                .iter()
                .filter(|(_, id)| imap::normalize_message_id(id) == wanted)
                .map(|(uid, _)| *uid)
                .collect())
        }

        async fn delete_uid(&mut self, _folder: &str, uid: u32) -> Result<(), ImapError> {
            if self.failing_deletes > 0 {
                self.failing_deletes -= 1;
                return Err(ImapError::Connection("connection reset by peer".into()));
            }
            self.messages.retain(|(u, _)| *u != uid);
            self.deleted.push(uid);
            Ok(())
        }
    }

    fn dead(_: &envelope_email_store::AttemptOwner, _: &str) -> envelope_email_store::Liveness {
        envelope_email_store::Liveness::Dead
    }

    /// A row sent from Drafts UID 41 (`<draft-copy@mac.lan>`) whose sender
    /// stopped after recording acceptance: nothing removed the copy.
    fn sent_and_stopped(db: &Database) -> String {
        use envelope_email_store::{AttemptStart, ClaimMode};
        db.set_detected_folder("gmail1", "drafts", "[Gmail]/Drafts")
            .unwrap();
        let draft = db
            .create_draft(
                "gmail1",
                "to@test.com",
                Some("S"),
                Some("B"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.mark_draft_message_id(&draft.id, "<draft-copy@mac.lan>")
            .unwrap();
        db.update_draft_imap_uid(&draft.id, 41).unwrap();
        let draft = db.get_draft(&draft.id).unwrap().unwrap();
        let start = AttemptStart::new("<sent@mac.lan>", "sweep", None);
        let claim = db
            .claim_send_attempt(&draft.id, draft.revision, ClaimMode::Immediate, &start)
            .unwrap()
            .expect("claim");
        db.finish_attempt_sent(
            &draft.id,
            &claim.token,
            "<sent@mac.lan>",
            serde_json::json!({}),
        )
        .unwrap();
        draft.id
    }

    /// One sweep pass over the pending cleanups, as the dashboard runs it.
    async fn sweep_pass(db: &Database, mailbox: &mut ScriptedDrafts) -> Vec<bool> {
        let pending = db
            .pending_provider_draft_cleanups(chrono::Utc::now(), &dead, 10)
            .unwrap();
        let mut deleted = Vec::new();
        for row in pending {
            let target = resolve_sent_cleanup_target(db, &row).unwrap();
            let result = delete_provider_draft_exact(mailbox, &target)
                .await
                .map_err(|e| e.to_string());
            deleted.push(settle_provider_draft_cleanup(
                db,
                &row.id,
                Some(&target.folder),
                result,
            ));
        }
        deleted
    }

    fn cleanup_record(db: &Database, id: &str) -> serde_json::Value {
        db.get_draft(id).unwrap().unwrap().metadata.unwrap()["provider_draft_cleanup"].clone()
    }

    /// Mailroom trial D: the sender stopped between transmitting and removing
    /// the Drafts copy. The next pass removes exactly that copy.
    #[tokio::test]
    async fn the_next_pass_removes_the_copy_a_stopped_sender_left() {
        let db = seeded_db();
        let id = sent_and_stopped(&db);
        let mut drafts = ScriptedDrafts {
            messages: vec![
                (41, "<draft-copy@mac.lan>".into()),
                (42, "<other@mac.lan>".into()),
            ],
            ..Default::default()
        };

        assert_eq!(sweep_pass(&db, &mut drafts).await, vec![true]);
        assert_eq!(drafts.deleted, vec![41]);
        assert_eq!(drafts.messages, vec![(42, "<other@mac.lan>".to_string())]);
        assert_eq!(cleanup_record(&db, &id)["state"], "done");
        assert_eq!(cleanup_record(&db, &id)["outcome"], "deleted");
        assert!(
            sweep_pass(&db, &mut drafts).await.is_empty(),
            "a settled cleanup is not retried"
        );
    }

    #[tokio::test]
    async fn a_failed_delete_is_retried_by_the_next_pass() {
        let db = seeded_db();
        let id = sent_and_stopped(&db);
        let mut drafts = ScriptedDrafts {
            messages: vec![(41, "<draft-copy@mac.lan>".into())],
            failing_deletes: 1,
            ..Default::default()
        };

        assert_eq!(sweep_pass(&db, &mut drafts).await, vec![false]);
        let record = cleanup_record(&db, &id);
        assert_eq!(record["state"], "pending");
        assert_eq!(record["attempts"], 1);
        assert!(
            record["last_error"]
                .as_str()
                .unwrap()
                .contains("connection reset")
        );

        assert_eq!(sweep_pass(&db, &mut drafts).await, vec![true]);
        assert!(drafts.messages.is_empty());
        assert_eq!(cleanup_record(&db, &id)["state"], "done");
    }

    #[tokio::test]
    async fn a_copy_already_gone_closes_the_cleanup_without_an_error() {
        let db = seeded_db();
        let id = sent_and_stopped(&db);
        let mut drafts = ScriptedDrafts::default();

        assert_eq!(sweep_pass(&db, &mut drafts).await, vec![false]);
        let record = cleanup_record(&db, &id);
        assert_eq!(record["state"], "done");
        assert_eq!(record["outcome"], "absent");
        assert_eq!(record["attempts"], 0, "an absent copy is not a failure");
        assert!(record.get("last_error").is_none());
    }

    /// The copy left UID 41 and a person's message now sits there (or the
    /// person edited the draft, which re-saves it under another Message-ID).
    /// Only the exact Message-ID is ever deleted, never "whatever is at 41".
    #[tokio::test]
    async fn a_different_message_at_the_drafts_uid_is_never_deleted() {
        let db = seeded_db();
        let id = sent_and_stopped(&db);
        let mut drafts = ScriptedDrafts {
            messages: vec![(41, "<human-edit@mac.lan>".into())],
            ..Default::default()
        };

        assert_eq!(sweep_pass(&db, &mut drafts).await, vec![false]);
        assert!(drafts.deleted.is_empty());
        assert_eq!(
            drafts.messages,
            vec![(41, "<human-edit@mac.lan>".to_string())]
        );
        assert_eq!(cleanup_record(&db, &id)["outcome"], "absent");
    }

    #[tokio::test]
    async fn duplicate_copies_are_left_alone() {
        let db = seeded_db();
        let id = sent_and_stopped(&db);
        let mut drafts = ScriptedDrafts {
            messages: vec![
                (41, "<draft-copy@mac.lan>".into()),
                (43, "<draft-copy@mac.lan>".into()),
            ],
            ..Default::default()
        };

        assert_eq!(sweep_pass(&db, &mut drafts).await, vec![false]);
        assert!(drafts.deleted.is_empty());
        assert_eq!(cleanup_record(&db, &id)["outcome"], "ambiguous");
    }

    #[test]
    fn the_sent_target_names_the_drafts_copy_never_the_sent_message() {
        let db = seeded_db();
        let id = sent_and_stopped(&db);
        let row = db.get_draft(&id).unwrap().unwrap();
        assert_eq!(row.message_id.as_deref(), Some("<sent@mac.lan>"));

        let target = resolve_sent_cleanup_target(&db, &row).unwrap();
        assert_eq!(target.folder, "[Gmail]/Drafts");
        assert_eq!(target.message_id, "draft-copy@mac.lan");

        db.finish_provider_draft_cleanup(&id, "deleted").unwrap();
        let row = db.get_draft(&id).unwrap().unwrap();
        assert_eq!(
            resolve_sent_cleanup_target(&db, &row).unwrap_err(),
            "no provider Drafts cleanup pending"
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
