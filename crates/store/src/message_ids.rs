// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! One stored form for Message-IDs: bare `id@host`, no angle brackets.
//!
//! IMAP ENVELOPE hands us `<id@host>` and mail_parser hands us `id@host`, so
//! the message index, the event log and the thread cache used to disagree and
//! any join across them silently matched nothing. Every write to those three
//! tables goes through [`stored_message_id`], and
//! [`Database::normalize_stored_message_ids`] repairs rows written before
//! that, or by a build that still writes brackets.

use rusqlite::params;

use crate::db::Database;
use crate::errors::Result;
use crate::threads::normalize_message_id;

/// The form a Message-ID is stored in. An id that is empty once its brackets
/// are gone is stored as no id at all.
pub fn stored_message_id(raw: Option<&str>) -> Option<String> {
    raw.map(normalize_message_id).filter(|id| !id.is_empty())
}

/// A References header in stored form: its ids, each bare, space-separated.
pub fn stored_references(raw: Option<&str>) -> Option<String> {
    let ids: Vec<String> = raw?
        .split_whitespace()
        .filter_map(|id| stored_message_id(Some(id)))
        .collect();
    (!ids.is_empty()).then(|| ids.join(" "))
}

/// SQL twin of [`stored_message_id`] for the repair (strips spaces, not all
/// Unicode whitespace; Message-IDs carry none).
const STORED_FORM_SQL: &str = "NULLIF(trim(trim(trim(message_id), '<>')), '')";

/// Rows each table had rewritten by one repair pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MessageIdRepair {
    pub index_rows: usize,
    pub events: usize,
    pub thread_messages: usize,
}

impl Database {
    /// Rewrite every stored Message-ID that is not in the stored form. Only
    /// the `message_id` column changes: event payloads (what a webhook
    /// already delivered) and idempotency keys stay as written. Idempotent,
    /// and cheap once nothing is left to repair.
    pub fn normalize_stored_message_ids(&self) -> Result<MessageIdRepair> {
        let tx = self.conn().unchecked_transaction()?;
        let mut repair = MessageIdRepair::default();
        for (table, count) in [
            ("indexed_message_summaries", &mut repair.index_rows),
            ("events", &mut repair.events),
            ("thread_messages", &mut repair.thread_messages),
        ] {
            *count = tx.execute(
                &format!(
                    "UPDATE {table} SET message_id = {STORED_FORM_SQL}
                     WHERE message_id IS NOT {STORED_FORM_SQL}"
                ),
                params![],
            )?;
        }
        tx.commit()?;
        Ok(repair)
    }
}
