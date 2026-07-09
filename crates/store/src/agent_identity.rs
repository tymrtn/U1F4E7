// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Agent-identity storage: per-agent tokens and policies for a shared inbox.
//!
//! Every agent gets a stable id, a human name, and one bearer token. Only a
//! SHA-256 hash of the token is persisted (`token_hash`) alongside a short
//! display prefix (`token_prefix`, e.g. `envtok_1a2b3c4d`). The raw token is
//! returned exactly once from [`Database::create_agent`] and never stored,
//! logged, or surfaced again.
//!
//! ## Hash scheme
//! The repo has no prior token-hashing scheme: account passwords are AES-GCM
//! *encrypted* (recoverable by design), and Argon2id is used only for
//! passphrase-derived encryption keys. A bearer token must never be
//! recoverable, so it is one-way hashed. Tokens carry 128 bits of OS-random
//! entropy (`envtok_<32 hex>`), which makes a fast cryptographic digest
//! sufficient — a slow KDF (Argon2) guards low-entropy human passwords, not
//! high-entropy random secrets. Lookups compare digests in constant time to
//! avoid leaking the stored hash through timing.

use crate::db::Database;
use crate::errors::{Result, StoreError};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// The four stable send-mode names, ordered by increasing autonomy. Used as a
/// per-agent ceiling: an agent may never be granted a mode above this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SendModeCeiling {
    DraftOnly,
    ConfirmSend,
    AllowlistedSend,
    AutonomousSend,
}

/// Default send-mode ceiling for a newly created agent: the safest mode.
pub const DEFAULT_SEND_MODE_CEILING: SendModeCeiling = SendModeCeiling::DraftOnly;

impl SendModeCeiling {
    /// Stable serialized name (matches the CLI/agent contract).
    pub fn as_str(self) -> &'static str {
        match self {
            SendModeCeiling::DraftOnly => "draft-only",
            SendModeCeiling::ConfirmSend => "confirm-send",
            SendModeCeiling::AllowlistedSend => "allowlisted-send",
            SendModeCeiling::AutonomousSend => "autonomous-send",
        }
    }

    /// Parse a stable serialized name.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "draft-only" => Ok(SendModeCeiling::DraftOnly),
            "confirm-send" => Ok(SendModeCeiling::ConfirmSend),
            "allowlisted-send" => Ok(SendModeCeiling::AllowlistedSend),
            "autonomous-send" => Ok(SendModeCeiling::AutonomousSend),
            other => Err(StoreError::Config(format!(
                "invalid send_mode_ceiling: {other}"
            ))),
        }
    }
}

/// A stored agent identity. Never carries the raw token or the token hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentIdentity {
    pub id: String,
    pub name: String,
    /// Short display prefix, e.g. `envtok_1a2b3c4d`. Not a secret.
    pub token_prefix: String,
    pub created_at: String,
    pub revoked_at: Option<String>,
    pub last_used_at: Option<String>,
}

/// Returned exactly once when an agent is created. The `token` is the only time
/// the raw bearer token is ever exposed — surface it to the operator and drop.
#[derive(Debug, Clone)]
pub struct NewAgentToken {
    pub identity: AgentIdentity,
    /// Raw bearer token (`envtok_<32 hex>`). Never persisted; never re-derivable.
    pub token: String,
}

/// Per-agent authorization policy. One row per agent (upsert semantics).
///
/// The wildcard fields carry either the literal `"*"` (allow all) or a JSON
/// array of entries; they are kept as opaque strings here so callers own the
/// interpretation. `allow_recipients` is `None` when unset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPolicy {
    pub agent_id: String,
    /// `"*"` or a JSON array of account ids.
    pub allowed_accounts: String,
    /// `"*"` or a JSON array of folder names.
    pub allowed_folders: String,
    /// `"*"` or a JSON array of action names.
    pub allowed_actions: String,
    pub send_mode_ceiling: SendModeCeiling,
    /// `None`, or a JSON array of email/domain patterns.
    pub allow_recipients: Option<String>,
}

impl AgentPolicy {
    /// A permissive-account/folder/action policy pinned to the safest send mode.
    /// Used as the implicit default when an agent has no explicit policy row.
    pub fn default_for(agent_id: &str) -> Self {
        AgentPolicy {
            agent_id: agent_id.to_string(),
            allowed_accounts: "*".to_string(),
            allowed_folders: "*".to_string(),
            allowed_actions: "*".to_string(),
            send_mode_ceiling: DEFAULT_SEND_MODE_CEILING,
            allow_recipients: None,
        }
    }
}

/// SHA-256 hex digest of the raw token. One-way; see module docs.
fn hash_token(raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Constant-time equality over equal-length hex digests. Avoids leaking the
/// stored hash via early-exit timing during token lookup.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Generate a fresh raw token `envtok_<32 lowercase hex>` (128 bits entropy).
fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut hex = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    format!("envtok_{hex}")
}

impl Database {
    /// Create a new agent, generating its bearer token. The raw token is
    /// returned once in [`NewAgentToken`]; only its hash + display prefix are
    /// stored. Fails if the name is already taken.
    pub fn create_agent(&self, name: &str) -> Result<NewAgentToken> {
        let id = Uuid::new_v4().to_string();
        let token = generate_token();
        let token_hash = hash_token(&token);
        // First 15 chars = "envtok_" (7) + 8 hex — the documented display form.
        let token_prefix: String = token.chars().take(15).collect();

        self.conn().execute(
            "INSERT INTO agent_identities (id, name, token_hash, token_prefix)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, name, token_hash, token_prefix],
        )?;

        let identity = self
            .get_agent_by_id(&id)?
            .ok_or_else(|| StoreError::Config("agent vanished after insert".into()))?;
        Ok(NewAgentToken { identity, token })
    }

    /// Look up an agent by a raw bearer token. Rejects unknown and revoked
    /// tokens. On success, stamps `last_used_at` and returns the identity.
    ///
    /// The raw token is hashed and compared in constant time; it is never
    /// logged or embedded in any error.
    pub fn get_agent_by_token(&self, raw_token: &str) -> Result<Option<AgentIdentity>> {
        let candidate_hash = hash_token(raw_token);
        let mut stmt = self.conn().prepare(
            "SELECT id, name, token_hash, token_prefix, created_at, revoked_at, last_used_at
             FROM agent_identities
             WHERE token_hash = ?1",
        )?;
        let row = stmt
            .query_row(params![candidate_hash], |row| {
                Ok((
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(5)?,
                    map_identity(row)?,
                ))
            })
            .optional()?;

        let (stored_hash, revoked_at, identity) = match row {
            Some(r) => r,
            None => return Ok(None),
        };
        // Constant-time confirm even though the WHERE already matched: keeps the
        // compare path uniform regardless of how the row was located.
        if !constant_time_eq(&candidate_hash, &stored_hash) {
            return Ok(None);
        }
        if revoked_at.is_some() {
            return Ok(None);
        }

        self.conn().execute(
            "UPDATE agent_identities SET last_used_at = datetime('now') WHERE id = ?1",
            params![identity.id],
        )?;
        // Return the identity with the freshly-stamped last_used_at.
        self.get_agent_by_id(&identity.id)
    }

    /// Fetch an agent by its human name.
    pub fn get_agent_by_name(&self, name: &str) -> Result<Option<AgentIdentity>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, name, token_hash, token_prefix, created_at, revoked_at, last_used_at
             FROM agent_identities
             WHERE name = ?1",
        )?;
        Ok(stmt.query_row(params![name], map_identity).optional()?)
    }

    /// Fetch an agent by id.
    pub fn get_agent_by_id(&self, id: &str) -> Result<Option<AgentIdentity>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, name, token_hash, token_prefix, created_at, revoked_at, last_used_at
             FROM agent_identities
             WHERE id = ?1",
        )?;
        Ok(stmt.query_row(params![id], map_identity).optional()?)
    }

    /// List all agents, newest first. Never exposes token hashes.
    pub fn list_agents(&self) -> Result<Vec<AgentIdentity>> {
        // rowid tiebreaks agents created within the same clock second so the
        // newest-inserted is deterministically first.
        let mut stmt = self.conn().prepare(
            "SELECT id, name, token_hash, token_prefix, created_at, revoked_at, last_used_at
             FROM agent_identities
             ORDER BY created_at DESC, rowid DESC",
        )?;
        let rows = stmt.query_map([], map_identity)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Revoke an agent by id. Idempotent: an already-revoked agent keeps its
    /// original `revoked_at`. Returns true if the agent exists.
    pub fn revoke_agent(&self, id: &str) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE agent_identities
             SET revoked_at = COALESCE(revoked_at, datetime('now'))
             WHERE id = ?1",
            params![id],
        )? > 0)
    }

    /// Count agents that have not been revoked. Feeds the license gate.
    pub fn count_active_agents(&self) -> Result<i64> {
        Ok(self.conn().query_row(
            "SELECT COUNT(*) FROM agent_identities WHERE revoked_at IS NULL",
            [],
            |row| row.get(0),
        )?)
    }

    /// Upsert an agent's policy (one row per agent).
    pub fn set_agent_policy(&self, policy: &AgentPolicy) -> Result<()> {
        self.conn().execute(
            "INSERT INTO agent_policies (
                agent_id, allowed_accounts, allowed_folders, allowed_actions,
                send_mode_ceiling, allow_recipients, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))
             ON CONFLICT(agent_id) DO UPDATE SET
                allowed_accounts = excluded.allowed_accounts,
                allowed_folders = excluded.allowed_folders,
                allowed_actions = excluded.allowed_actions,
                send_mode_ceiling = excluded.send_mode_ceiling,
                allow_recipients = excluded.allow_recipients,
                updated_at = datetime('now')",
            params![
                policy.agent_id,
                policy.allowed_accounts,
                policy.allowed_folders,
                policy.allowed_actions,
                policy.send_mode_ceiling.as_str(),
                policy.allow_recipients,
            ],
        )?;
        Ok(())
    }

    /// Fetch an agent's explicit policy row, if one exists.
    pub fn get_agent_policy(&self, agent_id: &str) -> Result<Option<AgentPolicy>> {
        let mut stmt = self.conn().prepare(
            "SELECT agent_id, allowed_accounts, allowed_folders, allowed_actions,
                    send_mode_ceiling, allow_recipients
             FROM agent_policies
             WHERE agent_id = ?1",
        )?;
        Ok(stmt.query_row(params![agent_id], map_policy).optional()?)
    }
}

fn map_identity(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentIdentity> {
    // Column 2 (token_hash) is intentionally not read into the public struct.
    Ok(AgentIdentity {
        id: row.get(0)?,
        name: row.get(1)?,
        token_prefix: row.get(3)?,
        created_at: row.get(4)?,
        revoked_at: row.get(5)?,
        last_used_at: row.get(6)?,
    })
}

fn map_policy(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentPolicy> {
    let ceiling_raw: String = row.get(4)?;
    // A ceiling value outside the four stable names means the DB was written by
    // a newer/corrupt writer; surface it as a typed decode failure.
    let send_mode_ceiling = SendModeCeiling::parse(&ceiling_raw).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            format!("invalid send_mode_ceiling: {ceiling_raw}").into(),
        )
    })?;
    Ok(AgentPolicy {
        agent_id: row.get(0)?,
        allowed_accounts: row.get(1)?,
        allowed_folders: row.get(2)?,
        allowed_actions: row.get(3)?,
        send_mode_ceiling,
        allow_recipients: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Database {
        Database::open_memory().unwrap()
    }

    #[test]
    fn token_roundtrip_create_lookup_revoke() {
        let db = test_db();
        let created = db.create_agent("skippy").unwrap();

        assert_eq!(created.identity.name, "skippy");
        assert!(created.token.starts_with("envtok_"));
        assert_eq!(created.token.len(), 7 + 32);
        assert_eq!(created.identity.token_prefix.len(), 15);
        assert!(created.token.starts_with(&created.identity.token_prefix));
        assert!(created.identity.revoked_at.is_none());
        assert!(created.identity.last_used_at.is_none());

        // Correct raw token resolves and stamps last_used_at.
        let looked_up = db.get_agent_by_token(&created.token).unwrap().unwrap();
        assert_eq!(looked_up.id, created.identity.id);
        assert!(looked_up.last_used_at.is_some());

        // Wrong token fails (fake fixture, never a real token).
        assert!(
            db.get_agent_by_token("envtok_deadbeefdeadbeefdeadbeefdeadbeef")
                .unwrap()
                .is_none()
        );

        // Revoked token is rejected.
        assert!(db.revoke_agent(&created.identity.id).unwrap());
        assert!(db.get_agent_by_token(&created.token).unwrap().is_none());
        let after = db.get_agent_by_id(&created.identity.id).unwrap().unwrap();
        assert!(after.revoked_at.is_some());
    }

    #[test]
    fn duplicate_name_is_rejected() {
        let db = test_db();
        db.create_agent("skippy").unwrap();
        assert!(db.create_agent("skippy").is_err());
    }

    #[test]
    fn get_by_name_and_list_hide_hashes() {
        let db = test_db();
        let a = db.create_agent("alpha").unwrap();
        let b = db.create_agent("bravo").unwrap();

        let by_name = db.get_agent_by_name("alpha").unwrap().unwrap();
        assert_eq!(by_name.id, a.identity.id);
        assert!(by_name.token_prefix.starts_with("envtok_"));

        let all = db.list_agents().unwrap();
        assert_eq!(all.len(), 2);
        // Newest first.
        assert_eq!(all[0].id, b.identity.id);
        // The public struct has no field capable of carrying the hash.
        assert!(all.iter().all(|ident| ident.token_prefix.len() == 15));
    }

    #[test]
    fn policy_upsert_and_typed_read() {
        let db = test_db();
        let agent = db.create_agent("skippy").unwrap();

        // No policy row yet.
        assert!(db.get_agent_policy(&agent.identity.id).unwrap().is_none());

        let mut policy = AgentPolicy::default_for(&agent.identity.id);
        policy.allowed_accounts = r#"["acc-1","acc-2"]"#.to_string();
        policy.send_mode_ceiling = SendModeCeiling::AllowlistedSend;
        policy.allow_recipients = Some(r#"["*@example.com"]"#.to_string());
        db.set_agent_policy(&policy).unwrap();

        let read = db.get_agent_policy(&agent.identity.id).unwrap().unwrap();
        assert_eq!(read.allowed_accounts, r#"["acc-1","acc-2"]"#);
        assert_eq!(read.allowed_folders, "*");
        assert_eq!(read.send_mode_ceiling, SendModeCeiling::AllowlistedSend);
        assert_eq!(
            read.allow_recipients.as_deref(),
            Some(r#"["*@example.com"]"#)
        );

        // Upsert (one row per agent): change the ceiling, re-read.
        policy.send_mode_ceiling = SendModeCeiling::DraftOnly;
        db.set_agent_policy(&policy).unwrap();
        let read = db.get_agent_policy(&agent.identity.id).unwrap().unwrap();
        assert_eq!(read.send_mode_ceiling, SendModeCeiling::DraftOnly);
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM agent_policies WHERE agent_id = ?1",
                params![agent.identity.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn count_active_excludes_revoked() {
        let db = test_db();
        let a = db.create_agent("alpha").unwrap();
        db.create_agent("bravo").unwrap();
        assert_eq!(db.count_active_agents().unwrap(), 2);

        db.revoke_agent(&a.identity.id).unwrap();
        assert_eq!(db.count_active_agents().unwrap(), 1);
    }

    #[test]
    fn hash_is_deterministic_and_token_never_recoverable() {
        // Same input → same digest; digest is not the token.
        assert_eq!(hash_token("envtok_test"), hash_token("envtok_test"));
        assert_ne!(hash_token("envtok_test"), "envtok_test");
        assert_eq!(hash_token("envtok_test").len(), 64);
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
    }
}
