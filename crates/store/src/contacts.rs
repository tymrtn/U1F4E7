// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::db::Database;
use crate::errors::Result;
use crate::models::Contact;

impl Database {
    /// Add or update a contact.
    pub fn upsert_contact(&self, contact: &Contact) -> Result<()> {
        self.conn().execute(
            "INSERT INTO contacts (id, account_id, email, name, tags, notes, message_count, first_seen, last_seen, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(account_id, email) DO UPDATE SET
                name = COALESCE(excluded.name, contacts.name),
                tags = excluded.tags,
                notes = COALESCE(excluded.notes, contacts.notes),
                message_count = excluded.message_count,
                last_seen = excluded.last_seen,
                updated_at = datetime('now')",
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
        Ok(())
    }

    /// Get a contact by email for an account.
    pub fn get_contact(&self, account_id: &str, email: &str) -> Result<Option<Contact>> {
        use rusqlite::OptionalExtension;
        let contact = self
            .conn()
            .query_row(
                "SELECT id, account_id, email, name, tags, notes, message_count, first_seen, last_seen, created_at, updated_at
                 FROM contacts WHERE account_id = ?1 AND email = ?2",
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
    pub fn delete_contact(&self, account_id: &str, email: &str) -> Result<bool> {
        let deleted = self.conn().execute(
            "DELETE FROM contacts WHERE account_id = ?1 AND email = ?2",
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
                let tags_json = serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string());
                self.conn().execute(
                    "UPDATE contacts SET tags = ?3, updated_at = datetime('now') WHERE account_id = ?1 AND email = ?2",
                    rusqlite::params![account_id, email, tags_json],
                )?;
            }
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
                let tags_json = serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string());
                self.conn().execute(
                    "UPDATE contacts SET tags = ?3, updated_at = datetime('now') WHERE account_id = ?1 AND email = ?2",
                    rusqlite::params![account_id, email, tags_json],
                )?;
            }
            Ok(true)
        } else {
            Ok(false)
        }
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
}
