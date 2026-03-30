// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::db::Database;
use crate::errors::Result;
use crate::models::StoredLicense;
use rusqlite::params;
use uuid::Uuid;

impl Database {
    /// Store (or replace) the active license. Deletes any existing licenses first.
    pub fn store_license(
        &self,
        token: &str,
        licensee: &str,
        expires_at: &str,
        features: &[String],
    ) -> Result<()> {
        let id = Uuid::new_v4().to_string();
        let features_json = serde_json::to_string(features).unwrap_or_else(|_| "[]".to_string());

        self.conn().execute("DELETE FROM license_keys", [])?;
        self.conn().execute(
            "INSERT INTO license_keys (id, token, licensee, expires_at, features)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, token, licensee, expires_at, features_json],
        )?;

        Ok(())
    }

    /// Return the most recent valid license, or None if no license exists.
    pub fn get_active_license(&self) -> Result<Option<StoredLicense>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, token, licensee, expires_at, features, activated_at
             FROM license_keys
             WHERE datetime(expires_at) >= datetime('now')
             ORDER BY activated_at DESC LIMIT 1",
        )?;

        let license = stmt
            .query_row([], |row| {
                let features_str: String = row.get(4)?;
                Ok(StoredLicense {
                    id: row.get(0)?,
                    token: row.get(1)?,
                    licensee: row.get(2)?,
                    expires_at: row.get(3)?,
                    features: serde_json::from_str(&features_str).unwrap_or_default(),
                    activated_at: row.get(5)?,
                })
            })
            .optional()?;

        Ok(license)
    }

    /// Remove all stored licenses.
    pub fn delete_license(&self) -> Result<()> {
        self.conn().execute("DELETE FROM license_keys", [])?;
        Ok(())
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
    use crate::db::Database;

    #[test]
    fn store_and_get_license() {
        let db = Database::open_memory().unwrap();

        db.store_license(
            "tok-abc-123",
            "Tyler Martin",
            "2099-12-31T23:59:59",
            &["api".to_string(), "agent".to_string()],
        )
        .unwrap();

        let license = db.get_active_license().unwrap().unwrap();
        assert_eq!(license.token, "tok-abc-123");
        assert_eq!(license.licensee, "Tyler Martin");
        assert_eq!(license.features, vec!["api", "agent"]);
    }

    #[test]
    fn expired_license_not_returned() {
        let db = Database::open_memory().unwrap();

        db.store_license("tok-expired", "Old User", "2020-01-01T00:00:00", &[])
            .unwrap();

        let license = db.get_active_license().unwrap();
        assert!(license.is_none());
    }

    #[test]
    fn delete_license() {
        let db = Database::open_memory().unwrap();

        db.store_license("tok-delete-me", "Someone", "2099-12-31T23:59:59", &[])
            .unwrap();

        db.delete_license().unwrap();
        let license = db.get_active_license().unwrap();
        assert!(license.is_none());
    }

    #[test]
    fn store_replaces_existing() {
        let db = Database::open_memory().unwrap();

        db.store_license("tok-first", "First", "2099-12-31T23:59:59", &[])
            .unwrap();
        db.store_license("tok-second", "Second", "2099-12-31T23:59:59", &[])
            .unwrap();

        let license = db.get_active_license().unwrap().unwrap();
        assert_eq!(license.token, "tok-second");
        assert_eq!(license.licensee, "Second");
    }
}
