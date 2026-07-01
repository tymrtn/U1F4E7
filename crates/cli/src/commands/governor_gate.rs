// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! CLI/MCP-side glue for the Governor send gate.
//!
//! The actual decision engine lives in `envelope_email_transport::outbound`
//! (which shells out to the real Governor CLI). This module only wires that gate
//! into the CLI/MCP send primitives: it resolves config from the environment,
//! runs the gate, and records a sanitized audit/event row. No message bodies,
//! full recipient addresses, attachment bytes, or secrets are ever logged here.

use envelope_email_store::{Database, Event};
use envelope_email_transport::attribution::{
    AttributedSendContext, classify_sensitive_attachment, collect_recipient_domains,
};
use envelope_email_transport::outbound::{
    GovernorConfig, GovernorOutcome, GovernorRequest, SendSurface, gate,
};
use envelope_email_transport::smtp::Attachment;

/// Build the attributed Governor request for an actual-send attempt.
///
/// This is the single place the CLI and MCP send surfaces derive their
/// blind-attribution keys, so they converge on identical semantics: thread /
/// domain / recipient shape from the headers, plus attachment sensitivity
/// classified from filenames (class only — bytes are never inspected). Store
/// relationship facts and content classifiers are left unknown (omitted) until
/// they are wired; they are never fabricated.
#[allow(clippy::too_many_arguments)]
pub(crate) fn governor_request(
    account_id: &str,
    account_domain: Option<String>,
    subject: &str,
    to: &str,
    cc: Option<&str>,
    bcc: Option<&str>,
    surface: SendSurface,
    draft_id: Option<&str>,
    attachments: &[Attachment],
    is_reply: bool,
) -> GovernorRequest {
    let summary = collect_recipient_domains(to, cc, bcc);
    let sensitive_attachment = attachments
        .iter()
        .any(|a| classify_sensitive_attachment(&a.filename, &a.content_type));
    let ctx = AttributedSendContext {
        account_domain,
        recipient_domains: summary.domains,
        recipient_count: summary.count,
        is_reply,
        has_bcc: summary.has_bcc,
        attachment_count: attachments.len(),
        sensitive_attachment,
        ..Default::default()
    };
    let sizes: Vec<(String, u64)> = attachments
        .iter()
        .map(|a| (a.content_type.clone(), a.data.len() as u64))
        .collect();
    GovernorRequest::from_context(account_id, subject, surface, draft_id, &sizes, &ctx)
}

/// Run the Governor gate for an actual-send attempt and persist a sanitized
/// audit event. Returns the outcome; callers must refuse SMTP unless
/// `outcome.allowed` is true.
pub(crate) fn gate_and_record(
    db: &Database,
    account_id: &str,
    req: &GovernorRequest,
) -> GovernorOutcome {
    let config = GovernorConfig::from_env();
    let outcome = gate(&config, req);
    record_governor_event(db, account_id, req, &outcome);
    outcome
}

/// Extract a lowercased domain from an account email/username, if present.
pub(crate) fn account_domain(email: &str) -> Option<String> {
    email
        .rsplit_once('@')
        .map(|(_, domain)| domain.trim().to_ascii_lowercase())
        .filter(|d| !d.is_empty())
}

fn record_governor_event(
    db: &Database,
    account_id: &str,
    req: &GovernorRequest,
    outcome: &GovernorOutcome,
) {
    let event_type = if outcome.allowed {
        "send_governor.allowed"
    } else {
        "send_governor.blocked"
    };
    let payload = serde_json::json!({
        "request": req.audit_payload(),
        "outcome": outcome.audit_json(),
    });
    let event = Event {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: account_id.to_string(),
        event_type: event_type.to_string(),
        folder: "policy".to_string(),
        uid: None,
        message_id: None,
        from_addr: None,
        subject: None,
        snippet: None,
        payload: Some(payload.to_string()),
        idempotency_key: None,
        secure_pending: false,
        acked_at: Some(chrono::Utc::now().to_rfc3339()),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let _ = db.insert_event(&event);
}
