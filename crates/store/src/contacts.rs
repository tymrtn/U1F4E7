// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::db::Database;
use crate::errors::Result;
use crate::models::Contact;

impl Database {
    /// Add or update a contact.
    ///
    /// Either way the row comes out manually managed (`history_derived = 0`),
    /// including when this lands on a row the address-history derivation
    /// invented: a contact someone has chosen to curate is no longer the
    /// derivation's to delete when its last cached source disappears. See
    /// `crate::address_book` for the ownership split.
    ///
    /// The row is found on `lower(email)`, not on the `UNIQUE(account_id,
    /// email)` key, because that constraint is case-sensitive and the
    /// derivation writes lowercase. Curating `Alice@Example.com` after history
    /// invented `alice@example.com` has to reach the derived row: leaving it to
    /// the constraint inserts a second row instead, which keeps the derived
    /// name and count ranking ahead of the curated ones in the dropdown and
    /// leaves the derived twin sweepable by the next rebuild. This is the same
    /// match `merge_observation` makes from the other direction, so the two
    /// paths cannot invent rows for each other.
    ///
    /// The derived counters are left alone, so a row taken over this way keeps
    /// the interaction signal history earned for it. Existing rows also keep
    /// their stored spelling: an address is one identity case-folded, but RFC
    /// 5321 §2.4 leaves the local part case-sensitive, so nothing here rewrites
    /// an address someone already has on file.
    pub fn upsert_contact(&self, contact: &Contact) -> Result<()> {
        let conn = self.conn();
        let tx = conn.unchecked_transaction()?;

        // Scoped to the account: the same address under two accounts is two
        // contacts, and one must never be curated through the other.
        let existing: Option<String> = {
            use rusqlite::OptionalExtension;
            tx.query_row(
                "SELECT id FROM contacts
                 WHERE account_id = ?1 AND lower(email) = lower(?2)
                 ORDER BY history_derived ASC, message_count DESC, history_count DESC
                 LIMIT 1",
                rusqlite::params![contact.account_id, contact.email],
                |row| row.get(0),
            )
            .optional()?
        };

        match existing {
            // Curated fields land on the row that is already there. `tags` and
            // `message_count` are assignments because the caller owns them
            // outright; `name` and `notes` coalesce so an add that supplies
            // neither does not blank what is on file.
            //
            // The timestamps coalesce too, and for a sharper reason: they are
            // observations, not curation. `envelope contacts add` has none to
            // offer and passes `None` for both, so assigning `last_seen`
            // outright erased the recency the derivation earned — unrecoverably,
            // since `suggest_addresses` breaks ties on it and no later rebuild
            // re-reads the headers that row was derived from. `first_seen` keeps
            // what is on file ahead of what is supplied: the earliest
            // observation of a contact is not something a later add can move,
            // though a supplied value does fill a row that never had one.
            Some(id) => {
                tx.execute(
                    "UPDATE contacts SET
                        name = COALESCE(?2, name),
                        tags = ?3,
                        notes = COALESCE(?4, notes),
                        message_count = ?5,
                        first_seen = COALESCE(first_seen, ?6),
                        last_seen = COALESCE(?7, last_seen),
                        history_derived = 0,
                        updated_at = datetime('now')
                     WHERE id = ?1",
                    rusqlite::params![
                        id,
                        contact.name,
                        contact.tags,
                        contact.notes,
                        contact.message_count,
                        contact.first_seen,
                        contact.last_seen,
                    ],
                )?;
            }
            None => {
                tx.execute(
                    "INSERT INTO contacts (id, account_id, email, name, tags, notes, message_count, first_seen, last_seen, created_at, updated_at, history_derived)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0)",
                    rusqlite::params![
                        contact.id,
                        contact.account_id,
                        contact.email,
                        contact.name,
                        contact.tags,
                        contact.notes,
                        contact.message_count,
                        contact.first_seen,
                        contact.last_seen,
                        contact.created_at,
                        contact.updated_at,
                    ],
                )?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// Get a contact by email for an account.
    ///
    /// Matched case-insensitively, like every other address lookup here: a
    /// contact curated as `Alice@Example.com` and one history derived as
    /// `alice@example.com` are one contact, and `upsert_contact` keeps them one
    /// row. Ordered so a pre-existing pair of case variants resolves to the
    /// manual row rather than whichever SQLite reaches first.
    pub fn get_contact(&self, account_id: &str, email: &str) -> Result<Option<Contact>> {
        use rusqlite::OptionalExtension;
        let contact = self
            .conn()
            .query_row(
                "SELECT id, account_id, email, name, tags, notes, message_count, first_seen, last_seen, created_at, updated_at
                 FROM contacts WHERE account_id = ?1 AND lower(email) = lower(?2)
                 ORDER BY history_derived ASC, message_count DESC, history_count DESC
                 LIMIT 1",
                rusqlite::params![account_id, email],
                |row: &rusqlite::Row| {
                    Ok(Contact {
                        id: row.get(0)?,
                        account_id: row.get(1)?,
                        email: row.get(2)?,
                        name: row.get(3)?,
                        tags: row.get(4)?,
                        notes: row.get(5)?,
                        message_count: row.get(6)?,
                        first_seen: row.get(7)?,
                        last_seen: row.get(8)?,
                        created_at: row.get(9)?,
                        updated_at: row.get(10)?,
                    })
                },
            )
            .optional()?;
        Ok(contact)
    }

    /// List contacts for an account, optionally filtered by tag.
    pub fn list_contacts(
        &self,
        account_id: &str,
        tag_filter: Option<&str>,
    ) -> Result<Vec<Contact>> {
        let (sql, params): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = match tag_filter {
            Some(tag) => (
                "SELECT id, account_id, email, name, tags, notes, message_count, first_seen, last_seen, created_at, updated_at
                 FROM contacts WHERE account_id = ?1 AND tags LIKE ?2 ORDER BY last_seen DESC",
                vec![
                    Box::new(account_id.to_string()),
                    Box::new(format!("%\"{tag}\"%")),
                ],
            ),
            None => (
                "SELECT id, account_id, email, name, tags, notes, message_count, first_seen, last_seen, created_at, updated_at
                 FROM contacts WHERE account_id = ?1 ORDER BY last_seen DESC",
                vec![Box::new(account_id.to_string())],
            ),
        };

        let mut stmt = self.conn().prepare(sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(params.iter()),
            |row: &rusqlite::Row| {
                Ok(Contact {
                    id: row.get(0)?,
                    account_id: row.get(1)?,
                    email: row.get(2)?,
                    name: row.get(3)?,
                    tags: row.get(4)?,
                    notes: row.get(5)?,
                    message_count: row.get(6)?,
                    first_seen: row.get(7)?,
                    last_seen: row.get(8)?,
                    created_at: row.get(9)?,
                    updated_at: row.get(10)?,
                })
            },
        )?;
        Ok(rows
            .filter_map(|r: std::result::Result<Contact, rusqlite::Error>| r.ok())
            .collect())
    }

    /// Delete a contact by email.
    ///
    /// Case-insensitive, so deleting the address someone can see in the list
    /// removes it whatever spelling the row was created with — including a
    /// legacy case-variant pair that predates `upsert_contact` reconciling them.
    pub fn delete_contact(&self, account_id: &str, email: &str) -> Result<bool> {
        let deleted = self.conn().execute(
            "DELETE FROM contacts WHERE account_id = ?1 AND lower(email) = lower(?2)",
            rusqlite::params![account_id, email],
        )?;
        Ok(deleted > 0)
    }

    /// Get contact tags for a sender email (used by rules engine).
    /// Returns empty vec if no contact exists.
    pub fn get_contact_tags(&self, account_id: &str, email: &str) -> Result<Vec<String>> {
        match self.get_contact(account_id, email)? {
            Some(contact) => {
                let tags: Vec<String> = serde_json::from_str(&contact.tags).unwrap_or_default();
                Ok(tags)
            }
            None => Ok(vec![]),
        }
    }

    /// Add a tag to a contact's tag list.
    pub fn add_contact_tag(&self, account_id: &str, email: &str, tag: &str) -> Result<bool> {
        if let Some(contact) = self.get_contact(account_id, email)? {
            let mut tags: Vec<String> = serde_json::from_str(&contact.tags).unwrap_or_default();
            if !tags.contains(&tag.to_string()) {
                tags.push(tag.to_string());
                self.write_contact_tags(&contact.id, &tags)?;
            }
            self.take_contact_ownership(&contact.id)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Remove a tag from a contact's tag list.
    pub fn remove_contact_tag(&self, account_id: &str, email: &str, tag: &str) -> Result<bool> {
        if let Some(contact) = self.get_contact(account_id, email)? {
            let mut tags: Vec<String> = serde_json::from_str(&contact.tags).unwrap_or_default();
            let before = tags.len();
            tags.retain(|t| t != tag);
            if tags.len() < before {
                self.write_contact_tags(&contact.id, &tags)?;
            }
            self.take_contact_ownership(&contact.id)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Written by row id rather than by address: the caller has already
    /// resolved the contact, and an account carrying a legacy pair of case
    /// variants must not have both rewritten by one tag edit.
    fn write_contact_tags(&self, id: &str, tags: &[String]) -> Result<()> {
        let tags_json = serde_json::to_string(tags).unwrap_or_else(|_| "[]".to_string());
        self.conn().execute(
            "UPDATE contacts SET tags = ?2, updated_at = datetime('now') WHERE id = ?1",
            rusqlite::params![id, tags_json],
        )?;
        Ok(())
    }

    /// Move a row out of the address-history derivation's ownership, so a later
    /// rebuild leaves it alone even with no cached source behind it. Curating a
    /// contact is choosing to keep it; tagging one the derivation invented is
    /// the case this guards. Writes only when the flag actually flips — the
    /// ownership change is not an edit to the contact's content.
    fn take_contact_ownership(&self, id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE contacts SET history_derived = 0
             WHERE id = ?1 AND history_derived = 1",
            rusqlite::params![id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Database {
        Database::open_memory().unwrap()
    }

    fn sample_contact() -> Contact {
        Contact {
            id: uuid::Uuid::new_v4().to_string(),
            account_id: "acc-1".to_string(),
            email: "alice@example.com".to_string(),
            name: Some("Alice".to_string()),
            tags: r#"["vendor"]"#.to_string(),
            notes: Some("Net-30 terms".to_string()),
            message_count: 5,
            first_seen: Some("2026-01-01T00:00:00".to_string()),
            last_seen: Some("2026-04-19T00:00:00".to_string()),
            created_at: "2026-04-19T00:00:00".to_string(),
            updated_at: "2026-04-19T00:00:00".to_string(),
        }
    }

    #[test]
    fn upsert_and_get_contact() {
        let db = test_db();
        let contact = sample_contact();
        db.upsert_contact(&contact).unwrap();

        let found = db.get_contact("acc-1", "alice@example.com").unwrap();
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.name, Some("Alice".to_string()));
        assert_eq!(found.message_count, 5);
    }

    #[test]
    fn list_contacts_with_tag_filter() {
        let db = test_db();
        db.upsert_contact(&sample_contact()).unwrap();
        db.upsert_contact(&Contact {
            id: uuid::Uuid::new_v4().to_string(),
            email: "bob@example.com".to_string(),
            tags: r#"["personal"]"#.to_string(),
            ..sample_contact()
        })
        .unwrap();

        assert_eq!(db.list_contacts("acc-1", None).unwrap().len(), 2);
        assert_eq!(db.list_contacts("acc-1", Some("vendor")).unwrap().len(), 1);
        assert_eq!(
            db.list_contacts("acc-1", Some("personal")).unwrap().len(),
            1
        );
    }

    #[test]
    fn contact_tag_operations() {
        let db = test_db();
        db.upsert_contact(&sample_contact()).unwrap();

        db.add_contact_tag("acc-1", "alice@example.com", "vip")
            .unwrap();
        let tags = db.get_contact_tags("acc-1", "alice@example.com").unwrap();
        assert!(tags.contains(&"vendor".to_string()));
        assert!(tags.contains(&"vip".to_string()));

        db.remove_contact_tag("acc-1", "alice@example.com", "vendor")
            .unwrap();
        let tags = db.get_contact_tags("acc-1", "alice@example.com").unwrap();
        assert!(!tags.contains(&"vendor".to_string()));
        assert!(tags.contains(&"vip".to_string()));
    }

    #[test]
    fn get_contact_tags_returns_empty_for_unknown() {
        let db = test_db();
        let tags = db.get_contact_tags("acc-1", "unknown@example.com").unwrap();
        assert!(tags.is_empty());
    }

    /// Insert a row the way `crate::address_book` does: lowercase, derived, and
    /// carrying the interaction signal history earned for it.
    fn derived_row(db: &Database, account_id: &str, email: &str, history_count: i64) {
        db.conn()
            .execute(
                "INSERT INTO contacts
                    (id, account_id, email, name, tags, notes, message_count,
                     history_count, history_sent_count, history_derived,
                     first_seen, last_seen, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, '[]', NULL, 0, ?5, 0, 1,
                         '2026-01-01T00:00:00', '2026-03-01T00:00:00',
                         '2026-01-01T00:00:00', '2026-01-01T00:00:00')",
                rusqlite::params![
                    uuid::Uuid::new_v4().to_string(),
                    account_id,
                    email,
                    "alice from a header",
                    history_count,
                ],
            )
            .unwrap();
    }

    fn row_count(db: &Database, account_id: &str) -> i64 {
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM contacts WHERE account_id = ?1",
                rusqlite::params![account_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn derived_columns(db: &Database, account_id: &str, email: &str) -> (i64, i64, i64) {
        db.conn()
            .query_row(
                "SELECT history_count, history_sent_count, history_derived FROM contacts
                 WHERE account_id = ?1 AND lower(email) = lower(?2)",
                rusqlite::params![account_id, email],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
    }

    /// Curating an address history already invented has to land on the derived
    /// row. `UNIQUE(account_id, email)` is case-sensitive, so relying on it
    /// inserted a second row for `Alice@Example.com`: the curated name and
    /// notes sat on a zero-signal twin while the derived row kept the count
    /// that decides ranking, and the derived row stayed sweepable by the next
    /// rebuild even though someone had chosen to keep the contact.
    #[test]
    fn manual_upsert_takes_over_a_derived_row_in_another_case() {
        let db = test_db();
        derived_row(&db, "acc-1", "alice@example.com", 7);

        db.upsert_contact(&Contact {
            email: "Alice@Example.com".to_string(),
            name: Some("Alice Winters".to_string()),
            tags: r#"["vendor"]"#.to_string(),
            notes: Some("Net-30 terms".to_string()),
            message_count: 0,
            ..sample_contact()
        })
        .unwrap();

        assert_eq!(row_count(&db, "acc-1"), 1, "no case-variant twin");

        // The curated metadata is what the contact now reads as, through the
        // spelling the operator typed and through the one already on file.
        for spelling in ["Alice@Example.com", "alice@example.com"] {
            let found = db.get_contact("acc-1", spelling).unwrap().unwrap();
            assert_eq!(found.name.as_deref(), Some("Alice Winters"), "{spelling}");
            assert_eq!(found.tags, r#"["vendor"]"#, "{spelling}");
            assert_eq!(found.notes.as_deref(), Some("Net-30 terms"), "{spelling}");
        }

        // The historical signal survives the takeover, and the row is now
        // manual so a rebuild that zeroes derived counts cannot sweep it.
        let (history_count, _, history_derived) =
            derived_columns(&db, "acc-1", "alice@example.com");
        assert_eq!(history_count, 7, "derived signal preserved");
        assert_eq!(history_derived, 0, "row is manually owned");
    }

    /// `envelope contacts add` builds its `Contact` with `first_seen: None,
    /// last_seen: None` — the CLI has no timestamps to offer. Assigning those
    /// straight onto the row erased the recency the derivation earned, and
    /// `suggest_addresses` breaks ties on `last_seen`: curating a contact by
    /// hand sank it below every address the history still had a date for. The
    /// erasure is also unrecoverable, because the derived dates come from
    /// message headers a later rebuild no longer re-reads for that row.
    #[test]
    fn curating_without_timestamps_keeps_the_derived_recency() {
        let db = test_db();
        derived_row(&db, "acc-1", "alice@example.com", 7);
        derived_row(&db, "acc-2", "alice@example.com", 3);

        db.upsert_contact(&Contact {
            email: "Alice@Example.com".to_string(),
            name: Some("Alice Winters".to_string()),
            notes: None,
            first_seen: None,
            last_seen: None,
            ..sample_contact()
        })
        .unwrap();

        let found = db
            .get_contact("acc-1", "alice@example.com")
            .unwrap()
            .unwrap();
        assert_eq!(
            found.last_seen.as_deref(),
            Some("2026-03-01T00:00:00"),
            "recency survives an upsert that supplies none"
        );
        assert_eq!(
            found.first_seen.as_deref(),
            Some("2026-01-01T00:00:00"),
            "first contact survives an upsert that supplies none"
        );
        assert_eq!(
            found.name.as_deref(),
            Some("Alice Winters"),
            "the curated name still applied"
        );

        let (history_count, _, history_derived) =
            derived_columns(&db, "acc-1", "alice@example.com");
        assert_eq!(history_count, 7, "derived signal preserved");
        assert_eq!(history_derived, 0, "row is manually owned");

        // Same address, other account: untouched, dates included.
        let other = db
            .get_contact("acc-2", "alice@example.com")
            .unwrap()
            .unwrap();
        assert_eq!(other.last_seen.as_deref(), Some("2026-03-01T00:00:00"));
        assert_eq!(other.name.as_deref(), Some("alice from a header"));
        let (_, _, other_derived) = derived_columns(&db, "acc-2", "alice@example.com");
        assert_eq!(other_derived, 1, "the other account's row stays derived");
    }

    /// A caller that DOES carry timestamps still gets to move recency forward —
    /// `contacts scan` reads them off the messages it walked.
    #[test]
    fn curating_with_timestamps_moves_recency_forward() {
        let db = test_db();
        derived_row(&db, "acc-1", "alice@example.com", 7);

        db.upsert_contact(&Contact {
            email: "alice@example.com".to_string(),
            last_seen: Some("2026-06-01T00:00:00".to_string()),
            ..sample_contact()
        })
        .unwrap();

        let found = db
            .get_contact("acc-1", "alice@example.com")
            .unwrap()
            .unwrap();
        assert_eq!(found.last_seen.as_deref(), Some("2026-06-01T00:00:00"));
    }

    /// The suggestion query dedupes case-insensitively, so a twin row was not
    /// visible as two dropdown entries — it was visible as the WRONG entry,
    /// the derived one, because it carried the higher signal. One row means
    /// one suggestion carrying the curated name.
    #[test]
    fn a_taken_over_row_suggests_once_with_the_curated_name() {
        let db = test_db();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acc-1', 'Work', 'me@example.com', 'example.com',
                         'smtp.example.com', 587, 'imap.example.com', 993, 'x')",
                [],
            )
            .unwrap();
        derived_row(&db, "acc-1", "alice@example.com", 7);

        db.upsert_contact(&Contact {
            email: "Alice@Example.com".to_string(),
            name: Some("Alice Winters".to_string()),
            message_count: 0,
            ..sample_contact()
        })
        .unwrap();

        let rows = db.suggest_addresses("acc-1", "alice", 8).unwrap();
        assert_eq!(rows.len(), 1, "one suggestion, not two case variants");
        assert_eq!(rows[0].email, "alice@example.com");
        assert_eq!(rows[0].name.as_deref(), Some("Alice Winters"));
    }

    /// The reconciliation is scoped to one account. The same address under two
    /// accounts is two contacts, and curating one must not reach the other.
    #[test]
    fn manual_upsert_never_reaches_another_accounts_row() {
        let db = test_db();
        derived_row(&db, "acc-2", "alice@example.com", 7);

        db.upsert_contact(&Contact {
            account_id: "acc-1".to_string(),
            email: "Alice@Example.com".to_string(),
            name: Some("Alice Winters".to_string()),
            ..sample_contact()
        })
        .unwrap();

        assert_eq!(row_count(&db, "acc-1"), 1);
        assert_eq!(row_count(&db, "acc-2"), 1);

        let other = db
            .get_contact("acc-2", "alice@example.com")
            .unwrap()
            .unwrap();
        assert_eq!(
            other.name.as_deref(),
            Some("alice from a header"),
            "the other account's row is untouched"
        );
        let (_, _, history_derived) = derived_columns(&db, "acc-2", "alice@example.com");
        assert_eq!(history_derived, 1, "the other account's row stays derived");
    }

    /// An upsert that supplies no name or notes must not blank what is on
    /// file, and re-curating an existing manual contact must not reset the
    /// count an import gave it beyond what the caller actually passes.
    #[test]
    fn re_upserting_preserves_metadata_the_caller_did_not_supply() {
        let db = test_db();
        db.upsert_contact(&Contact {
            message_count: 12,
            ..sample_contact()
        })
        .unwrap();

        db.upsert_contact(&Contact {
            email: "ALICE@example.com".to_string(),
            name: None,
            notes: None,
            tags: r#"["client"]"#.to_string(),
            message_count: 12,
            ..sample_contact()
        })
        .unwrap();

        assert_eq!(row_count(&db, "acc-1"), 1);
        let found = db
            .get_contact("acc-1", "alice@example.com")
            .unwrap()
            .unwrap();
        assert_eq!(found.name.as_deref(), Some("Alice"), "name kept");
        assert_eq!(found.notes.as_deref(), Some("Net-30 terms"), "notes kept");
        assert_eq!(found.tags, r#"["client"]"#, "tags are the caller's to set");
        assert_eq!(found.message_count, 12, "manual count intact");
    }

    /// Tagging reaches the contact whatever spelling it is asked for, and
    /// takes a derived row into manual ownership exactly once.
    #[test]
    fn tagging_matches_case_insensitively_and_claims_the_derived_row() {
        let db = test_db();
        derived_row(&db, "acc-1", "alice@example.com", 7);

        assert!(
            db.add_contact_tag("acc-1", "Alice@Example.com", "vip")
                .unwrap()
        );
        assert_eq!(row_count(&db, "acc-1"), 1);
        assert_eq!(
            db.get_contact_tags("acc-1", "ALICE@EXAMPLE.COM").unwrap(),
            vec!["vip".to_string()]
        );

        let (history_count, _, history_derived) =
            derived_columns(&db, "acc-1", "alice@example.com");
        assert_eq!(history_derived, 0, "tagging claims the row");
        assert_eq!(history_count, 7, "tagging does not touch the signal");
    }

    #[test]
    fn delete_contact_matches_case_insensitively() {
        let db = test_db();
        db.upsert_contact(&sample_contact()).unwrap();

        assert!(db.delete_contact("acc-1", "ALICE@Example.com").unwrap());
        assert_eq!(row_count(&db, "acc-1"), 0);
        assert!(!db.delete_contact("acc-1", "alice@example.com").unwrap());
    }
}
