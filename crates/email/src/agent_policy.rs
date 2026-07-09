// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Pure per-agent policy enforcement for Envelope v2 per-agent identities.
//!
//! This module is the **decision logic only** — no I/O, no store access, no
//! network. The v2 store owns the `agent_identities` / `agent_policies` tables;
//! the wiring layer (a later wave) maps a stored policy row into [`AgentPolicy`]
//! and calls the pure functions here. This crate does not depend on those store
//! rows so it can compile and be tested independently of the store migration.
//!
//! Three invariants are load-bearing:
//!
//! 1. **Deny by default.** An empty or missing allow-list denies. Only an
//!    explicit `*` wildcard grants allow-all. There is no implicit widening.
//! 2. **Send-mode ceilings never widen.** [`AgentPolicy::clamp_send_mode`]
//!    returns the *stricter* of the requested mode and the policy ceiling. A
//!    permissive ceiling can never make a request more permissive than it asked.
//! 3. **No secret material in denials.** [`PolicyDenial`] carries a stable
//!    machine code and a human reason — never a recipient address, account
//!    secret, or body content.

use serde_json::{Value, json};

use crate::send_policy::{RecipientAllowlist, SendMode, parse_recipient_email_domain};

/// Per-agent authorization policy. Field types mirror the v2 store JSON shape:
/// each allow-list is a `Vec<String>` where a single `"*"` entry means
/// allow-all and an empty list means deny-all.
///
/// This struct is defined here (not imported from the store) on purpose: the
/// store's `agent_policies` module may not compile yet while it is being built
/// concurrently. The wiring layer maps store rows into this shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPolicy {
    /// Account ids/emails the agent may act on. `["*"]` = any account.
    pub allowed_accounts: Vec<String>,
    /// Folders the agent may act on. `["*"]` = any folder.
    pub allowed_folders: Vec<String>,
    /// Action names the agent may perform. `["*"]` = any action.
    pub allowed_actions: Vec<String>,
    /// The most permissive send mode this agent may reach. A requested mode is
    /// clamped *down* to this ceiling and never widened past it.
    pub send_mode_ceiling: SendMode,
    /// Recipient allow patterns (exact email or `@domain`/bare-domain) usable by
    /// `allowlisted-send` flows. Reuses the send-policy allowlist matcher.
    pub allow_recipients: Vec<String>,
}

impl Default for AgentPolicy {
    /// The default policy is maximally restrictive: every allow-list is empty
    /// (deny-all) and the send ceiling is the strictest mode (`draft-only`). A
    /// wiring layer must explicitly widen from here; nothing is granted by
    /// omission.
    fn default() -> Self {
        Self {
            allowed_accounts: Vec::new(),
            allowed_folders: Vec::new(),
            allowed_actions: Vec::new(),
            send_mode_ceiling: SendMode::DraftOnly,
            allow_recipients: Vec::new(),
        }
    }
}

/// A stable, secret-free denial from [`AgentPolicy::authorize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDenial {
    /// Stable machine code, e.g. `agent_policy_denied_action`.
    pub code: &'static str,
    /// Human-readable reason. Contains no recipient address, account secret, or
    /// body content — only the field name and (redacted) subject of the check.
    pub reason: String,
}

impl PolicyDenial {
    fn denied_action(action: &str) -> Self {
        Self {
            code: "agent_policy_denied_action",
            reason: format!("agent policy does not permit action '{action}'"),
        }
    }

    fn denied_account() -> Self {
        Self {
            code: "agent_policy_denied_account",
            reason: "agent policy does not permit this account".to_string(),
        }
    }

    fn denied_folder() -> Self {
        Self {
            code: "agent_policy_denied_folder",
            reason: "agent policy does not permit this folder".to_string(),
        }
    }

    /// Sanitized `{code, reason}` JSON for denial payloads and audit rows.
    pub fn to_json(&self) -> Value {
        json!({
            "code": self.code,
            "reason": self.reason,
        })
    }
}

impl AgentPolicy {
    /// Authorize an `(action, account, folder)` tuple against this policy.
    ///
    /// Deny-by-default: an empty allow-list denies; a single `"*"` entry allows
    /// all. `folder` is only checked when the caller supplies one (some actions
    /// are folder-agnostic); when `Some`, it must pass the folder allow-list.
    ///
    /// Checks run action → account → folder so the returned denial names the
    /// first failing dimension deterministically.
    pub fn authorize(
        &self,
        action: &str,
        account: &str,
        folder: Option<&str>,
    ) -> Result<(), PolicyDenial> {
        if !list_allows(&self.allowed_actions, action) {
            return Err(PolicyDenial::denied_action(action));
        }
        if !list_allows(&self.allowed_accounts, account) {
            return Err(PolicyDenial::denied_account());
        }
        if let Some(folder) = folder
            && !list_allows(&self.allowed_folders, folder)
        {
            return Err(PolicyDenial::denied_folder());
        }
        Ok(())
    }

    /// Clamp a requested send mode down to this policy's ceiling.
    ///
    /// Returns the **stricter** of `requested` and `send_mode_ceiling` under the
    /// strictness order:
    /// `draft-only > confirm-send > allowlisted-send > autonomous-send`.
    ///
    /// This never widens: requesting `draft-only` under an `autonomous-send`
    /// ceiling stays `draft-only`.
    pub fn clamp_send_mode(&self, requested: SendMode) -> SendMode {
        stricter(requested, self.send_mode_ceiling)
    }

    /// True when `email` (its lowercased address + domain) is permitted by this
    /// policy's `allow_recipients` patterns. Reuses the canonical send-policy
    /// [`RecipientAllowlist`] matcher rather than a second matcher.
    ///
    /// Deny-by-default: an empty pattern set matches nothing. Tokens that do not
    /// parse to a single well-formed address never match.
    pub fn recipient_allowed(&self, email: &str) -> bool {
        match parse_recipient_email_domain(email) {
            Some((email, domain)) => {
                RecipientAllowlist::from_patterns(&self.allow_recipients).matches(&email, &domain)
            }
            None => false,
        }
    }
}

/// Deny-by-default membership: empty list denies, `"*"` allows all, otherwise
/// exact (case-sensitive) membership. Ids/folders/actions are compared verbatim
/// so this makes no normalization assumptions the store does not.
fn list_allows(list: &[String], value: &str) -> bool {
    if list.iter().any(|entry| entry == "*") {
        return true;
    }
    list.iter().any(|entry| entry == value)
}

/// Explicit strictness rank: **lower = stricter**. The `min` of two ranks is the
/// stricter mode, which is what clamping returns.
fn strictness_rank(mode: SendMode) -> u8 {
    match mode {
        SendMode::DraftOnly => 0,
        SendMode::ConfirmSend => 1,
        SendMode::AllowlistedSend => 2,
        SendMode::AutonomousSend => 3,
    }
}

/// Return whichever of `a`/`b` is stricter (lower rank). Ties return `a`.
fn stricter(a: SendMode, b: SendMode) -> SendMode {
    if strictness_rank(a) <= strictness_rank(b) {
        a
    } else {
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_MODES: [SendMode; 4] = [
        SendMode::DraftOnly,
        SendMode::ConfirmSend,
        SendMode::AllowlistedSend,
        SendMode::AutonomousSend,
    ];

    fn policy_with_ceiling(ceiling: SendMode) -> AgentPolicy {
        AgentPolicy {
            allowed_accounts: vec!["*".to_string()],
            allowed_folders: vec!["*".to_string()],
            allowed_actions: vec!["*".to_string()],
            send_mode_ceiling: ceiling,
            allow_recipients: Vec::new(),
        }
    }

    #[test]
    fn deny_by_default_when_lists_empty() {
        let policy = AgentPolicy {
            send_mode_ceiling: SendMode::DraftOnly,
            ..Default::default()
        };
        let denial = policy
            .authorize("draft.create", "me@example.com", None)
            .unwrap_err();
        assert_eq!(denial.code, "agent_policy_denied_action");
    }

    #[test]
    fn wildcard_allows_all_dimensions() {
        let policy = policy_with_ceiling(SendMode::AutonomousSend);
        assert!(
            policy
                .authorize("anything", "any@account.test", Some("AnyFolder"))
                .is_ok()
        );
    }

    #[test]
    fn per_field_denial_codes_are_stable() {
        // Action denied first.
        let action_only = AgentPolicy {
            allowed_accounts: vec!["*".to_string()],
            allowed_folders: vec!["*".to_string()],
            allowed_actions: vec!["draft.create".to_string()],
            ..Default::default()
        };
        assert_eq!(
            action_only
                .authorize("draft.send", "a@b.test", Some("Inbox"))
                .unwrap_err()
                .code,
            "agent_policy_denied_action"
        );

        // Action allowed, account denied.
        let account_denied = AgentPolicy {
            allowed_accounts: vec!["ok@b.test".to_string()],
            allowed_folders: vec!["*".to_string()],
            allowed_actions: vec!["*".to_string()],
            ..Default::default()
        };
        assert_eq!(
            account_denied
                .authorize("draft.send", "nope@b.test", Some("Inbox"))
                .unwrap_err()
                .code,
            "agent_policy_denied_account"
        );

        // Action + account allowed, folder denied.
        let folder_denied = AgentPolicy {
            allowed_accounts: vec!["*".to_string()],
            allowed_folders: vec!["Inbox".to_string()],
            allowed_actions: vec!["*".to_string()],
            ..Default::default()
        };
        assert_eq!(
            folder_denied
                .authorize("draft.send", "a@b.test", Some("Archive"))
                .unwrap_err()
                .code,
            "agent_policy_denied_folder"
        );
    }

    #[test]
    fn folder_check_skipped_when_none() {
        let policy = AgentPolicy {
            allowed_accounts: vec!["*".to_string()],
            allowed_folders: Vec::new(), // would deny any folder
            allowed_actions: vec!["*".to_string()],
            ..Default::default()
        };
        // Folder-agnostic action: None means the (empty) folder list is not consulted.
        assert!(policy.authorize("inbox.list", "a@b.test", None).is_ok());
        // But supplying a folder against an empty list denies.
        assert_eq!(
            policy
                .authorize("inbox.list", "a@b.test", Some("Inbox"))
                .unwrap_err()
                .code,
            "agent_policy_denied_folder"
        );
    }

    #[test]
    fn denial_json_carries_no_secret_material() {
        let policy = AgentPolicy {
            allowed_accounts: vec!["ok@corp.test".to_string()],
            allowed_folders: vec!["*".to_string()],
            allowed_actions: vec!["*".to_string()],
            ..Default::default()
        };
        let denial = policy
            .authorize("draft.send", "secret@private.test", Some("Inbox"))
            .unwrap_err();
        let payload = denial.to_json();
        // The denied account address must not leak into the reason/JSON.
        assert!(!payload.to_string().contains("secret@private.test"));
        assert_eq!(
            payload.get("code"),
            Some(&json!("agent_policy_denied_account"))
        );
    }

    #[test]
    fn clamp_never_widens_draft_only_under_autonomous_ceiling() {
        let policy = policy_with_ceiling(SendMode::AutonomousSend);
        assert_eq!(
            policy.clamp_send_mode(SendMode::DraftOnly),
            SendMode::DraftOnly
        );
    }

    #[test]
    fn clamp_matrix_returns_stricter_of_requested_and_ceiling() {
        // Full 4x4 matrix: result is always the stricter (lower-rank) mode.
        for &ceiling in &ALL_MODES {
            let policy = policy_with_ceiling(ceiling);
            for &requested in &ALL_MODES {
                let got = policy.clamp_send_mode(requested);
                let expected = if strictness_rank(requested) <= strictness_rank(ceiling) {
                    requested
                } else {
                    ceiling
                };
                assert_eq!(got, expected, "clamp({requested}, ceiling={ceiling})");
                // Result must never be weaker than either input.
                assert!(strictness_rank(got) <= strictness_rank(requested));
                assert!(strictness_rank(got) <= strictness_rank(ceiling));
            }
        }
    }

    #[test]
    fn clamp_is_antisymmetric_under_strictness_order() {
        // For any pair, clamping is symmetric in its two inputs (min is
        // commutative) and the result is the unique stricter element unless the
        // ranks are equal. This proves the order is a genuine total order with
        // no accidental widening.
        for &a in &ALL_MODES {
            for &b in &ALL_MODES {
                let ab = stricter(a, b);
                let ba = stricter(b, a);
                assert_eq!(
                    strictness_rank(ab),
                    strictness_rank(ba),
                    "strictness must be order-independent for {a} vs {b}"
                );
                if strictness_rank(a) < strictness_rank(b) {
                    // Anti-symmetry: strictly stricter a is chosen over b, and
                    // never the reverse.
                    assert_eq!(ab, a);
                    assert_ne!(ab, b);
                }
            }
        }
    }

    #[test]
    fn recipient_matcher_exact_domain_and_miss() {
        let policy = AgentPolicy {
            allow_recipients: vec![
                "ops@corp.test".to_string(),
                "@allowed.test".to_string(),
                "bare.test".to_string(),
            ],
            ..Default::default()
        };
        // Exact email match (case-insensitive).
        assert!(policy.recipient_allowed("OPS@corp.test"));
        // `@domain` suffix match on any local part.
        assert!(policy.recipient_allowed("anyone@allowed.test"));
        // Bare-domain form also matches.
        assert!(policy.recipient_allowed("someone@bare.test"));
        // Miss: neither email nor domain listed.
        assert!(!policy.recipient_allowed("stranger@unknown.test"));
        // Unparseable token never matches.
        assert!(!policy.recipient_allowed("not-an-address"));
    }

    #[test]
    fn recipient_matcher_denies_when_empty() {
        let policy = AgentPolicy::default();
        assert!(!policy.recipient_allowed("anyone@anywhere.test"));
    }
}
