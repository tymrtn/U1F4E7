// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::db::Database;
use crate::errors::{Result, StoreError};
use crate::models::{Draft, DraftStatus};
use rusqlite::params;
use uuid::Uuid;

impl Database {
    pub fn create_draft(
        &self,
        account_id: &str,
        to_addr: &str,
        subject: Option<&str>,
        text_content: Option<&str>,
        html_content: Option<&str>,
        in_reply_to: Option<&str>,
        cc_addr: Option<&str>,
        bcc_addr: Option<&str>,
        created_by: Option<&str>,
    ) -> Result<Draft> {
        let id = Uuid::new_v4().to_string();

        self.conn().execute(
            "INSERT INTO drafts (id, account_id, to_addr, subject, text_content, html_content,
             in_reply_to, cc_addr, bcc_addr, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                id,
                account_id,
                to_addr,
                subject,
                text_content,
                html_content,
                in_reply_to,
                cc_addr,
                bcc_addr,
                created_by
            ],
        )?;

        self.get_draft(&id)?
            .ok_or_else(|| StoreError::DraftNotFound(id))
    }

    pub fn get_draft(&self, id: &str) -> Result<Option<Draft>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, status, to_addr, cc_addr, bcc_addr, reply_to, subject,
                    text_content, html_content, in_reply_to, metadata, attachments, message_id,
                    send_after, snoozed_until, created_at, updated_at, sent_at, created_by,
                    imap_uid
             FROM drafts WHERE id = ?1",
        )?;

        let draft = stmt
            .query_row(params![id], |row| {
                let status_str: String = row.get(2)?;
                let metadata_str: Option<String> = row.get(11)?;
                let attachments_str: String = row.get(12)?;
                let imap_uid_i64: Option<i64> = row.get(20)?;
                Ok(Draft {
                    id: row.get(0)?,
                    account_id: row.get(1)?,
                    status: status_str.parse().unwrap_or(DraftStatus::Draft),
                    to_addr: row.get(3)?,
                    cc_addr: row.get(4)?,
                    bcc_addr: row.get(5)?,
                    reply_to: row.get(6)?,
                    subject: row.get(7)?,
                    text_content: row.get(8)?,
                    html_content: row.get(9)?,
                    in_reply_to: row.get(10)?,
                    metadata: metadata_str.and_then(|s| serde_json::from_str(&s).ok()),
                    attachments: serde_json::from_str(&attachments_str).unwrap_or_default(),
                    message_id: row.get(13)?,
                    send_after: row.get(14)?,
                    snoozed_until: row.get(15)?,
                    created_at: row.get(16)?,
                    updated_at: row.get(17)?,
                    sent_at: row.get(18)?,
                    created_by: row.get(19)?,
                    imap_uid: imap_uid_i64.map(|v| v as u32),
                })
            })
            .optional()?;

        Ok(draft)
    }

    /// Find the active local draft that corresponds to an IMAP Drafts-folder UID.
    ///
    /// Dashboard links often arrive as `/messages/<imap_uid>?folder=<Drafts>`
    /// because IMAP clients identify the server-side draft by UID. Envelope's
    /// review surface, however, is the local draft row. This lookup lets the
    /// dashboard prioritize the reviewable local draft instead of showing a raw
    /// read-only message/composer detour.
    pub fn get_draft_by_imap_uid(&self, account_id: &str, imap_uid: u32) -> Result<Option<Draft>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, status, to_addr, cc_addr, bcc_addr, reply_to, subject,
                    text_content, html_content, in_reply_to, metadata, attachments, message_id,
                    send_after, snoozed_until, created_at, updated_at, sent_at, created_by,
                    imap_uid
             FROM drafts
             WHERE account_id = ?1
               AND imap_uid = ?2
               AND status IN ('draft', 'pending_review', 'blocked')
             ORDER BY updated_at DESC
             LIMIT 1",
        )?;

        let draft = stmt
            .query_row(params![account_id, imap_uid as i64], Self::map_draft)
            .optional()?;
        Ok(draft)
    }

    pub fn list_drafts(
        &self,
        account_id: &str,
        status: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Draft>> {
        let sql = if status.is_some() {
            "SELECT id, account_id, status, to_addr, cc_addr, bcc_addr, reply_to, subject,
                    text_content, html_content, in_reply_to, metadata, attachments, message_id,
                    send_after, snoozed_until, created_at, updated_at, sent_at, created_by,
                    imap_uid
             FROM drafts WHERE account_id = ?1 AND status = ?2
             ORDER BY updated_at DESC LIMIT ?3 OFFSET ?4"
        } else {
            "SELECT id, account_id, status, to_addr, cc_addr, bcc_addr, reply_to, subject,
                    text_content, html_content, in_reply_to, metadata, attachments, message_id,
                    send_after, snoozed_until, created_at, updated_at, sent_at, created_by,
                    imap_uid
             FROM drafts WHERE account_id = ?1
             ORDER BY updated_at DESC LIMIT ?3 OFFSET ?4"
        };

        let mut stmt = self.conn().prepare(sql)?;
        let rows = if let Some(s) = status {
            stmt.query_map(params![account_id, s, limit, offset], Self::map_draft)?
        } else {
            stmt.query_map(params![account_id, "", limit, offset], Self::map_draft)?
        };

        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn update_draft_status(&self, id: &str, status: DraftStatus) -> Result<()> {
        let current = self
            .get_draft(id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.to_string()))?;

        if !current.status.is_editable() {
            return Err(StoreError::DraftNotEditable(
                current.status.as_str().to_string(),
            ));
        }

        self.conn().execute(
            "UPDATE drafts SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![status.as_str(), id],
        )?;

        Ok(())
    }

    pub fn update_draft_content(
        &self,
        id: &str,
        to_addr: Option<&str>,
        cc_addr: Option<&str>,
        bcc_addr: Option<&str>,
        subject: Option<&str>,
        text_content: Option<&str>,
        html_content: Option<&str>,
    ) -> Result<Draft> {
        let current = self
            .get_draft(id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.to_string()))?;
        if !current.status.is_editable() {
            return Err(StoreError::DraftNotEditable(
                current.status.as_str().to_string(),
            ));
        }

        self.conn().execute(
            "UPDATE drafts SET
                to_addr = COALESCE(?1, to_addr),
                cc_addr = ?2,
                bcc_addr = ?3,
                subject = COALESCE(?4, subject),
                text_content = COALESCE(?5, text_content),
                html_content = COALESCE(?6, html_content),
                updated_at = datetime('now')
             WHERE id = ?7",
            params![
                to_addr,
                cc_addr,
                bcc_addr,
                subject,
                text_content,
                html_content,
                id
            ],
        )?;

        self.get_draft(id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.to_string()))
    }

    pub fn mark_draft_sent(&self, id: &str, message_id: Option<&str>) -> Result<()> {
        self.conn().execute(
            "UPDATE drafts SET status = 'sent', message_id = ?1,
             sent_at = datetime('now'), updated_at = datetime('now') WHERE id = ?2",
            params![message_id, id],
        )?;
        Ok(())
    }

    pub fn discard_draft(&self, id: &str) -> Result<bool> {
        let rows = self.conn().execute(
            "UPDATE drafts SET status = 'discarded', updated_at = datetime('now')
             WHERE id = ?1 AND status IN ('draft', 'pending_review', 'blocked')",
            params![id],
        )?;
        Ok(rows > 0)
    }

    /// Replace the draft's `attachments` JSON array.
    ///
    /// For scheduled sends, attachment bytes are snapshotted at schedule time
    /// (base64-encoded inside each entry) so a later send sweep does not depend
    /// on the original files still existing. Each entry is expected to carry at
    /// least `filename`, `content_type`, and `size`; scheduled-send entries also
    /// carry `data_base64`. Never log or echo the `data_base64` field.
    pub fn update_draft_attachments(
        &self,
        id: &str,
        attachments: &[serde_json::Value],
    ) -> Result<()> {
        let serialized = serde_json::to_string(attachments)?;
        self.conn().execute(
            "UPDATE drafts SET attachments = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![serialized, id],
        )?;
        Ok(())
    }

    /// Set the `send_after` timestamp on a draft (for scheduled sending).
    pub fn update_draft_send_after(&self, id: &str, send_after: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE drafts SET send_after = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![send_after, id],
        )?;
        Ok(())
    }

    /// Query drafts that are due for scheduled sending.
    pub fn list_drafts_due_for_send(&self) -> Result<Vec<Draft>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, status, to_addr, cc_addr, bcc_addr, reply_to, subject,
                    text_content, html_content, in_reply_to, metadata, attachments, message_id,
                    send_after, snoozed_until, created_at, updated_at, sent_at, created_by,
                    imap_uid
             FROM drafts
             WHERE status = 'draft'
               AND send_after IS NOT NULL
               AND datetime(send_after) <= datetime('now')
             ORDER BY send_after ASC",
        )?;

        let rows = stmt.query_map([], Self::map_draft)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Store the IMAP UID assigned by the server after APPEND to the Drafts folder.
    pub fn update_draft_imap_uid(&self, id: &str, imap_uid: u32) -> Result<()> {
        self.conn().execute(
            "UPDATE drafts SET imap_uid = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![imap_uid as i64, id],
        )?;
        Ok(())
    }

    /// Count drafts with actionable/pending status: `draft` or `pending_review`.
    /// Pass `None` to count across all accounts.
    pub fn count_active_drafts(&self, account_id: Option<&str>) -> Result<u64> {
        let count: i64 = if let Some(aid) = account_id {
            self.conn().query_row(
                "SELECT COUNT(*) FROM drafts
                 WHERE account_id = ?1 AND status IN ('draft', 'pending_review')",
                params![aid],
                |row| row.get(0),
            )?
        } else {
            self.conn().query_row(
                "SELECT COUNT(*) FROM drafts WHERE status IN ('draft', 'pending_review')",
                [],
                |row| row.get(0),
            )?
        };
        Ok(count as u64)
    }

    /// Replace the draft's `metadata` JSON blob.
    ///
    /// Used to persist contextual reply/forward state (draft_kind, source
    /// folder/uid/message_id, references, quote/forward block, signature state,
    /// preview metadata) so the full draft can be reconstructed and sent later
    /// without the original message in context.
    pub fn set_draft_metadata(&self, id: &str, metadata: &serde_json::Value) -> Result<()> {
        let serialized = serde_json::to_string(metadata)?;
        self.conn().execute(
            "UPDATE drafts SET metadata = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![serialized, id],
        )?;
        Ok(())
    }

    /// Store the RFC822 Message-ID for a draft (set during IMAP APPEND).
    pub fn mark_draft_message_id(&self, id: &str, message_id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE drafts SET message_id = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![message_id, id],
        )?;
        Ok(())
    }

    fn map_draft(row: &rusqlite::Row<'_>) -> rusqlite::Result<Draft> {
        let status_str: String = row.get(2)?;
        let metadata_str: Option<String> = row.get(11)?;
        let attachments_str: String = row.get(12)?;
        let imap_uid_i64: Option<i64> = row.get(20)?;
        Ok(Draft {
            id: row.get(0)?,
            account_id: row.get(1)?,
            status: status_str.parse().unwrap_or(DraftStatus::Draft),
            to_addr: row.get(3)?,
            cc_addr: row.get(4)?,
            bcc_addr: row.get(5)?,
            reply_to: row.get(6)?,
            subject: row.get(7)?,
            text_content: row.get(8)?,
            html_content: row.get(9)?,
            in_reply_to: row.get(10)?,
            metadata: metadata_str.and_then(|s| serde_json::from_str(&s).ok()),
            attachments: serde_json::from_str(&attachments_str).unwrap_or_default(),
            message_id: row.get(13)?,
            send_after: row.get(14)?,
            snoozed_until: row.get(15)?,
            created_at: row.get(16)?,
            updated_at: row.get(17)?,
            sent_at: row.get(18)?,
            created_by: row.get(19)?,
            imap_uid: imap_uid_i64.map(|v| v as u32),
        })
    }
}

trait OptionalExt<T> {
    fn optional(self) -> std::result::Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalExt<T> for std::result::Result<T, rusqlite::Error> {
    fn optional(self) -> std::result::Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(val) => Ok(Some(val)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn setup() -> Database {
        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acc1', 'Test', 'test@test.com', 'test.com', 'smtp.test.com', 587,
                         'imap.test.com', 993, 'encrypted')",
                [],
            )
            .unwrap();
        db
    }

    #[test]
    fn create_and_get_draft() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        assert_eq!(draft.to_addr, "to@test.com");
        assert_eq!(draft.status, DraftStatus::Draft);
        assert_eq!(draft.imap_uid, None);

        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(fetched.id, draft.id);
        assert_eq!(fetched.imap_uid, None);
    }

    #[test]
    fn discard_draft() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Sub"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        assert!(db.discard_draft(&draft.id).unwrap());
        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(fetched.status, DraftStatus::Discarded);
    }

    #[test]
    fn mark_draft_sent_sets_status_and_sent_at() {
        // Regression for the stale-draft incident: a successful send must move
        // the local row to status=sent AND stamp sent_at. A draft that has been
        // "sent" must never remain status=draft with a null sent_at.
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(draft.status, DraftStatus::Draft);
        assert!(draft.sent_at.is_none());

        db.mark_draft_sent(&draft.id, Some("<mid@host>")).unwrap();

        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(fetched.status, DraftStatus::Sent);
        assert!(
            fetched.sent_at.is_some(),
            "sent_at must be stamped after a successful send"
        );
        assert_eq!(fetched.message_id.as_deref(), Some("<mid@host>"));
    }

    #[test]
    fn cooldown_queue_defers_send_and_stays_draft() {
        // Regression for the "agents send too fast" incident: an allowed send
        // queues into the outbox with a FUTURE send_after instead of
        // transmitting immediately. Until the cooldown elapses, the draft is
        // NOT returned by the scheduled-send sweep and remains status=draft
        // (never sent). A due draft (past send_after) IS returned.
        let db = setup();
        let queued = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Queued"),
                Some("Body"),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        // 1 hour in the future — well past any reasonable default cooldown.
        let future = (chrono::Utc::now() + chrono::Duration::hours(1))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        db.update_draft_send_after(&queued.id, &future).unwrap();

        let due = db.list_drafts_due_for_send().unwrap();
        assert!(
            !due.iter().any(|d| d.id == queued.id),
            "a queued draft within its cooldown must not be due for send"
        );
        let fetched = db.get_draft(&queued.id).unwrap().unwrap();
        assert_eq!(fetched.status, DraftStatus::Draft);
        assert!(fetched.sent_at.is_none());
        assert_eq!(fetched.send_after.as_deref(), Some(future.as_str()));

        // Once the cooldown has elapsed (past send_after), the sweep sees it.
        let past = (chrono::Utc::now() - chrono::Duration::minutes(1))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        db.update_draft_send_after(&queued.id, &past).unwrap();
        let due = db.list_drafts_due_for_send().unwrap();
        assert!(
            due.iter().any(|d| d.id == queued.id),
            "a draft past its cooldown must be due for send"
        );
    }

    #[test]
    fn update_imap_uid() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        assert_eq!(draft.imap_uid, None);

        db.update_draft_imap_uid(&draft.id, 42).unwrap();
        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(fetched.imap_uid, Some(42));
    }

    #[test]
    fn get_draft_by_imap_uid_prioritizes_active_local_draft() {
        let db = setup();
        let active = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Active"),
                Some("Body"),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        db.update_draft_imap_uid(&active.id, 38103).unwrap();

        let discarded = db
            .create_draft(
                "acc1",
                "old@test.com",
                Some("Old"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        db.update_draft_imap_uid(&discarded.id, 38103).unwrap();
        db.discard_draft(&discarded.id).unwrap();

        let found = db.get_draft_by_imap_uid("acc1", 38103).unwrap().unwrap();
        assert_eq!(found.id, active.id);
        assert_eq!(found.imap_uid, Some(38103));
        assert!(db.get_draft_by_imap_uid("acc1", 999).unwrap().is_none());
    }

    #[test]
    fn mark_message_id() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        assert_eq!(draft.message_id, None);

        db.mark_draft_message_id(&draft.id, "<test@example.com>")
            .unwrap();
        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(fetched.message_id, Some("<test@example.com>".to_string()));
    }

    #[test]
    fn count_active_drafts_excludes_sent_and_discarded() {
        let db = setup();

        // Create two active drafts
        let d1 = db
            .create_draft(
                "acc1",
                "a@test.com",
                Some("Active"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let _d2 = db
            .create_draft(
                "acc1",
                "b@test.com",
                Some("Active2"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        // Promote one to pending_review
        db.update_draft_status(&d1.id, DraftStatus::PendingReview)
            .unwrap();

        // Create a sent draft and a discarded draft (historical, should not count)
        let sent = db
            .create_draft(
                "acc1",
                "c@test.com",
                Some("Sent"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        db.mark_draft_sent(&sent.id, None).unwrap();

        let discard = db
            .create_draft(
                "acc1",
                "d@test.com",
                Some("Discarded"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        db.discard_draft(&discard.id).unwrap();

        // Only the two active drafts (draft + pending_review) should count
        let count = db.count_active_drafts(None).unwrap();
        assert_eq!(count, 2);

        // Account-scoped count matches
        let scoped = db.count_active_drafts(Some("acc1")).unwrap();
        assert_eq!(scoped, 2);
    }

    #[test]
    fn set_and_read_draft_metadata_round_trips() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("S"),
                Some("B"),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        let meta = serde_json::json!({
            "draft_kind": "reply",
            "source": {"folder": "INBOX", "uid": 42, "message_id": "parent@x"},
            "references": ["a@x", "parent@x"],
            "signature_applied": false,
        });
        db.set_draft_metadata(&draft.id, &meta).unwrap();

        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        let stored = fetched.metadata.expect("metadata persisted");
        assert_eq!(stored["draft_kind"], "reply");
        assert_eq!(stored["source"]["uid"], 42);
        assert_eq!(stored["references"][1], "parent@x");
    }

    #[test]
    fn update_draft_attachments_round_trips() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        assert!(draft.attachments.is_empty());

        let attachments = vec![
            serde_json::json!({
                "filename": "packet.txt",
                "content_type": "text/plain",
                "size": 5,
                "data_base64": "aGVsbG8=",
            }),
            serde_json::json!({
                "filename": "report.pdf",
                "content_type": "application/pdf",
                "size": 3,
                "data_base64": "Zm9v",
            }),
        ];
        db.update_draft_attachments(&draft.id, &attachments)
            .unwrap();

        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(fetched.attachments.len(), 2);
        assert_eq!(fetched.attachments[0]["filename"], "packet.txt");
        assert_eq!(fetched.attachments[0]["size"], 5);
        assert_eq!(fetched.attachments[1]["data_base64"], "Zm9v");
    }

    #[test]
    fn list_drafts_includes_imap_uid() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        db.update_draft_imap_uid(&draft.id, 99).unwrap();

        let drafts = db.list_drafts("acc1", Some("draft"), 100, 0).unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].imap_uid, Some(99));
    }
}
