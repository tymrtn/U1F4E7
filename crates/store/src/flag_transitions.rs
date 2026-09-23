// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! `\Seen` transitions observed on the server: the signal that another client
//! (phone, Apple Mail) read a message.
//!
//! Envelope never sets `\Seen` when it reads, so a message that was unseen at
//! one observation and seen at the next was read somewhere else, or marked by
//! Envelope itself. Envelope's own flag writes patch the index row
//! ([`Database::patch_indexed_message_flags`]) so they never read as foreign.
//!
//! The emitted `message_seen` time is when Envelope *observed* the flag, an
//! upper bound on the read time, never the read time itself.

use std::collections::HashMap;

use rusqlite::params;

use crate::db::Database;
use crate::errors::Result;
use crate::event_catalog::MESSAGE_SEEN;
use crate::models::Event;

/// A `message_seen` detected by the dashboard/index refresh.
pub const SEEN_SOURCE_INDEX_REFRESH: &str = "index_refresh";
/// A `message_seen` detected by `envelope watch`'s post-IDLE FLAGS fetch.
pub const SEEN_SOURCE_WATCH: &str = "watch";

/// Flags for one UID as the server reported them. Flag names are in the index
/// spelling (async-imap `Debug`, e.g. `"Seen"`); `"\\Seen"` is accepted too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedFlags {
    pub uid: u32,
    pub message_id: Option<String>,
    pub flags: Vec<String>,
}

/// Does this flag list carry `\Seen`? The index stores async-imap's `Debug`
/// spelling (`"Seen"`); IMAP wire spelling (`"\\Seen"`) is accepted as well.
pub fn flags_has_seen(flags: &[String]) -> bool {
    flags.iter().any(|flag| flag_names_match(flag, "Seen"))
}

/// Compare two flag names across the index (`"Seen"`) and wire (`"\\Seen"`)
/// spellings. System flags are case-insensitive in IMAP.
fn flag_names_match(a: &str, b: &str) -> bool {
    a.trim_start_matches('\\')
        .eq_ignore_ascii_case(b.trim_start_matches('\\'))
}

/// `uid -> (message_id, flags)` for one indexed mailbox generation.
type IndexedFlags = HashMap<u32, (Option<String>, Vec<String>)>;

fn seen_idempotency_key(account_id: &str, folder: &str, uidvalidity: u64, uid: u32) -> String {
    format!("seen:{account_id}:{folder}:{uidvalidity}:{uid}")
}

impl Database {
    /// Current index flags per UID for one mailbox generation. Rows from
    /// another UIDVALIDITY are not comparable and are ignored.
    pub(crate) fn load_indexed_flags(
        &self,
        account_id: &str,
        folder: &str,
        uidvalidity: u64,
    ) -> Result<IndexedFlags> {
        let mut stmt = self.conn().prepare(
            "SELECT uid, message_id, flags_json FROM indexed_message_summaries
             WHERE account_id = ?1 AND folder = ?2 AND uidvalidity = ?3",
        )?;
        let rows = stmt.query_map(params![account_id, folder, uidvalidity as i64], |row| {
            let uid: i64 = row.get(0)?;
            let message_id: Option<String> = row.get(1)?;
            let flags_json: String = row.get(2)?;
            Ok((uid, message_id, flags_json))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (uid, message_id, flags_json) = row?;
            let flags = serde_json::from_str::<Vec<String>>(&flags_json)?;
            out.insert(uid as u32, (message_id, flags));
        }
        Ok(out)
    }

    /// Emit one `message_seen` per UID whose `prior` flags lacked `\Seen` and
    /// whose `observed` flags carry it. A UID with no prior observation emits
    /// nothing: it may simply have arrived read. Idempotent per
    /// `(account, folder, uidvalidity, uid)`. Returns the number of new events.
    pub(crate) fn emit_seen_transitions(
        &self,
        account_id: &str,
        folder: &str,
        uidvalidity: u64,
        prior: &HashMap<u32, Vec<String>>,
        observed: &[ObservedFlags],
        source: &str,
    ) -> Result<usize> {
        let mut emitted = 0;
        for obs in observed {
            let Some(before) = prior.get(&obs.uid) else {
                continue;
            };
            if flags_has_seen(before) || !flags_has_seen(&obs.flags) {
                continue;
            }
            let observed_at = chrono::Utc::now().to_rfc3339();
            let payload = serde_json::json!({
                "uid": obs.uid,
                "message_id": obs.message_id,
                "observed_at": observed_at,
                "source": source,
                "uidvalidity": uidvalidity,
            });
            let event = Event {
                id: uuid::Uuid::new_v4().to_string(),
                account_id: account_id.to_string(),
                event_type: MESSAGE_SEEN.to_string(),
                folder: folder.to_string(),
                uid: Some(i64::from(obs.uid)),
                message_id: obs.message_id.clone(),
                from_addr: None,
                subject: None,
                snippet: None,
                payload: Some(payload.to_string()),
                idempotency_key: Some(seen_idempotency_key(
                    account_id,
                    folder,
                    uidvalidity,
                    obs.uid,
                )),
                secure_pending: false,
                // An observation, not an inbox action: pre-acked so it never
                // lands in the review queue or `envelope code` tail.
                acked_at: Some(observed_at.clone()),
                created_at: observed_at,
            };
            if self.insert_event_idempotent(&event)? {
                emitted += 1;
            }
        }
        Ok(emitted)
    }

    /// Record a FLAGS-only observation (from `envelope watch`). The prior for a
    /// UID is its index row when one exists (the index is what Envelope's own
    /// flag writes patch), else the caller's in-memory `prior`. Existing index
    /// rows take the observed flags; no rows are created, because a FLAGS fetch
    /// carries no envelope to list. Returns the number of `message_seen` events.
    pub fn record_observed_flags(
        &self,
        account_id: &str,
        folder: &str,
        uidvalidity: u64,
        prior: &HashMap<u32, Vec<String>>,
        observed: &[ObservedFlags],
        source: &str,
    ) -> Result<usize> {
        let indexed = self.load_indexed_flags(account_id, folder, uidvalidity)?;
        let mut effective_prior = prior.clone();
        let mut with_ids = Vec::with_capacity(observed.len());
        for obs in observed {
            let mut obs = obs.clone();
            if let Some((message_id, flags)) = indexed.get(&obs.uid) {
                effective_prior.insert(obs.uid, flags.clone());
                if obs.message_id.is_none() {
                    obs.message_id = message_id.clone();
                }
            }
            with_ids.push(obs);
        }

        let emitted = self.emit_seen_transitions(
            account_id,
            folder,
            uidvalidity,
            &effective_prior,
            &with_ids,
            source,
        )?;

        for obs in with_ids.iter().filter(|obs| indexed.contains_key(&obs.uid)) {
            self.conn().execute(
                "UPDATE indexed_message_summaries SET flags_json = ?5
                 WHERE account_id = ?1 AND folder = ?2 AND uidvalidity = ?3 AND uid = ?4",
                params![
                    account_id,
                    folder,
                    uidvalidity as i64,
                    obs.uid as i64,
                    serde_json::to_string(&obs.flags)?,
                ],
            )?;
        }
        Ok(emitted)
    }

    /// Apply one of Envelope's own flag writes to the index so the next
    /// refresh or watch pass does not mistake it for another client's read.
    /// `index_flag` is in the index spelling (see
    /// `envelope_email_transport::imap::index_flag_name`). Returns rows changed;
    /// zero when the message is not indexed, which is not an error.
    pub fn patch_indexed_message_flags(
        &self,
        account_id: &str,
        folder: &str,
        uids: &[u32],
        index_flag: &str,
        add: bool,
    ) -> Result<usize> {
        let mut changed = 0;
        for uid in uids {
            let rows: Vec<(i64, String)> = {
                let mut stmt = self.conn().prepare(
                    "SELECT uidvalidity, flags_json FROM indexed_message_summaries
                     WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
                )?;
                stmt.query_map(params![account_id, folder, *uid as i64], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<std::result::Result<_, _>>()?
            };
            for (uidvalidity, flags_json) in rows {
                let mut flags: Vec<String> = serde_json::from_str(&flags_json)?;
                let present = flags.iter().any(|f| flag_names_match(f, index_flag));
                if add == present {
                    continue;
                }
                if add {
                    flags.push(index_flag.to_string());
                } else {
                    flags.retain(|f| !flag_names_match(f, index_flag));
                }
                changed += self.conn().execute(
                    "UPDATE indexed_message_summaries SET flags_json = ?5
                     WHERE account_id = ?1 AND folder = ?2 AND uidvalidity = ?3 AND uid = ?4",
                    params![
                        account_id,
                        folder,
                        uidvalidity,
                        *uid as i64,
                        serde_json::to_string(&flags)?
                    ],
                )?;
            }
        }
        Ok(changed)
    }

    /// `message_seen` events for one message, oldest first.
    pub fn list_message_seen_events(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
    ) -> Result<Vec<Event>> {
        let ids: Vec<String> = {
            let mut stmt = self.conn().prepare(
                "SELECT id FROM events
                 WHERE account_id = ?1 AND folder = ?2 AND uid = ?3 AND event_type = ?4
                 ORDER BY created_at ASC",
            )?;
            stmt.query_map(
                params![account_id, folder, uid as i64, MESSAGE_SEEN],
                |row| row.get(0),
            )?
            .collect::<std::result::Result<_, _>>()?
        };
        let mut events = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(event) = self.get_event(&id)? {
                events.push(event);
            }
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::IndexedMessageInput;

    fn msg(uid: u32, flags: &[&str]) -> IndexedMessageInput {
        IndexedMessageInput {
            uid,
            message_id: Some(format!("<{uid}@x>")),
            from_addr: "p@example.test".into(),
            to_addr: "me@example.test".into(),
            subject: format!("subject {uid}"),
            date: Some("Tue, 22 Sep 2026 10:00:00 +0000".into()),
            flags: flags.iter().map(|f| f.to_string()).collect(),
            size: 1,
            snippet: None,
            thread_id: None,
        }
    }

    fn seen_events(db: &Database) -> Vec<Event> {
        db.list_events(None, 100)
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == MESSAGE_SEEN)
            .collect()
    }

    #[test]
    fn flags_has_seen_accepts_both_spellings() {
        assert!(flags_has_seen(&["Seen".to_string()]));
        assert!(flags_has_seen(&["\\Seen".to_string()]));
        assert!(flags_has_seen(&[
            "Flagged".to_string(),
            "\\seen".to_string()
        ]));
        assert!(!flags_has_seen(&["Flagged".to_string()]));
        assert!(!flags_has_seen(&[]));
        assert!(!flags_has_seen(&["Custom(\"Seen-ish\")".to_string()]));
    }

    #[test]
    fn refresh_emits_exactly_one_event_for_a_foreign_read() {
        let db = Database::open_memory().unwrap();
        db.upsert_indexed_message_summaries(
            "acc",
            "INBOX",
            7,
            &[msg(1, &[]), msg(2, &[]), msg(3, &["Seen"])],
        )
        .unwrap();
        assert!(seen_events(&db).is_empty(), "first index has no prior");

        db.upsert_indexed_message_summaries(
            "acc",
            "INBOX",
            7,
            &[msg(1, &["Seen"]), msg(2, &[]), msg(3, &["Seen"])],
        )
        .unwrap();

        let events = seen_events(&db);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.uid, Some(1));
        assert_eq!(event.folder, "INBOX");
        assert_eq!(event.message_id.as_deref(), Some("<1@x>"));
        assert_eq!(event.idempotency_key.as_deref(), Some("seen:acc:INBOX:7:1"));
        assert!(event.acked_at.is_some());
        let payload: serde_json::Value =
            serde_json::from_str(event.payload.as_deref().unwrap()).unwrap();
        assert_eq!(payload["uid"], 1);
        assert_eq!(payload["message_id"], "<1@x>");
        assert_eq!(payload["source"], SEEN_SOURCE_INDEX_REFRESH);
        assert!(payload["observed_at"].as_str().is_some());
    }

    #[test]
    fn rerunning_the_same_refresh_emits_nothing_more() {
        let db = Database::open_memory().unwrap();
        db.upsert_indexed_message_summaries("acc", "INBOX", 7, &[msg(1, &[])])
            .unwrap();
        db.upsert_indexed_message_summaries("acc", "INBOX", 7, &[msg(1, &["Seen"])])
            .unwrap();
        db.upsert_indexed_message_summaries("acc", "INBOX", 7, &[msg(1, &["Seen"])])
            .unwrap();
        // Unread again elsewhere, then read again: same message, same key.
        db.upsert_indexed_message_summaries("acc", "INBOX", 7, &[msg(1, &[])])
            .unwrap();
        db.upsert_indexed_message_summaries("acc", "INBOX", 7, &[msg(1, &["Seen"])])
            .unwrap();
        assert_eq!(seen_events(&db).len(), 1);
    }

    #[test]
    fn wire_spelling_prior_and_observation_both_count() {
        let db = Database::open_memory().unwrap();
        db.upsert_indexed_message_summaries("acc", "INBOX", 7, &[msg(1, &["\\Seen"]), msg(2, &[])])
            .unwrap();
        db.upsert_indexed_message_summaries(
            "acc",
            "INBOX",
            7,
            &[msg(1, &["Seen"]), msg(2, &["\\Seen"])],
        )
        .unwrap();
        let events = seen_events(&db);
        assert_eq!(events.len(), 1, "uid 1 was already seen as \\Seen");
        assert_eq!(events[0].uid, Some(2));
    }

    #[test]
    fn uidvalidity_change_is_not_a_transition() {
        let db = Database::open_memory().unwrap();
        db.upsert_indexed_message_summaries("acc", "INBOX", 7, &[msg(1, &[])])
            .unwrap();
        db.upsert_indexed_message_summaries("acc", "INBOX", 8, &[msg(1, &["Seen"])])
            .unwrap();
        assert!(seen_events(&db).is_empty());
    }

    #[test]
    fn own_mark_patches_the_index_and_emits_nothing() {
        let db = Database::open_memory().unwrap();
        db.upsert_indexed_message_summaries("acc", "INBOX", 7, &[msg(1, &["Flagged"])])
            .unwrap();

        let changed = db
            .patch_indexed_message_flags("acc", "INBOX", &[1], "Seen", true)
            .unwrap();
        assert_eq!(changed, 1);
        // Idempotent: already present.
        assert_eq!(
            db.patch_indexed_message_flags("acc", "INBOX", &[1], "Seen", true)
                .unwrap(),
            0
        );

        db.upsert_indexed_message_summaries("acc", "INBOX", 7, &[msg(1, &["Flagged", "Seen"])])
            .unwrap();
        assert!(
            seen_events(&db).is_empty(),
            "own mark is not a foreign read"
        );

        let changed = db
            .patch_indexed_message_flags("acc", "INBOX", &[1], "Seen", false)
            .unwrap();
        assert_eq!(changed, 1);
        let flags = &db.load_indexed_flags("acc", "INBOX", 7).unwrap()[&1].1;
        assert_eq!(flags, &vec!["Flagged".to_string()]);
    }

    #[test]
    fn own_mark_on_an_unindexed_message_changes_nothing() {
        let db = Database::open_memory().unwrap();
        assert_eq!(
            db.patch_indexed_message_flags("acc", "INBOX", &[9], "Seen", true)
                .unwrap(),
            0
        );
    }

    #[test]
    fn observed_flags_prefer_the_index_row_over_the_caller_prior() {
        let db = Database::open_memory().unwrap();
        db.upsert_indexed_message_summaries("acc", "INBOX", 7, &[msg(1, &[])])
            .unwrap();
        // Envelope marked it read; the watch's in-memory map is stale.
        db.patch_indexed_message_flags("acc", "INBOX", &[1], "Seen", true)
            .unwrap();
        let stale: HashMap<u32, Vec<String>> = HashMap::from([(1, vec![])]);
        let observed = [ObservedFlags {
            uid: 1,
            message_id: None,
            flags: vec!["Seen".into()],
        }];
        let emitted = db
            .record_observed_flags("acc", "INBOX", 7, &stale, &observed, SEEN_SOURCE_WATCH)
            .unwrap();
        assert_eq!(emitted, 0);
        assert!(seen_events(&db).is_empty());
    }

    #[test]
    fn observed_flags_emit_and_persist_to_the_index() {
        let db = Database::open_memory().unwrap();
        db.upsert_indexed_message_summaries("acc", "INBOX", 7, &[msg(1, &[])])
            .unwrap();
        let observed = [ObservedFlags {
            uid: 1,
            message_id: None,
            flags: vec!["Seen".into()],
        }];
        let emitted = db
            .record_observed_flags(
                "acc",
                "INBOX",
                7,
                &HashMap::new(),
                &observed,
                SEEN_SOURCE_WATCH,
            )
            .unwrap();
        assert_eq!(emitted, 1);
        let events = seen_events(&db);
        assert_eq!(
            events[0].message_id.as_deref(),
            Some("<1@x>"),
            "id from index"
        );
        let payload: serde_json::Value =
            serde_json::from_str(events[0].payload.as_deref().unwrap()).unwrap();
        assert_eq!(payload["source"], SEEN_SOURCE_WATCH);

        // The index row now says Seen, so the dashboard's next refresh is quiet.
        db.upsert_indexed_message_summaries("acc", "INBOX", 7, &[msg(1, &["Seen"])])
            .unwrap();
        assert_eq!(seen_events(&db).len(), 1);
    }

    #[test]
    fn list_message_seen_events_filters_to_the_message() {
        let db = Database::open_memory().unwrap();
        db.upsert_indexed_message_summaries("acc", "INBOX", 7, &[msg(1, &[]), msg(2, &[])])
            .unwrap();
        db.upsert_indexed_message_summaries(
            "acc",
            "INBOX",
            7,
            &[msg(1, &["Seen"]), msg(2, &["Seen"])],
        )
        .unwrap();
        let one = db.list_message_seen_events("acc", "INBOX", 1).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].uid, Some(1));
        assert!(
            db.list_message_seen_events("acc", "Archive", 1)
                .unwrap()
                .is_empty()
        );
    }
}
