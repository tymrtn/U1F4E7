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

    /// Store the IMAP UID assigned by the server after APPEND to the Drafts folder.
    pub fn update_draft_imap_uid(&self, id: &str, imap_uid: u32) -> Result<()> {
        self.conn().execute(
            "UPDATE drafts SET imap_uid = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![imap_uid as i64, id],
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
