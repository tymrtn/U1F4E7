// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Shared dashboard state: the SQLite database plus a per-account IMAP
//! connection pool so we don't reconnect on every request.

use std::collections::HashMap;
use std::sync::Arc;

use envelope_email_store::models::AccountWithCredentials;
use envelope_email_store::{CredentialBackend, Database};
use tokio::sync::Mutex;

use envelope_email_transport::ImapClient;
use envelope_email_transport::imap;

/// Shared application state injected into every handler.
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Mutex<Database>>,
    pub imap_pool: Arc<Mutex<HashMap<String, Arc<Mutex<ImapClient>>>>>,
    pub backend: CredentialBackend,
}

impl AppState {
    pub fn new(db: Database, backend: CredentialBackend) -> Self {
        Self {
            db: Arc::new(Mutex::new(db)),
            imap_pool: Arc::new(Mutex::new(HashMap::new())),
            backend,
        }
    }

    /// Resolve credentials for an account and return an Arc-Mutex-wrapped IMAP
    /// client, reusing a pooled connection if one exists.
    ///
    /// If the pooled connection fails (e.g., the server dropped it), a fresh
    /// one is created. Callers must hold the returned `Arc<Mutex<ImapClient>>`
    /// only briefly — serializing access per account is acceptable for a
    /// localhost single-user dashboard but will bottleneck under concurrent
    /// requests to the same account.
    pub async fn get_or_create_imap(
        &self,
        account_id: &str,
    ) -> anyhow::Result<(Arc<Mutex<ImapClient>>, AccountWithCredentials)> {
        // Fetch credentials fresh every time (they may have changed).
        let creds = self.resolve_credentials(account_id).await?;

        let mut pool = self.imap_pool.lock().await;
        if let Some(existing) = pool.get(account_id).cloned() {
            return Ok((existing, creds));
        }

        let client = imap::connect(&creds)
            .await
            .map_err(|e| anyhow::anyhow!("IMAP connect failed for {account_id}: {e}"))?;
        let arc = Arc::new(Mutex::new(client));
        pool.insert(account_id.to_string(), arc.clone());
        Ok((arc, creds))
    }

    /// Evict a cached IMAP connection (call when you detect a stale one).
    pub async fn evict_imap(&self, account_id: &str) {
        let mut pool = self.imap_pool.lock().await;
        pool.remove(account_id);
    }

    async fn resolve_credentials(&self, account_id: &str) -> anyhow::Result<AccountWithCredentials> {
        let passphrase = envelope_email_store::credential_store::get_or_create_passphrase(
            self.backend,
        )
        .map_err(|e| anyhow::anyhow!("credential store error: {e}"))?;

        let db = self.db.lock().await;
        // Try ID, then email lookup
        if let Some(acct) = db
            .get_account(account_id)
            .map_err(|e| anyhow::anyhow!("db error: {e}"))?
        {
            return db
                .get_account_with_credentials(&acct.id, &passphrase)
                .map_err(|e| anyhow::anyhow!("decrypt credentials for {account_id}: {e}"));
        }
        if let Some(acct) = db
            .find_account_by_email(account_id)
            .map_err(|e| anyhow::anyhow!("db error: {e}"))?
        {
            return db
                .get_account_with_credentials(&acct.id, &passphrase)
                .map_err(|e| anyhow::anyhow!("decrypt credentials for {account_id}: {e}"));
        }
        anyhow::bail!("account not found: {account_id}")
    }
}
