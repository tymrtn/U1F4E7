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

    let mut allowed_emails = Vec::new();
    let mut allowed_domains = Vec::new();
    for entry in input.allow_recipients {
        let normalized = entry.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            continue;
        }
        if normalized.contains('@') {
            allowed_emails.push(normalized);
        } else {
            allowed_domains.push(normalized);
        }
    }

    if recipients.iter().all(|recipient| {
        allowed_emails.contains(&recipient.email) || allowed_domains.contains(&recipient.domain)
    }) {
        SendPolicyDecision::Allowed
    } else {
        SendPolicyDecision::Denied(SendPolicyDenial::recipient_not_allowlisted())
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
}
