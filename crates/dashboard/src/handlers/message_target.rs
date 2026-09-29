// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Exact targeting for single-message mutations (flag, read state, move,
//! snooze).
//!
//! A webmail row names a message as `(account, folder, UID)` plus, when the
//! list knew them, the mailbox UIDVALIDITY and the Message-ID. Between paint and
//! click the message can move, the mailbox can be reset, or another client can
//! expunge it. IMAP answers `UID STORE`/`UID COPY` on a missing UID with a
//! silent OK, so without a probe a stale click would be reported as a success.
//! Every mutation therefore probes first and refuses with a stable 409 when the
//! handle no longer names the message the operator saw.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use envelope_email_transport::imap::{self, UidProbe};
use serde_json::json;

use crate::state::AppState;

/// What the client believed about the target when it rendered the row.
#[derive(Debug, Default, Clone)]
pub struct TargetExpectation {
    pub uidvalidity: Option<u32>,
    pub message_id: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TargetMismatch {
    /// The UID is not in the folder any more.
    Gone,
    /// The mailbox was reset: the same UID number may name another message.
    UidValidityChanged { expected: u32, actual: Option<u32> },
    /// The UID now holds a different message.
    MessageIdChanged,
}

/// Pure comparison of the client's expectation against a fresh probe.
pub fn check_target(expect: &TargetExpectation, probe: &UidProbe) -> Result<(), TargetMismatch> {
    if let Some(expected) = expect.uidvalidity
        && probe.uid_validity != Some(expected)
    {
        return Err(TargetMismatch::UidValidityChanged {
            expected,
            actual: probe.uid_validity,
        });
    }
    if !probe.exists() {
        return Err(TargetMismatch::Gone);
    }
    if let Some(wanted) = expect
        .message_id
        .as_deref()
        .and_then(imap::normalize_message_id)
    {
        let actual = probe
            .message_id
            .as_deref()
            .and_then(imap::normalize_message_id);
        if actual.as_deref() != Some(wanted.as_str()) {
            return Err(TargetMismatch::MessageIdChanged);
        }
    }
    Ok(())
}

/// Stable 409 body for a mismatch. Carries no addresses or message content.
pub fn mismatch_body(mismatch: &TargetMismatch) -> serde_json::Value {
    match mismatch {
        TargetMismatch::Gone => json!({
            "code": "message_not_found",
            "reason": "this message is no longer in that folder; it was moved or deleted",
        }),
        TargetMismatch::UidValidityChanged { .. } => json!({
            "code": "stale_uid",
            "reason": "the mailbox was reset since this list loaded; refresh before acting",
        }),
        TargetMismatch::MessageIdChanged => json!({
            "code": "stale_uid",
            "reason": "that message slot now holds a different message; refresh before acting",
        }),
    }
}

/// Probe `(folder, uid)` and refuse unless it still names the expected
/// message. Returns the probe (flags, Message-ID) on success so callers can use
/// the server's own values instead of the client's.
pub async fn verify_target(
    state: &AppState,
    client: &mut envelope_email_transport::ImapClient,
    account_id: &str,
    folder: &str,
    uid: u32,
    expect: &TargetExpectation,
) -> Result<UidProbe, Response> {
    let probe = match imap::probe_uid(client, folder, uid).await {
        Ok(p) => p,
        Err(e) => {
            state.evict_imap(account_id).await;
            return Err((StatusCode::BAD_GATEWAY, format!("probe: {e}")).into_response());
        }
    };
    match check_target(expect, &probe) {
        Ok(()) => Ok(probe),
        Err(m) => Err((StatusCode::CONFLICT, Json(mismatch_body(&m))).into_response()),
    }
}

/// Convenience for flag spelling checks on probe output (`"Seen"`, `"\\Seen"`).
pub fn has_flag(flags: &[String], name: &str) -> bool {
    flags
        .iter()
        .any(|f| f.trim_start_matches('\\').eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(validity: Option<u32>, flags: Option<&[&str]>, mid: Option<&str>) -> UidProbe {
        UidProbe {
            uid_validity: validity,
            flags: flags.map(|f| f.iter().map(|s| s.to_string()).collect()),
            message_id: mid.map(str::to_string),
        }
    }

    #[test]
    fn matching_handle_passes() {
        let expect = TargetExpectation {
            uidvalidity: Some(7),
            message_id: Some("<a@x>".into()),
        };
        assert_eq!(
            check_target(&expect, &probe(Some(7), Some(&[]), Some("a@x"))),
            Ok(())
        );
    }

    #[test]
    fn missing_uid_is_gone_not_success() {
        let expect = TargetExpectation::default();
        assert_eq!(
            check_target(&expect, &probe(Some(7), None, None)),
            Err(TargetMismatch::Gone)
        );
    }

    #[test]
    fn uidvalidity_reset_is_stale_even_when_the_uid_exists() {
        let expect = TargetExpectation {
            uidvalidity: Some(7),
            message_id: None,
        };
        assert_eq!(
            check_target(&expect, &probe(Some(8), Some(&["Seen"]), Some("b@x"))),
            Err(TargetMismatch::UidValidityChanged {
                expected: 7,
                actual: Some(8)
            })
        );
    }

    #[test]
    fn a_different_message_at_the_same_uid_is_stale() {
        let expect = TargetExpectation {
            uidvalidity: None,
            message_id: Some("a@x".into()),
        };
        assert_eq!(
            check_target(&expect, &probe(Some(7), Some(&[]), Some("b@x"))),
            Err(TargetMismatch::MessageIdChanged)
        );
        // No Message-ID on the server while the client expected one: refuse.
        assert_eq!(
            check_target(&expect, &probe(Some(7), Some(&[]), None)),
            Err(TargetMismatch::MessageIdChanged)
        );
    }

    #[test]
    fn no_expectations_only_require_existence() {
        let expect = TargetExpectation::default();
        assert_eq!(check_target(&expect, &probe(None, Some(&[]), None)), Ok(()));
    }

    #[test]
    fn mismatch_codes_are_stable() {
        assert_eq!(
            mismatch_body(&TargetMismatch::Gone)["code"],
            "message_not_found"
        );
        assert_eq!(
            mismatch_body(&TargetMismatch::UidValidityChanged {
                expected: 1,
                actual: None
            })["code"],
            "stale_uid"
        );
        assert_eq!(
            mismatch_body(&TargetMismatch::MessageIdChanged)["code"],
            "stale_uid"
        );
    }

    #[test]
    fn has_flag_accepts_both_spellings() {
        let flags = vec!["Seen".to_string(), "\\Flagged".to_string()];
        assert!(has_flag(&flags, "seen"));
        assert!(has_flag(&flags, "Flagged"));
        assert!(!has_flag(&flags, "answered"));
    }
}
