// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Correspondent facts for the threat engine's ledger analyzer.
//!
//! Public v1 has no relationship ledger, so these facts come from the rows v1
//! does have: `contacts` (curated and history-derived) and cached
//! `thread_messages` headers. Reads are bounded and local; no IMAP.

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::address_book::parse_address_list;
use crate::db::Database;
use crate::errors::Result;
use crate::relationship_facts::TM_IN_SENT_FOLDER;

/// Known correspondent domains returned per scan (look-alike candidates).
const KNOWN_DOMAIN_LIMIT: i64 = 5000;
/// Thread header rows examined for prior mail with the sender.
const THREAD_SCAN_LIMIT: i64 = 200;
/// Contacts returned for a display-name match.
const NAME_MATCH_LIMIT: i64 = 20;

/// What this mailbox already knows about a sender.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorrespondentFacts {
    /// The sender address has a contact row.
    pub known_contact: bool,
    /// Cached inbound messages from the sender.
    pub prior_inbound: u32,
    /// Cached outbound messages to the sender.
    pub prior_outbound: u32,
    /// Domains of this account's contacts (lowercased, distinct).
    pub known_domains: Vec<String>,
    /// Addresses of contacts whose name equals the sender's display name.
    pub name_matches: Vec<String>,
}

impl Database {
    pub fn correspondent_facts(
        &self,
        account_id: &str,
        from_addr: &str,
        display_name: Option<&str>,
    ) -> Result<CorrespondentFacts> {
        let from = from_addr.trim().to_lowercase();
        let mut facts = CorrespondentFacts::default();
        if from.is_empty() {
            return Ok(facts);
        }

        facts.known_contact = self.conn().query_row(
            "SELECT COUNT(*) FROM contacts WHERE account_id = ?1 AND lower(email) = ?2",
            params![account_id, from],
            |row| row.get::<_, i64>(0),
        )? > 0;

        let mut stmt = self.conn().prepare(
            "SELECT DISTINCT lower(substr(email, instr(email, '@') + 1))
             FROM contacts
             WHERE account_id = ?1 AND instr(email, '@') > 0
             LIMIT ?2",
        )?;
        facts.known_domains = stmt
            .query_map(params![account_id, KNOWN_DOMAIN_LIMIT], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let pattern = format!("%{from}%");
        // Outbound means a copy in the detected Sent folder: rows an earlier
        // indexer flagged outbound for a self-From elsewhere (a spoof in
        // INBOX) are not mail the account sent.
        let mut stmt = self.conn().prepare(&format!(
            "SELECT tm.is_outbound = 1 AND {TM_IN_SENT_FOLDER},
                    tm.from_address, tm.to_addresses, tm.cc_addresses
             FROM thread_messages tm
             JOIN threads t ON t.thread_id = tm.thread_id
             WHERE t.account_id = ?1
               AND (lower(tm.from_address) LIKE ?2 OR lower(tm.to_addresses) LIKE ?2
                    OR lower(tm.cc_addresses) LIKE ?2)
             ORDER BY tm.id DESC
             LIMIT ?3"
        ))?;
        let rows = stmt.query_map(params![account_id, pattern, THREAD_SCAN_LIMIT], |row| {
            Ok((
                row.get::<_, bool>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (outbound, sender, to, cc) = row?;
            let has = |raw: Option<&str>| {
                raw.map(parse_address_list)
                    .unwrap_or_default()
                    .iter()
                    .any(|a| a.email == from)
            };
            if outbound && (has(to.as_deref()) || has(cc.as_deref())) {
                facts.prior_outbound += 1;
            } else if !outbound && has(sender.as_deref()) {
                facts.prior_inbound += 1;
            }
        }

        if let Some(name) = display_name.map(str::trim).filter(|n| n.len() >= 3) {
            let mut stmt = self.conn().prepare(
                "SELECT lower(email) FROM contacts
                 WHERE account_id = ?1 AND lower(trim(name)) = lower(?2)
                 LIMIT ?3",
            )?;
            facts.name_matches = stmt
                .query_map(params![account_id, name, NAME_MATCH_LIMIT], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
        }
        Ok(facts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Contact;

    fn contact(email: &str, name: Option<&str>) -> Contact {
        Contact {
            id: uuid::Uuid::new_v4().to_string(),
            account_id: "acc".to_string(),
            email: email.to_string(),
            name: name.map(str::to_string),
            tags: "[]".to_string(),
            notes: None,
            message_count: 1,
            first_seen: None,
            last_seen: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn facts_come_from_contacts_and_cached_threads() {
        let db = Database::open_memory().unwrap();
        db.upsert_contact(&contact("ceo@acme.example", Some("Dana Chief")))
            .unwrap();
        db.upsert_contact(&contact("bob@supplier.example", None))
            .unwrap();
        let thread = db
            .create_thread(
                "hello",
                "2026-09-01T00:00:00Z",
                "2026-09-01T00:00:00Z",
                "acc",
            )
            .unwrap();
        db.upsert_thread_message(
            &thread.thread_id,
            1,
            Some("m1@x"),
            None,
            None,
            "INBOX",
            "Bob <bob@supplier.example>",
            "me@example.org",
            None,
            None,
            "2026-09-01T00:00:00Z",
            "hello",
            false,
            None,
        )
        .unwrap();

        let facts = db
            .correspondent_facts("acc", "Bob@Supplier.example", None)
            .unwrap();
        assert!(facts.known_contact);
        assert_eq!(facts.prior_inbound, 1);
        assert_eq!(facts.prior_outbound, 0);
        let mut domains = facts.known_domains.clone();
        domains.sort();
        assert_eq!(domains, vec!["acme.example", "supplier.example"]);

        let impostor = db
            .correspondent_facts("acc", "dana.chief@freemail.example", Some("Dana Chief"))
            .unwrap();
        assert!(!impostor.known_contact);
        assert_eq!(impostor.name_matches, vec!["ceo@acme.example"]);
    }

    /// An earlier indexer marked any message whose From was the account as
    /// outbound, in any folder. A spoofed "From: me, To: attacker" in INBOX
    /// must not count as having written to the attacker; the Sent copy does.
    #[test]
    fn only_sent_folder_copies_count_as_prior_outbound() {
        let db = Database::open_memory().unwrap();
        db.set_detected_folder("acc", "sent", "Sent").unwrap();
        for (uid, folder, to) in [
            (1, "INBOX", "attacker@evil.example"),
            (2, "Junk", "attacker@evil.example"),
            (3, "Sent", "friend@good.example"),
        ] {
            let thread = db
                .create_thread(
                    "hello",
                    "2026-09-01T00:00:00Z",
                    "2026-09-01T00:00:00Z",
                    "acc",
                )
                .unwrap();
            db.upsert_thread_message(
                &thread.thread_id,
                uid,
                Some(&format!("m{uid}@x")),
                None,
                None,
                folder,
                "me@example.org",
                to,
                None,
                None,
                "2026-09-01T00:00:00Z",
                "hello",
                true,
                None,
            )
            .unwrap();
        }

        let attacker = db
            .correspondent_facts("acc", "attacker@evil.example", None)
            .unwrap();
        assert_eq!(attacker.prior_outbound, 0);
        let friend = db
            .correspondent_facts("acc", "friend@good.example", None)
            .unwrap();
        assert_eq!(friend.prior_outbound, 1);
    }
}
