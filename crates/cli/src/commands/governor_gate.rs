// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! CLI/MCP-side glue for the Governor send gate and the attribution protocol.
//!
//! The actual decision engine lives in `envelope_email_transport::outbound`
//! (which shells out to the real Governor CLI). This module wires that gate into
//! the CLI/MCP send primitives: it derives Envelope's host-observed attributes,
//! resolves them against the bot's declared attributes, refuses an
//! unattributed/invalid request **before** any side effect, and records a
//! sanitized audit/event row. No message bodies, full recipient addresses,
//! attachment bytes, or secrets are ever logged here, and **no numeric Governor
//! score is ever recorded** in Envelope audit/event payloads.

use envelope_email_store::{Database, Event};
use envelope_email_transport::attribution::{
    AttributedSendContext, classify_sensitive_attachment, collect_recipient_domains,
};
use envelope_email_transport::outbound::{
    GovernorConfig, GovernorMode, GovernorOutcome, GovernorRequest, SendSurface,
    gate_with_attribution,
};
use envelope_email_transport::smtp::Attachment;

/// Build the attributed Governor request for an actual-send attempt, resolving
/// the bot's `declared` attribute keys against Envelope's host-derived facts.
///
/// This is the single place the CLI and MCP send surfaces derive their
/// blind-attribution keys, so they converge on identical semantics: thread /
/// domain / recipient shape from the headers, attachment sensitivity classified
/// from filenames (class only), plus the bot's own declarations. Store
/// relationship facts and content classifiers are left unknown (omitted) until
/// they are wired; they are never fabricated. Bot-originated surfaces (CLI/MCP)
/// require at least one factual declaration — host facts never substitute.
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
    text_body: Option<&str>,
    html_body: Option<&str>,
    declared: &[String],
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
        // Derive `short_body` from the FINAL bodies actually being sent via the
        // one canonical policy, so a bot's `short_body` declaration is
        // corroborated (not rejected host_verification_unavailable) for every
        // body shape — text, HTML-only, dual, and empty. With the final bodies
        // in hand `short_body` is always observable, never left unknown.
        short_body: Some(envelope_email_transport::attribution::final_body_is_short(
            text_body, html_body,
        )),
        ..Default::default()
    };
    let sizes: Vec<(String, u64)> = attachments
        .iter()
        .map(|a| (a.content_type.clone(), a.data.len() as u64))
        .collect();
    // Bot-originated actual-send surfaces must carry a factual declaration.
    let require_declaration = matches!(surface, SendSurface::Cli | SendSurface::Mcp);
    GovernorRequest::from_context_with_declared(
        account_id,
        subject,
        surface,
        draft_id,
        &sizes,
        &ctx,
        declared,
        require_declaration,
    )
}

/// Build a Governor request for a `mailto:` compliance unsubscribe SMTP send.
///
/// The `mailto:` unsubscribe is a real SMTP surface, so it is gated like any
/// other actual send: this is an agent-facing CLI path with no authenticated
/// human-only attestation, so it **requires** a non-empty valid declaration
/// (`require_declaration = true`) supplied via repeatable `--attr`. Host-derived
/// facts (recipient domain, the empty-body `short_body`) never substitute for the
/// declaration; an empty/invalid `--attr` set fails closed before Governor/SMTP.
pub(crate) fn unsubscribe_request(
    account_id: &str,
    account_domain: Option<String>,
    mailto_addr: &str,
    declared: &[String],
) -> GovernorRequest {
    let domain = mailto_addr
        .rsplit_once('@')
        .map(|(_, d)| d.trim().to_ascii_lowercase())
        .filter(|d| !d.is_empty());
    let ctx = AttributedSendContext {
        account_domain,
        recipient_domains: domain.into_iter().collect(),
        recipient_count: 1,
        short_body: Some(true),
        ..Default::default()
    };
    GovernorRequest::from_context_with_declared(
        account_id,
        "unsubscribe",
        SendSurface::Cli,
        None,
        &[],
        &ctx,
        declared,
        true,
    )
}

/// Resolve attribution **before any side effect**. Returns the canonical
/// refusal outcome (already recorded in audit) when the declared+derived set is
/// missing or invalid; returns `None` when the request may proceed (attributed,
/// or `off` mode — the documented gate kill-switch).
///
/// The attribution precondition fails closed in **both** `required` and `warn`
/// modes: warn softens a Governor *verdict* on an already-attributed request, but
/// it never waives Envelope's attribution protocol. A missing/invalid declaration
/// on a bot-originated action always blocks here, before any draft is created or
/// any wire send happens.
///
/// This runs at queue time on every agent surface so a bot learns about a
/// problem immediately rather than discovering a parked draft later; the actual
/// Governor decision still runs at transmission (immediate path or sweep) via
/// [`gate_and_record`].
pub(crate) fn precheck_attribution(
    db: &Database,
    account_id: &str,
    req: &GovernorRequest,
    agent_id: Option<&str>,
) -> Option<GovernorOutcome> {
    let config = GovernorConfig::from_env();
    if config.mode == GovernorMode::Off {
        // Off is the documented operator kill-switch: it disables the gate and the
        // attribution precondition alike.
        return None;
    }
    let resolution = req.resolution.as_ref()?;
    if resolution.is_attributed() {
        return None;
    }
    // Unattributed / invalid on a bot-originated surface. Produce the canonical
    // refusal via the gate (it does not spawn Governor for a non-attributed
    // request), record it, and block — in required and warn alike.
    let outcome = gate_with_attribution(&config, &req.clone().with_agent_id(agent_id));
    record_governor_event(db, account_id, req, &outcome, agent_id);
    Some(outcome)
}

/// Run the Governor gate for an actual-send attempt and persist a sanitized
/// audit event. Returns the outcome; callers must refuse SMTP unless
/// `outcome.allowed` is true.
pub(crate) fn gate_and_record(
    db: &Database,
    account_id: &str,
    req: &GovernorRequest,
) -> GovernorOutcome {
    gate_and_record_with_agent(db, account_id, req, None)
}

/// Like [`gate_and_record`], but attributes the gate decision and its audit
/// event to a specific agent (audit-only; the agent id never widens the gate).
pub(crate) fn gate_and_record_with_agent(
    db: &Database,
    account_id: &str,
    req: &GovernorRequest,
    agent_id: Option<&str>,
) -> GovernorOutcome {
    let config = GovernorConfig::from_env();
    let req = req.clone().with_agent_id(agent_id);
    let outcome = gate_with_attribution(&config, &req);
    record_governor_event(db, account_id, &req, &outcome, agent_id);
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
    agent_id: Option<&str>,
) {
    let event_type = if outcome.allowed {
        "send_governor.allowed"
    } else if outcome.is_attribution_failure() {
        "send_governor.attribution_refused"
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
    let _ = db.insert_event_with_agent(&event, agent_id);

    // Also emit the canonical catalog `governor_blocked` event for a genuine
    // gate block so durable delivery routes can subscribe by its stable wire
    // name. Attribution refusals are protocol errors, not gate blocks, so they
    // are recorded above but do not masquerade as `governor_blocked`.
    if !outcome.allowed && outcome.block_code.as_deref() == Some("governor_blocked") {
        let _ = db.emit_catalog_event(
            account_id,
            envelope_email_store::event_catalog::GOVERNOR_BLOCKED,
            Some(serde_json::json!({ "outcome": outcome.audit_json() })),
            agent_id,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use envelope_email_transport::outbound::{GovernorConfig, gate_with_attribution};

    fn nonexistent_required() -> GovernorConfig {
        GovernorConfig {
            mode: GovernorMode::Required,
            bin: "/nonexistent/governor-binary-xyz".to_string(),
        }
    }

    #[test]
    fn unsubscribe_request_requires_a_declaration() {
        // The mailto unsubscribe is a real SMTP surface: it requires a factual
        // declaration. With no `--attr`, it fails closed with attributes_required
        // BEFORE Governor is spawned (a nonexistent binary would otherwise be
        // governor_unavailable). Host facts (short_body, recipient domain) never
        // substitute.
        let req = unsubscribe_request(
            "acc1",
            Some("example.com".into()),
            "list@vendor.example",
            &[],
        );
        assert!(req.require_declaration);
        let outcome = gate_with_attribution(&nonexistent_required(), &req);
        assert!(!outcome.allowed);
        assert!(outcome.is_attribution_failure());
        assert_eq!(outcome.block_code.as_deref(), Some("attributes_required"));
        assert_ne!(outcome.decision, "unavailable");
    }

    #[test]
    fn unsubscribe_request_with_valid_declaration_reaches_governor() {
        // A valid declaration (informational is true of an unsubscribe) resolves
        // attributed and actually spawns Governor — a missing binary is then an
        // operator-side governor_unavailable, NOT an attribution failure.
        let req = unsubscribe_request(
            "acc1",
            Some("example.com".into()),
            "list@vendor.example",
            &["informational".to_string()],
        );
        let outcome = gate_with_attribution(&nonexistent_required(), &req);
        assert!(!outcome.allowed);
        assert!(!outcome.is_attribution_failure());
        assert_eq!(outcome.block_code.as_deref(), Some("governor_unavailable"));
    }

    #[test]
    fn governor_request_derives_short_body_from_the_final_body() {
        // Regression: a bot declaring `short_body` on a genuinely short body must
        // be corroborated — not rejected `host_verification_unavailable` because
        // the send boundary failed to observe the body (real evidence case C).
        let short = "just a handful of words in this short body";
        let req = governor_request(
            "acc1",
            Some("example.com".into()),
            "subject",
            "to@example.com",
            None,
            None,
            SendSurface::Cli,
            None,
            &[],
            false,
            Some(short),
            None,
            &["short_body".to_string()],
        );
        let res = req.resolution.expect("governor_request always resolves");
        assert!(
            res.is_attributed(),
            "short body should corroborate declared short_body: {:?}",
            res.rejected_attrs
        );
        assert!(res.governor_attrs.contains(&"short_body".to_string()));
        assert!(res.accepted_redundant.contains(&"short_body".to_string()));
    }

    #[test]
    fn governor_request_derives_short_body_from_html_only_body() {
        // Real evidence: an HTML-only send left `short_body` unobserved because
        // the boundary inspected only the text alternative. The canonical policy
        // now counts the HTML's visible text, so a truthful `short_body`
        // declaration on an HTML-only message is corroborated.
        let req = governor_request(
            "acc1",
            Some("example.com".into()),
            "subject",
            "to@example.com",
            None,
            None,
            SendSurface::Cli,
            None,
            &[],
            false,
            None,
            Some("<html><body><p>a short html-only note</p></body></html>"),
            &["short_body".to_string()],
        );
        let res = req.resolution.expect("governor_request always resolves");
        assert!(
            res.is_attributed(),
            "html-only short body must corroborate declared short_body: {:?}",
            res.rejected_attrs
        );
        assert!(res.accepted_redundant.contains(&"short_body".to_string()));
    }

    #[test]
    fn governor_request_rejects_short_body_declaration_on_a_long_body() {
        // The derivation is honest in both directions: declaring `short_body` on a
        // long body contradicts Envelope's observation and fails the request.
        let long = vec!["word"; 150].join(" ");
        let req = governor_request(
            "acc1",
            Some("example.com".into()),
            "subject",
            "to@example.com",
            None,
            None,
            SendSurface::Cli,
            None,
            &[],
            false,
            Some(&long),
            None,
            &["short_body".to_string()],
        );
        let res = req.resolution.expect("governor_request always resolves");
        assert!(!res.is_attributed());
        assert!(
            res.rejected_attrs
                .iter()
                .any(|r| r.key == "short_body" && r.code == "conflicts_with_host_observation"),
            "long body must contradict declared short_body: {:?}",
            res.rejected_attrs
        );
    }

    #[test]
    fn governor_request_accepts_agent_drafted_as_declarable_author_context() {
        // agent_drafted is now declarable author-context: a bot declaring it on a
        // generic CLI process is accepted, never rejected
        // host_verification_unavailable (real evidence case C).
        let req = governor_request(
            "acc1",
            Some("example.com".into()),
            "subject",
            "to@example.com",
            None,
            None,
            SendSurface::Cli,
            None,
            &[],
            false,
            Some("a short body"),
            None,
            &["agent_drafted".to_string()],
        );
        let res = req.resolution.expect("governor_request always resolves");
        assert!(res.is_attributed(), "{:?}", res.rejected_attrs);
        assert!(res.governor_attrs.contains(&"agent_drafted".to_string()));
        assert!(!res.derived_attrs.contains(&"agent_drafted".to_string()));
    }

    #[test]
    fn unsubscribe_request_rejects_invalid_declaration() {
        // An attestation-only key can never be declared here either.
        let req = unsubscribe_request(
            "acc1",
            Some("example.com".into()),
            "list@vendor.example",
            &["tyler_approved".to_string()],
        );
        let outcome = gate_with_attribution(&nonexistent_required(), &req);
        assert!(!outcome.allowed);
        assert_eq!(outcome.block_code.as_deref(), Some("attributes_invalid"));
    }
}
