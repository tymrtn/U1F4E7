// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SendMode {
    DraftOnly,
    ConfirmSend,
    AllowlistedSend,
    AutonomousSend,
}

impl SendMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DraftOnly => "draft-only",
            Self::ConfirmSend => "confirm-send",
            Self::AllowlistedSend => "allowlisted-send",
            Self::AutonomousSend => "autonomous-send",
        }
    }
}

impl fmt::Display for SendMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SendMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "draft-only" => Ok(Self::DraftOnly),
            "confirm-send" => Ok(Self::ConfirmSend),
            "allowlisted-send" => Ok(Self::AllowlistedSend),
            "autonomous-send" => Ok(Self::AutonomousSend),
            _ => Err(format!("unknown send mode: {value}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendRuntime {
    HumanCli,
    AgentMcp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendPolicyInput<'a> {
    pub to: &'a str,
    pub cc: Option<&'a str>,
    pub bcc: Option<&'a str>,
    pub confirm_send: bool,
    pub allow_recipients: &'a [String],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendPolicyDecision {
    Allowed,
    DraftOnly,
    Denied(SendPolicyDenial),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendPolicyDenial {
    pub code: String,
    pub reason: String,
}

impl SendPolicyDenial {
    pub fn confirmation_required() -> Self {
        Self {
            code: "send_confirmation_required".to_string(),
            reason: "send mode confirm-send requires explicit confirmation".to_string(),
        }
    }

    pub fn recipient_not_allowlisted() -> Self {
        Self {
            code: "send_recipient_not_allowlisted".to_string(),
            reason: "one or more recipients are not allowlisted".to_string(),
        }
    }

    pub fn recipient_parse_failed() -> Self {
        Self {
            code: "send_recipient_parse_failed".to_string(),
            reason: "could not parse one or more recipients".to_string(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "code": self.code,
            "reason": self.reason,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendPolicyAuditEvent {
    pub event: &'static str,
    pub payload: Value,
}

impl SendPolicyAuditEvent {
    /// Attach an agent identity to this audit event as **metadata only**.
    ///
    /// The `agent_id` is threaded into the audit payload for provenance so a
    /// human can later see which agent identity attempted a send. It is never an
    /// input to the send-policy decision (`evaluate` never sees it) and is
    /// omitted entirely when `None`, keeping today's payload byte-identical.
    pub fn with_agent_id(mut self, agent_id: Option<&str>) -> Self {
        if let Some(id) = agent_id
            && let Value::Object(map) = &mut self.payload
        {
            map.insert("agent_id".to_string(), Value::String(id.to_string()));
        }
        self
    }
}

pub fn default_mode_for_runtime(runtime: SendRuntime) -> SendMode {
    match runtime {
        SendRuntime::HumanCli => SendMode::AutonomousSend,
        SendRuntime::AgentMcp => SendMode::DraftOnly,
    }
}

pub fn evaluate(mode: SendMode, input: &SendPolicyInput<'_>) -> SendPolicyDecision {
    match mode {
        SendMode::DraftOnly => SendPolicyDecision::DraftOnly,
        SendMode::ConfirmSend => {
            if input.confirm_send {
                SendPolicyDecision::Allowed
            } else {
                SendPolicyDecision::Denied(SendPolicyDenial::confirmation_required())
            }
        }
        SendMode::AllowlistedSend => evaluate_allowlisted(input),
        SendMode::AutonomousSend => SendPolicyDecision::Allowed,
    }
}

pub fn audit_event_for(
    mode: SendMode,
    decision: &SendPolicyDecision,
    input: &SendPolicyInput<'_>,
) -> SendPolicyAuditEvent {
    let event = match decision {
        SendPolicyDecision::Allowed => "send_policy.allowed",
        SendPolicyDecision::DraftOnly => "send_policy.draft_only",
        SendPolicyDecision::Denied(_) => "send_policy.denied",
    };

    SendPolicyAuditEvent {
        event,
        payload: json!({
            "mode": mode,
            "recipient_count": parsed_recipient_count(input),
            "confirm_send": input.confirm_send,
            "allow_recipient_count": input.allow_recipients.len(),
            "denial_code": match decision {
                SendPolicyDecision::Denied(denial) => Some(denial.code.as_str()),
                _ => None,
            },
        }),
    }
}

fn evaluate_allowlisted(input: &SendPolicyInput<'_>) -> SendPolicyDecision {
    let recipients = match parse_all_recipients(input) {
        Ok(recipients) => recipients,
        Err(denial) => return SendPolicyDecision::Denied(denial),
    };

    let allowlist = RecipientAllowlist::from_patterns(input.allow_recipients);

    if recipients
        .iter()
        .all(|recipient| allowlist.matches(&recipient.email, &recipient.domain))
    {
        SendPolicyDecision::Allowed
    } else {
        SendPolicyDecision::Denied(SendPolicyDenial::recipient_not_allowlisted())
    }
}

/// Canonical allowlist matcher shared by `allowlisted-send` evaluation and any
/// other layer (e.g. agent policy) that must reuse the *same* recipient-matching
/// semantics rather than reimplement a second matcher.
///
/// Patterns are exact email addresses or bare/`@`-prefixed domains:
/// - `ops@corp.test` — exact email match (case-insensitive).
/// - `corp.test` or `@corp.test` — domain suffix match (any local part).
///
/// Empty entries are ignored. An empty pattern set matches nothing.
#[derive(Debug, Clone, Default)]
pub struct RecipientAllowlist {
    emails: Vec<String>,
    domains: Vec<String>,
}

impl RecipientAllowlist {
    /// Split allow patterns into exact-email and domain buckets (normalized to
    /// lowercase, `@` domain prefixes stripped).
    pub fn from_patterns(patterns: &[String]) -> Self {
        let mut emails = Vec::new();
        let mut domains = Vec::new();
        for entry in patterns {
            let normalized = entry.trim().to_ascii_lowercase();
            if normalized.is_empty() {
                continue;
            }
            if let Some(domain) = normalized.strip_prefix('@') {
                if !domain.is_empty() {
                    domains.push(domain.to_string());
                }
            } else if normalized.contains('@') {
                emails.push(normalized);
            } else {
                domains.push(normalized);
            }
        }
        Self { emails, domains }
    }

    /// True when a recipient (its lowercased email + domain) is permitted.
    pub fn matches(&self, email: &str, domain: &str) -> bool {
        self.emails.iter().any(|e| e == email) || self.domains.iter().any(|d| d == domain)
    }
}

fn parsed_recipient_count(input: &SendPolicyInput<'_>) -> usize {
    parse_all_recipients(input)
        .map(|items| items.len())
        .unwrap_or(0)
}

fn parse_all_recipients(
    input: &SendPolicyInput<'_>,
) -> Result<Vec<ParsedRecipient>, SendPolicyDenial> {
    let mut recipients = Vec::new();
    for header in [Some(input.to), input.cc, input.bcc].into_iter().flatten() {
        recipients.extend(parse_recipient_list(header)?);
    }
    Ok(recipients)
}

fn parse_recipient_list(value: &str) -> Result<Vec<ParsedRecipient>, SendPolicyDenial> {
    value
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(parse_recipient)
        .collect()
}

fn parse_recipient(token: &str) -> Result<ParsedRecipient, SendPolicyDenial> {
    let addr = if let Some(start) = token.rfind('<') {
        let end = token
            .rfind('>')
            .ok_or_else(SendPolicyDenial::recipient_parse_failed)?;
        token[start + 1..end].trim()
    } else {
        token.trim()
    };

    let normalized = addr.trim_matches('"').trim().to_ascii_lowercase();
    let (local, domain) = normalized
        .split_once('@')
        .ok_or_else(SendPolicyDenial::recipient_parse_failed)?;

    if normalized.matches('@').count() != 1
        || local.is_empty()
        || domain.is_empty()
        || local.contains(' ')
        || domain.contains(' ')
    {
        return Err(SendPolicyDenial::recipient_parse_failed());
    }

    let domain = domain.to_string();
    Ok(ParsedRecipient {
        email: normalized,
        domain,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedRecipient {
    email: String,
    domain: String,
}

/// Parse a single recipient token (`Name <a@b>` or bare `a@b`) into its
/// lowercased email and domain. Shared so other layers reuse the same parser
/// rather than writing a second address splitter. Returns `None` when the token
/// does not contain a single well-formed address.
pub fn parse_recipient_email_domain(token: &str) -> Option<(String, String)> {
    parse_recipient(token).ok().map(|r| (r.email, r.domain))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_modes_serialize_with_stable_names() {
        assert_eq!(
            serde_json::to_string(&SendMode::DraftOnly).unwrap(),
            "\"draft-only\""
        );
        assert_eq!(
            serde_json::to_string(&SendMode::ConfirmSend).unwrap(),
            "\"confirm-send\""
        );
        assert_eq!(
            serde_json::to_string(&SendMode::AllowlistedSend).unwrap(),
            "\"allowlisted-send\""
        );
        assert_eq!(
            serde_json::to_string(&SendMode::AutonomousSend).unwrap(),
            "\"autonomous-send\""
        );
    }

    #[test]
    fn defaults_are_explicit_for_agent_and_human_runtimes() {
        assert_eq!(
            default_mode_for_runtime(SendRuntime::AgentMcp),
            SendMode::DraftOnly
        );
        assert_eq!(
            default_mode_for_runtime(SendRuntime::HumanCli),
            SendMode::AutonomousSend
        );
    }

    #[test]
    fn confirm_send_denies_without_explicit_confirmation() {
        let input = SendPolicyInput {
            to: "Taylor <taylor@example.com>",
            cc: None,
            bcc: None,
            confirm_send: false,
            allow_recipients: &[],
        };

        let decision = evaluate(SendMode::ConfirmSend, &input);
        let denial = match decision {
            SendPolicyDecision::Denied(denial) => denial,
            other => panic!("expected denial, got {other:?}"),
        };

        assert_eq!(
            denial.to_json(),
            json!({
                "code": "send_confirmation_required",
                "reason": "send mode confirm-send requires explicit confirmation",
            })
        );
    }

    #[test]
    fn allowlisted_send_allows_exact_email_and_domain_matches_for_all_recipients() {
        let input = SendPolicyInput {
            to: "Taylor <taylor@example.com>, ops@corp.test",
            cc: Some("Casey <casey@allowed.test>"),
            bcc: Some("audit@example.com"),
            confirm_send: false,
            allow_recipients: &[
                "example.com".to_string(),
                "allowed.test".to_string(),
                "ops@corp.test".to_string(),
            ],
        };

        let decision = evaluate(SendMode::AllowlistedSend, &input);
        assert_eq!(decision, SendPolicyDecision::Allowed);
    }

    #[test]
    fn allowlisted_send_denies_when_any_recipient_is_outside_allowlist() {
        let input = SendPolicyInput {
            to: "Taylor <taylor@example.com>",
            cc: Some("Casey <casey@blocked.test>"),
            bcc: None,
            confirm_send: false,
            allow_recipients: &["example.com".to_string()],
        };

        let decision = evaluate(SendMode::AllowlistedSend, &input);
        let denial = match decision {
            SendPolicyDecision::Denied(denial) => denial,
            other => panic!("expected denial, got {other:?}"),
        };

        assert_eq!(
            denial.to_json(),
            json!({
                "code": "send_recipient_not_allowlisted",
                "reason": "one or more recipients are not allowlisted",
            })
        );
    }

    #[test]
    fn draft_only_short_circuits_to_draft_decision() {
        let input = SendPolicyInput {
            to: "taylor@example.com",
            cc: None,
            bcc: None,
            confirm_send: false,
            allow_recipients: &[],
        };

        assert_eq!(
            evaluate(SendMode::DraftOnly, &input),
            SendPolicyDecision::DraftOnly
        );
    }

    #[test]
    fn audit_payload_uses_stable_event_names_without_exposing_recipients() {
        let input = SendPolicyInput {
            to: "Taylor <taylor@example.com>",
            cc: Some("Casey <casey@example.com>"),
            bcc: None,
            confirm_send: true,
            allow_recipients: &["example.com".to_string()],
        };

        let event = audit_event_for(SendMode::ConfirmSend, &SendPolicyDecision::Allowed, &input);
        assert_eq!(event.event, "send_policy.allowed");
        let payload = event.payload.as_object().unwrap();
        assert!(!payload.values().any(|value| value == "taylor@example.com"));
        assert_eq!(payload.get("mode"), Some(&json!("confirm-send")));
        assert_eq!(payload.get("allow_recipient_count"), Some(&json!(1)));
    }

    #[test]
    fn audit_event_agent_id_defaults_none_and_only_serializes_when_some() {
        let input = SendPolicyInput {
            to: "Taylor <taylor@example.com>",
            cc: None,
            bcc: None,
            confirm_send: true,
            allow_recipients: &[],
        };

        // Default: agent_id is absent (byte-identical to today's payload).
        let base = audit_event_for(SendMode::ConfirmSend, &SendPolicyDecision::Allowed, &input);
        assert!(!base.payload.as_object().unwrap().contains_key("agent_id"));

        // with_agent_id(None) is a no-op.
        let none = base.clone().with_agent_id(None);
        assert_eq!(none.payload, base.payload);

        // with_agent_id(Some) adds metadata only, without leaking recipients.
        let tagged = base.clone().with_agent_id(Some("skippy-agent-42"));
        assert_eq!(
            tagged.payload.get("agent_id"),
            Some(&json!("skippy-agent-42"))
        );
        assert!(!tagged.payload.to_string().contains("taylor@example.com"));

        // Removing agent_id recovers the original payload exactly.
        let mut stripped = tagged.payload.as_object().unwrap().clone();
        stripped.remove("agent_id");
        assert_eq!(Value::Object(stripped), base.payload);
    }

    #[test]
    fn recipient_allowlist_reuses_matcher_for_exact_domain_and_at_prefix() {
        let allowlist = RecipientAllowlist::from_patterns(&[
            "ops@corp.test".to_string(),
            "@allowed.test".to_string(),
            "bare.test".to_string(),
        ]);
        assert!(allowlist.matches("ops@corp.test", "corp.test"));
        assert!(allowlist.matches("anyone@allowed.test", "allowed.test"));
        assert!(allowlist.matches("someone@bare.test", "bare.test"));
        assert!(!allowlist.matches("stranger@unknown.test", "unknown.test"));
        // Empty patterns match nothing (deny by default).
        assert!(!RecipientAllowlist::from_patterns(&[]).matches("a@b.test", "b.test"));
    }
}
