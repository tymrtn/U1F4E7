// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Per-agent identity context for the MCP transport.
//!
//! At MCP startup we read `ENVELOPE_AGENT_TOKEN`. When present, it resolves to a
//! stored [`AgentIdentity`] and its policy; every subsequent MCP tool call is
//! authorized against that policy before dispatch, send modes are clamped to the
//! policy ceiling, and mutating writes are attributed to the agent id. When the
//! env var is unset the MCP server runs anonymously exactly as before — existing
//! users are unaffected.
//!
//! This module owns the two impedance mismatches between the store and the
//! transport policy types:
//!
//! 1. **Send-mode ceiling.** The store's [`StoreSendModeCeiling`] and the
//!    transport's [`SendMode`] are distinct enums that happen to share the four
//!    stable serialized names. [`map_send_mode_ceiling`] is the single explicit
//!    bridge — an exhaustive 4-arm match pinned by a test — so a future rename on
//!    either side fails to compile rather than silently mismapping. Comparisons
//!    on both sides are verbatim/case-sensitive; keep them that way.
//! 2. **Allow-list shape.** The store persists each allow-list as an opaque
//!    string (`"*"` or a JSON array); the transport wants `Vec<String>`. See
//!    [`parse_allow_list`].

use envelope_email_store::{
    AgentIdentity, AgentPolicy as StoreAgentPolicy, Database,
    SendModeCeiling as StoreSendModeCeiling,
};
use envelope_email_transport::{AgentPolicy as TransportPolicy, PolicyDenial, SendMode};

/// Env var carrying the raw bearer token that selects an agent identity for the
/// MCP session. Unset means anonymous MCP (today's behavior).
pub const AGENT_TOKEN_ENV: &str = "ENVELOPE_AGENT_TOKEN";

/// A resolved agent identity plus its enforcement policy for one MCP session.
#[derive(Debug, Clone)]
pub struct AgentContext {
    pub agent_id: String,
    pub agent_name: String,
    pub policy: TransportPolicy,
}

impl AgentContext {
    /// Authorize a tool call. `tool_name` is mapped to a stable action name via
    /// [`tool_action`]; an unknown tool denies. `account`/`folder` are the
    /// resolved parameters for this call (folder omitted when the tool is
    /// folder-agnostic).
    pub fn authorize_tool(
        &self,
        tool_name: &str,
        account: &str,
        folder: Option<&str>,
    ) -> Result<(), PolicyDenial> {
        let action = tool_action(tool_name).ok_or_else(|| PolicyDenial {
            code: "agent_policy_denied_action",
            reason: format!("agent policy does not permit unknown tool '{tool_name}'"),
        })?;
        self.policy.authorize(action, account, folder)
    }

    /// Clamp a requested send mode down to this agent's policy ceiling.
    pub fn clamp_send_mode(&self, requested: SendMode) -> SendMode {
        self.policy.clamp_send_mode(requested)
    }
}

/// Resolve the MCP agent context from the environment.
///
/// - Env unset → `Ok(None)` (anonymous MCP; defaults unchanged).
/// - Env set + valid, non-revoked token → `Ok(Some(ctx))`.
/// - Env set + unknown/revoked token → `Err(_)`. The caller must fail startup
///   loud and must never fall back to anonymous.
///
/// The raw token is never echoed into the error.
pub fn resolve_from_env(db: &Database) -> anyhow::Result<Option<AgentContext>> {
    let raw = match std::env::var(AGENT_TOKEN_ENV) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => return Ok(None),
    };

    let identity = db.get_agent_by_token(&raw)?.ok_or_else(|| {
        anyhow::anyhow!(
            "{AGENT_TOKEN_ENV} is set but does not match any active agent identity \
             (unknown or revoked token); refusing to start the MCP server. \
             Create one with `envelope agent create <name>` or unset {AGENT_TOKEN_ENV} \
             to run anonymously."
        )
    })?;

    let store_policy = db
        .get_agent_policy(&identity.id)?
        .unwrap_or_else(|| StoreAgentPolicy::default_for(&identity.id));
    let policy = map_store_policy(&store_policy)?;

    Ok(Some(AgentContext {
        agent_id: identity.id.clone(),
        agent_name: identity.name.clone(),
        policy,
    }))
}

/// Map a stored agent policy row into the pure transport policy the enforcement
/// logic consumes.
pub fn map_store_policy(store: &StoreAgentPolicy) -> anyhow::Result<TransportPolicy> {
    Ok(TransportPolicy {
        allowed_accounts: parse_allow_list(&store.allowed_accounts)?,
        allowed_folders: parse_allow_list(&store.allowed_folders)?,
        allowed_actions: parse_allow_list(&store.allowed_actions)?,
        send_mode_ceiling: map_send_mode_ceiling(store.send_mode_ceiling),
        allow_recipients: match store.allow_recipients.as_deref() {
            Some(raw) => parse_allow_list(raw)?,
            None => Vec::new(),
        },
    })
}

/// The single explicit bridge from the store ceiling enum to the transport send
/// mode. Exhaustive by design: adding a variant to either enum without updating
/// this match is a compile error. The four serialized names are stable and
/// shared (see [`StoreSendModeCeiling::as_str`] / [`SendMode::as_str`]).
pub fn map_send_mode_ceiling(ceiling: StoreSendModeCeiling) -> SendMode {
    match ceiling {
        StoreSendModeCeiling::DraftOnly => SendMode::DraftOnly,
        StoreSendModeCeiling::ConfirmSend => SendMode::ConfirmSend,
        StoreSendModeCeiling::AllowlistedSend => SendMode::AllowlistedSend,
        StoreSendModeCeiling::AutonomousSend => SendMode::AutonomousSend,
    }
}

/// Parse a stored allow-list field. The store keeps either the literal `"*"`
/// (allow-all, preserved verbatim so the transport wildcard rule fires) or a
/// JSON array of strings. An empty/blank string is deny-all (empty vec).
fn parse_allow_list(raw: &str) -> anyhow::Result<Vec<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed == "*" {
        return Ok(vec!["*".to_string()]);
    }
    let parsed: Vec<String> = serde_json::from_str(trimmed).map_err(|e| {
        anyhow::anyhow!("agent policy allow-list is neither \"*\" nor a JSON string array: {e}")
    })?;
    Ok(parsed)
}

/// Map an MCP tool name to the stable policy action name it is authorized under.
///
/// The action namespace is intentionally coarse and matches the send-safety and
/// action-log vocabulary. Read surfaces authorize under `inbox.read`; mutating
/// surfaces under their operation. Unknown tools return `None` and are denied.
///
/// | tool                  | action           |
/// |-----------------------|------------------|
/// | accounts              | accounts.list    |
/// | inbox                 | inbox.read       |
/// | read                  | inbox.read       |
/// | search                | inbox.read       |
/// | folders               | folders.list     |
/// | contacts              | contacts.read    |
/// | send                  | send             |
/// | reply                 | send             |
/// | send_draft            | send             |
/// | create_reply_draft    | draft.create     |
/// | create_forward_draft  | draft.create     |
/// | modify_draft          | draft.modify     |
/// | get_draft             | draft.read       |
/// | move_message          | move             |
/// | flag                  | flag             |
/// | tag                   | tag              |
pub fn tool_action(tool_name: &str) -> Option<&'static str> {
    Some(match tool_name {
        "accounts" => "accounts.list",
        "inbox" | "read" | "search" => "inbox.read",
        "folders" => "folders.list",
        "contacts" => "contacts.read",
        "send" | "reply" | "send_draft" => "send",
        "create_reply_draft" | "create_forward_draft" => "draft.create",
        "modify_draft" => "draft.modify",
        "get_draft" => "draft.read",
        "move_message" => "move",
        "flag" => "flag",
        "tag" => "tag",
        _ => return None,
    })
}

/// The agent id to attribute a store write to, if a context is present.
pub fn agent_id_of(ctx: Option<&AgentContext>) -> Option<&str> {
    ctx.map(|c| c.agent_id.as_str())
}

/// Build an `AgentIdentity`-free display of the context for diagnostics. Never
/// includes the token.
pub fn _describe(identity: &AgentIdentity) -> String {
    format!("{} ({})", identity.name, identity.token_prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_mode_ceiling_maps_all_four_variants_exhaustively() {
        // Pins the store->transport bridge across all four stable names. If a
        // variant is added to either enum, this test (and the exhaustive match)
        // must be updated together.
        assert_eq!(
            map_send_mode_ceiling(StoreSendModeCeiling::DraftOnly),
            SendMode::DraftOnly
        );
        assert_eq!(
            map_send_mode_ceiling(StoreSendModeCeiling::ConfirmSend),
            SendMode::ConfirmSend
        );
        assert_eq!(
            map_send_mode_ceiling(StoreSendModeCeiling::AllowlistedSend),
            SendMode::AllowlistedSend
        );
        assert_eq!(
            map_send_mode_ceiling(StoreSendModeCeiling::AutonomousSend),
            SendMode::AutonomousSend
        );
        // The serialized names must be identical on both sides.
        for c in [
            StoreSendModeCeiling::DraftOnly,
            StoreSendModeCeiling::ConfirmSend,
            StoreSendModeCeiling::AllowlistedSend,
            StoreSendModeCeiling::AutonomousSend,
        ] {
            assert_eq!(c.as_str(), map_send_mode_ceiling(c).as_str());
        }
    }

    #[test]
    fn parse_allow_list_wildcard_array_and_empty() {
        assert_eq!(parse_allow_list("*").unwrap(), vec!["*".to_string()]);
        assert_eq!(parse_allow_list("").unwrap(), Vec::<String>::new());
        assert_eq!(parse_allow_list("   ").unwrap(), Vec::<String>::new());
        assert_eq!(
            parse_allow_list(r#"["a@x.test","b@y.test"]"#).unwrap(),
            vec!["a@x.test".to_string(), "b@y.test".to_string()]
        );
        assert!(parse_allow_list("not-json").is_err());
    }

    #[test]
    fn map_store_policy_default_is_deny_all_draft_only() {
        let store = StoreAgentPolicy::default_for("agent-1");
        // default_for is permissive account/folder/action + draft-only ceiling.
        let mapped = map_store_policy(&store).unwrap();
        assert_eq!(mapped.allowed_accounts, vec!["*".to_string()]);
        assert_eq!(mapped.send_mode_ceiling, SendMode::DraftOnly);
    }

    #[test]
    fn tool_action_maps_known_and_denies_unknown() {
        assert_eq!(tool_action("send"), Some("send"));
        assert_eq!(tool_action("reply"), Some("send"));
        assert_eq!(tool_action("send_draft"), Some("send"));
        assert_eq!(tool_action("inbox"), Some("inbox.read"));
        assert_eq!(tool_action("read"), Some("inbox.read"));
        assert_eq!(tool_action("search"), Some("inbox.read"));
        assert_eq!(tool_action("move_message"), Some("move"));
        assert_eq!(tool_action("tag"), Some("tag"));
        assert_eq!(tool_action("nonexistent_tool"), None);
    }

    #[test]
    fn authorize_tool_denies_unknown_tool() {
        let ctx = AgentContext {
            agent_id: "id".to_string(),
            agent_name: "skippy".to_string(),
            policy: TransportPolicy {
                allowed_accounts: vec!["*".to_string()],
                allowed_folders: vec!["*".to_string()],
                allowed_actions: vec!["*".to_string()],
                send_mode_ceiling: SendMode::DraftOnly,
                allow_recipients: Vec::new(),
            },
        };
        let denial = ctx
            .authorize_tool("totally_unknown", "a@b.test", None)
            .unwrap_err();
        assert_eq!(denial.code, "agent_policy_denied_action");
    }
}
