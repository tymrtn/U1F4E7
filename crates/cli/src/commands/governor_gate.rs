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
    is_calendar_invitation_content_type,
};
use envelope_email_transport::outbound::{
    GovernorConfig, GovernorOutcome, GovernorRequest, SendSurface, gate_with_attribution,
};
use envelope_email_transport::smtp::{Attachment, envelope_recipients};

/// Build the attributed Governor request for an actual-send attempt, resolving
/// the bot's `declared` attribute keys against Envelope's host-derived facts.
///
/// This is the single place the CLI and MCP send surfaces derive their
/// blind-attribution keys, so they converge on identical semantics: thread /
/// domain / recipient shape from the headers, attachment sensitivity classified
/// from filenames (class only), bounded relationship facts from the local store,
/// plus the bot's own declarations. Classifier facts without a real classifier
/// remain unknown (omitted); they are never fabricated. Bot-originated surfaces
/// (CLI/MCP) require at least one factual declaration — host facts never
/// substitute.
///
/// `in_reply_to` is the parent Message-ID the send will carry. It earns reply
/// credit only through [`Database::is_verified_reply`]; otherwise the send
/// derives as a fresh compose, and a declared `reply_to_thread` is refused.
#[allow(clippy::too_many_arguments)]
pub(crate) fn governor_request(
    db: &Database,
    account_id: &str,
    account_domain: Option<String>,
    subject: &str,
    to: &str,
    cc: Option<&str>,
    bcc: Option<&str>,
    surface: SendSurface,
    draft_id: Option<&str>,
    attachments: &[Attachment],
    in_reply_to: Option<&str>,
    text_body: Option<&str>,
    html_body: Option<&str>,
    declared: &[String],
) -> GovernorRequest {
    let summary = collect_recipient_domains(to, cc, bcc);
    let sensitive_attachment = attachments
        .iter()
        .any(|a| classify_sensitive_attachment(&a.filename, &a.content_type));
    // Calendar invitation is a MIME-only structural fact. Do not infer it from
    // filenames, bodies, or subjects: an attachment is eligible only when its
    // declared content type is text/calendar.
    let calendar_invitation = attachments
        .iter()
        .any(|a| is_calendar_invitation_content_type(&a.content_type));
    // Judged on exactly the addresses SMTP will deliver to. Headers it can't
    // parse, a failed lookup or an oversized set get no credit and keep
    // `cold_email` (see `RelationshipFacts::UNVERIFIED`).
    let recipients = envelope_recipients(to, cc, bcc).ok();
    let relationship = db.observe_send_relationship(account_id, recipients.as_deref(), in_reply_to);
    let ctx = AttributedSendContext {
        account_domain,
        recipient_domains: summary.domains,
        recipient_count: summary.count,
        is_reply: relationship.verified_reply,
        has_bcc: summary.has_bcc,
        attachment_count: attachments.len(),
        sensitive_attachment,
        calendar_invitation,
        known_contact: relationship.known_contact,
        frequent_contact: relationship.frequent_contact,
        cold_email: relationship.cold_email,
        unknown_domain: relationship.unknown_domain,
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
    db: &Database,
    account_id: &str,
    account_domain: Option<String>,
    mailto_addr: &str,
    declared: &[String],
) -> GovernorRequest {
    let domain = mailto_addr
        .rsplit_once('@')
        .map(|(_, d)| d.trim().to_ascii_lowercase())
        .filter(|d| !d.is_empty());
    let recipients = envelope_recipients(mailto_addr, None, None).ok();
    let relationship = db.observe_send_relationship(account_id, recipients.as_deref(), None);
    let ctx = AttributedSendContext {
        account_domain,
        recipient_domains: domain.into_iter().collect(),
        recipient_count: 1,
        short_body: Some(true),
        known_contact: relationship.known_contact,
        frequent_contact: relationship.frequent_contact,
        cold_email: relationship.cold_email,
        unknown_domain: relationship.unknown_domain,
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
/// missing or invalid; returns `None` when the request is attributed and may
/// proceed to its normal send-policy disposition.
///
/// SMTP-capable Envelope processes use [`GovernorConfig::smtp`], fixed at build
/// time by the `governor` feature. The attribution precondition holds in every
/// build and gate mode, so a missing/invalid declaration on a bot-originated
/// action always blocks here, before any draft is created or any wire send
/// happens.
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
    let config = GovernorConfig::smtp();
    let resolution = req.resolution.as_ref()?;
    if resolution.is_attributed() {
        return None;
    }
    // Unattributed / invalid on a bot-originated surface. Produce the canonical
    // refusal via the gate (it does not spawn Governor for a non-attributed
    // request), record it, and block — in required, warn, and off alike.
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
    let config = GovernorConfig::smtp();
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
    use envelope_email_transport::outbound::{GovernorConfig, GovernorMode, gate_with_attribution};

    fn nonexistent_required() -> GovernorConfig {
        GovernorConfig {
            mode: GovernorMode::Required,
            bin: "/nonexistent/governor-binary-xyz".to_string(),
        }
    }

    fn cli_request(db: &Database, declared: &[&str]) -> GovernorRequest {
        let declared: Vec<String> = declared.iter().map(|s| s.to_string()).collect();
        governor_request(
            db,
            "acc1",
            Some("example.com".into()),
            "subject",
            "to@example.net",
            None,
            None,
            SendSurface::Cli,
            None,
            &[],
            None,
            Some("a short body"),
            None,
            &declared,
        )
    }

    fn gate_event_payload(db: &Database, event_type: &str) -> serde_json::Value {
        let event = db
            .list_events(Some("acc1"), 10)
            .unwrap()
            .into_iter()
            .find(|e| e.event_type == event_type)
            .unwrap_or_else(|| panic!("no {event_type} event recorded"));
        serde_json::from_str(event.payload.as_deref().expect("event payload")).unwrap()
    }

    #[test]
    fn precheck_refuses_an_undeclared_send_in_every_build() {
        let db = Database::open_memory().unwrap();
        let refusal = precheck_attribution(&db, "acc1", &cli_request(&db, &[]), None)
            .expect("an undeclared bot send is refused before any side effect");
        assert!(!refusal.allowed);
        assert_eq!(refusal.block_code.as_deref(), Some("attributes_required"));
        let payload = gate_event_payload(&db, "send_governor.attribution_refused");
        assert_eq!(payload["outcome"]["block_code"], "attributes_required");
    }

    #[cfg(not(feature = "governor"))]
    #[test]
    fn gated_send_without_the_governor_feature_records_the_off_verdict() {
        let db = Database::open_memory().unwrap();
        let req = cli_request(&db, &["informational"]);
        assert!(precheck_attribution(&db, "acc1", &req, None).is_none());

        let outcome = gate_and_record(&db, "acc1", &req);
        assert!(outcome.allowed, "attributed send proceeds unscored");
        assert_eq!(outcome.mode, GovernorMode::Off);
        // `disabled`, not `unavailable`: no Governor binary was spawned.
        assert_eq!(outcome.decision, "disabled");

        let payload = gate_event_payload(&db, "send_governor.allowed");
        assert_eq!(payload["outcome"]["mode"], "off");
        assert_eq!(payload["outcome"]["decision"], "disabled");
        assert_eq!(payload["outcome"]["attribution_state"], "attributed");
        assert_eq!(
            outcome.success_attribution().unwrap()["governor"]["mode"],
            "off"
        );
    }

    #[test]
    fn unsubscribe_request_requires_a_declaration() {
        // The mailto unsubscribe is a real SMTP surface: it requires a factual
        // declaration. With no `--attr`, it fails closed with attributes_required
        // BEFORE Governor is spawned (a nonexistent binary would otherwise be
        // governor_unavailable). Host facts (short_body, recipient domain) never
        // substitute.
        let req = unsubscribe_request(
            &Database::open_memory().unwrap(),
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
            &Database::open_memory().unwrap(),
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
            &Database::open_memory().unwrap(),
            "acc1",
            Some("example.com".into()),
            "subject",
            "to@example.com",
            None,
            None,
            SendSurface::Cli,
            None,
            &[],
            None,
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
    fn direct_governor_request_derives_known_contact_from_shared_store_history() {
        let db = Database::open_memory().unwrap();
        db.set_detected_folder("acc1", "sent", "Sent").unwrap();
        let thread = db
            .create_thread(
                "prior correspondence",
                "2026-09-01T00:00:00Z",
                "2026-09-01T00:00:00Z",
                "acc1",
            )
            .unwrap();
        db.upsert_thread_message(
            &thread.thread_id,
            1,
            Some("prior@example.test"),
            None,
            None,
            "Sent",
            "me@example.test",
            "known@example.net",
            None,
            None,
            "2026-09-01T00:00:00Z",
            "prior correspondence",
            true,
            None,
        )
        .unwrap();

        let req = governor_request(
            &db,
            "acc1",
            Some("example.test".into()),
            "subject",
            "known@example.net",
            None,
            None,
            SendSurface::Cli,
            None,
            &[],
            None,
            Some("short body"),
            None,
            &["known_contact".to_string()],
        );
        let resolution = req.resolution.unwrap();
        assert!(
            resolution.is_attributed(),
            "{:?}",
            resolution.rejected_attrs
        );
        assert!(
            resolution
                .accepted_redundant
                .contains(&"known_contact".to_string())
        );
        assert!(
            resolution
                .governor_attrs
                .contains(&"known_contact".to_string())
        );
        assert!(
            !resolution
                .governor_attrs
                .contains(&"cold_email".to_string())
        );
    }

    #[test]
    fn governor_request_derives_short_body_from_html_only_body() {
        // Real evidence: an HTML-only send left `short_body` unobserved because
        // the boundary inspected only the text alternative. The canonical policy
        // now counts the HTML's visible text, so a truthful `short_body`
        // declaration on an HTML-only message is corroborated.
        let req = governor_request(
            &Database::open_memory().unwrap(),
            "acc1",
            Some("example.com".into()),
            "subject",
            "to@example.com",
            None,
            None,
            SendSurface::Cli,
            None,
            &[],
            None,
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
            &Database::open_memory().unwrap(),
            "acc1",
            Some("example.com".into()),
            "subject",
            "to@example.com",
            None,
            None,
            SendSurface::Cli,
            None,
            &[],
            None,
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
            &Database::open_memory().unwrap(),
            "acc1",
            Some("example.com".into()),
            "subject",
            "to@example.com",
            None,
            None,
            SendSurface::Cli,
            None,
            &[],
            None,
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
            &Database::open_memory().unwrap(),
            "acc1",
            Some("example.com".into()),
            "list@vendor.example",
            &["tyler_approved".to_string()],
        );
        let outcome = gate_with_attribution(&nonexistent_required(), &req);
        assert!(!outcome.allowed);
        assert_eq!(outcome.block_code.as_deref(), Some("attributes_invalid"));
    }

    // ── Injection-safe relationship derivation ──────────────────────────
    //
    // Each test below reproduces a path by which content an attacker controls
    // (an inbound message, or an agent acting on one) could lift a send to a
    // stranger out of first-contact scoring.

    fn relationship_db() -> Database {
        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acc1', 'Me', 'me@example.test', 'example.test',
                         'smtp.example.test', 587, 'imap.example.test', 993, 'encrypted')",
                [],
            )
            .unwrap();
        db.set_detected_folder("acc1", "sent", "Sent").unwrap();
        db
    }

    #[allow(clippy::too_many_arguments)]
    fn cache_message(
        db: &Database,
        thread_id: Option<&str>,
        message_id: &str,
        folder: &str,
        from: &str,
        to: &str,
        cc: Option<&str>,
        is_outbound: bool,
    ) -> String {
        let thread_id = match thread_id {
            Some(id) => id.to_string(),
            None => {
                db.create_thread(
                    "project",
                    "2026-09-01T00:00:00Z",
                    "2026-09-01T00:00:00Z",
                    "acc1",
                )
                .unwrap()
                .thread_id
            }
        };
        let uid = db
            .conn()
            .query_row("SELECT COUNT(*) + 1 FROM thread_messages", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap() as u32;
        db.upsert_thread_message(
            &thread_id,
            uid,
            Some(message_id),
            None,
            None,
            folder,
            from,
            to,
            cc,
            None,
            "2026-09-01T00:00:00Z",
            "project",
            is_outbound,
            None,
        )
        .unwrap();
        thread_id
    }

    /// A thread the account really took part in: its own message to
    /// `known@example.net` in Sent, then that contact's answer in INBOX.
    fn verified_thread(db: &Database) {
        let thread = cache_message(
            db,
            None,
            "<mine-1@example.test>",
            "Sent",
            "me@example.test",
            "known@example.net",
            None,
            true,
        );
        cache_message(
            db,
            Some(&thread),
            "<theirs-2@example.net>",
            "INBOX",
            "known@example.net",
            "me@example.test",
            None,
            false,
        );
    }

    /// Resolve attribution for a stored draft exactly as CLI `draft send` /
    /// MCP `send_draft` do before transmission.
    fn draft_resolution(
        db: &Database,
        to: &str,
        cc: Option<&str>,
        in_reply_to: Option<&str>,
        declared: &[&str],
    ) -> envelope_email_transport::attribution::AttributionResolution {
        let draft = db
            .create_draft(
                "acc1",
                to,
                Some("Re: project"),
                Some("a short body"),
                None,
                in_reply_to,
                cc,
                None,
                Some("agent"),
            )
            .unwrap();
        let declared: Vec<String> = declared.iter().map(|s| s.to_string()).collect();
        crate::commands::drafts::precheck_draft(db, &draft.id, SendSurface::Cli, &declared, None)
            .unwrap()
            .resolution
    }

    fn has(attrs: &[String], key: &str) -> bool {
        attrs.iter().any(|a| a == key)
    }

    #[test]
    fn spoofed_self_from_in_inbox_does_not_corroborate_known_contact() {
        let db = relationship_db();
        cache_message(
            &db,
            None,
            "<spoof@evil.test>",
            "INBOX",
            "me@example.test",
            "attacker@evil.test",
            None,
            true,
        );
        let res = draft_resolution(&db, "attacker@evil.test", None, None, &["known_contact"]);
        assert!(
            res.rejected_attrs
                .iter()
                .any(|r| r.key == "known_contact" && r.code == "conflicts_with_host_observation"),
            "a forged self-From in INBOX must not vouch: {res:?}"
        );
        assert!(has(&res.derived_attrs, "cold_email"), "{res:?}");
    }

    #[test]
    fn forged_in_reply_to_earns_no_reply_credit() {
        let db = relationship_db();
        let res = draft_resolution(
            &db,
            "fresh@stranger.test",
            None,
            Some("<forged@nowhere.test>"),
            &["informational"],
        );
        assert!(!has(&res.derived_attrs, "reply_to_thread"), "{res:?}");
        assert!(has(&res.derived_attrs, "cold_email"), "{res:?}");
        assert!(has(&res.derived_attrs, "unknown_domain"), "{res:?}");
    }

    #[test]
    fn replying_to_a_strangers_thread_keeps_cold_email() {
        let db = relationship_db();
        cache_message(
            &db,
            None,
            "<attack@evil.test>",
            "INBOX",
            "attacker@evil.test",
            "me@example.test",
            None,
            false,
        );
        let res = draft_resolution(
            &db,
            "attacker@evil.test",
            None,
            Some("<attack@evil.test>"),
            &["informational"],
        );
        assert!(!has(&res.derived_attrs, "reply_to_thread"), "{res:?}");
        assert!(has(&res.derived_attrs, "cold_email"), "{res:?}");
    }

    #[test]
    fn a_reply_that_adds_a_new_cc_keeps_first_contact_signals() {
        let db = relationship_db();
        verified_thread(&db);
        let res = draft_resolution(
            &db,
            "known@example.net",
            Some("attacker@evil.test"),
            Some("<theirs-2@example.net>"),
            &["informational"],
        );
        assert!(!has(&res.derived_attrs, "reply_to_thread"), "{res:?}");
        assert!(has(&res.derived_attrs, "cold_email"), "{res:?}");
        assert!(has(&res.derived_attrs, "unknown_domain"), "{res:?}");
    }

    #[test]
    fn a_reply_in_a_verified_thread_keeps_reply_credit() {
        let db = relationship_db();
        verified_thread(&db);
        let res = draft_resolution(
            &db,
            "known@example.net",
            None,
            Some("<theirs-2@example.net>"),
            &["reply_to_thread"],
        );
        assert!(res.is_attributed(), "{res:?}");
        assert!(has(&res.derived_attrs, "reply_to_thread"), "{res:?}");
        assert!(has(&res.derived_attrs, "known_contact"), "{res:?}");
        assert!(!has(&res.derived_attrs, "cold_email"), "{res:?}");
    }

    /// Pins a behaviour change that reaches release builds too: attribution
    /// runs whether or not Governor does, so declaring `reply_to_thread` on a
    /// reply the account has no verified history for refuses the send. Dropping
    /// the key sends it as new mail.
    #[test]
    fn a_declared_reply_to_thread_without_verified_history_is_refused_in_every_build() {
        let db = relationship_db();
        cache_message(
            &db,
            None,
            "<hello@newcomer.test>",
            "INBOX",
            "newcomer@newcomer.test",
            "me@example.test",
            None,
            false,
        );
        let res = draft_resolution(
            &db,
            "newcomer@newcomer.test",
            None,
            Some("<hello@newcomer.test>"),
            &["reply_to_thread", "informational"],
        );
        let rejected = res
            .rejected_attrs
            .iter()
            .find(|r| r.key == "reply_to_thread")
            .expect("reply_to_thread is refused");
        assert_eq!(rejected.code, "conflicts_with_host_observation");
        assert!(
            rejected
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("resubmit without `reply_to_thread`")),
            "{rejected:?}"
        );
        assert!(!res.is_attributed(), "{res:?}");

        let res = draft_resolution(
            &db,
            "newcomer@newcomer.test",
            None,
            Some("<hello@newcomer.test>"),
            &["informational"],
        );
        assert!(res.is_attributed(), "{res:?}");
    }

    /// Injection path: an address SMTP delivers to but a looser header parse
    /// drops (a `<` inside a quoted display name, a Unicode, underscore or
    /// single-label domain) used to vanish from the relationship check, so a
    /// verified thread plus that Cc kept full reply credit.
    #[test]
    fn a_cc_only_smtp_can_parse_still_counts_as_a_recipient() {
        for cc in [
            "\"Name <extra>\" <attacker@evil.test>",
            "attacker@exämple.com",
            "attacker@ex_ample.com",
            "attacker@localhost",
        ] {
            let db = relationship_db();
            verified_thread(&db);
            let res = draft_resolution(
                &db,
                "known@example.net",
                Some(cc),
                Some("<theirs-2@example.net>"),
                &["informational"],
            );
            assert!(!has(&res.derived_attrs, "reply_to_thread"), "{cc}: {res:?}");
            assert!(!has(&res.derived_attrs, "known_contact"), "{cc}: {res:?}");
            assert!(has(&res.derived_attrs, "cold_email"), "{cc}: {res:?}");
            assert!(has(&res.derived_attrs, "unknown_domain"), "{cc}: {res:?}");
        }
    }

    #[test]
    fn a_lone_recipient_only_smtp_can_parse_is_a_first_contact() {
        let db = relationship_db();
        verified_thread(&db);
        let res = draft_resolution(&db, "attacker@exämple.com", None, None, &["informational"]);
        assert!(has(&res.derived_attrs, "cold_email"), "{res:?}");
        assert!(has(&res.derived_attrs, "unknown_domain"), "{res:?}");
    }

    /// The first-contact signals used to give up once the account had more
    /// than 256 cached Sent rows, so on a real mailbox a stranger scored with
    /// no relationship facts at all.
    #[test]
    fn relationship_facts_hold_on_a_large_sent_history() {
        let db = relationship_db();
        verified_thread(&db);
        for index in 0..300 {
            cache_message(
                &db,
                None,
                &format!("<bulk-{index}@example.test>"),
                "Sent",
                "me@example.test",
                &format!("colleague-{index}@example.org"),
                None,
                true,
            );
        }

        for (to, cc) in [
            ("stranger@evil.test", None),
            ("known@example.net", Some("stranger@evil.test")),
        ] {
            let res = draft_resolution(&db, to, cc, None, &["informational"]);
            assert!(
                has(&res.derived_attrs, "cold_email"),
                "{to} {cc:?}: {res:?}"
            );
            assert!(
                has(&res.derived_attrs, "unknown_domain"),
                "{to} {cc:?}: {res:?}"
            );
        }

        // The oldest verified correspondent is still found past the newest 256.
        let res = draft_resolution(
            &db,
            "known@example.net",
            None,
            Some("<theirs-2@example.net>"),
            &["reply_to_thread", "known_contact"],
        );
        assert!(res.is_attributed(), "{res:?}");
        assert!(!has(&res.derived_attrs, "unknown_domain"), "{res:?}");
    }

    /// More recipients than the lookup checks used to leave every fact unknown,
    /// dropping the first-contact signal. The set now counts as containing one.
    #[test]
    fn a_recipient_set_over_the_lookup_limit_carries_the_first_contact_signal() {
        let db = relationship_db();
        verified_thread(&db);
        let many: Vec<String> = (0..envelope_email_store::RELATIONSHIP_FACT_RECIPIENT_LIMIT)
            .map(|index| format!("stranger-{index}@evil.test"))
            .collect();
        let res = draft_resolution(
            &db,
            "known@example.net",
            Some(&many.join(", ")),
            Some("<theirs-2@example.net>"),
            &["informational"],
        );
        assert!(!has(&res.derived_attrs, "reply_to_thread"), "{res:?}");
        assert!(!has(&res.derived_attrs, "known_contact"), "{res:?}");
        assert!(has(&res.derived_attrs, "cold_email"), "{res:?}");
    }

    /// A store error used to become "no facts": no credit, but no first-contact
    /// signal either. It now withholds credit and keeps the signal.
    #[test]
    fn a_failed_relationship_lookup_keeps_the_first_contact_signal() {
        let db = relationship_db();
        verified_thread(&db);
        db.conn()
            .execute_batch("DROP TABLE detected_folders")
            .unwrap();
        let res = draft_resolution(
            &db,
            "known@example.net",
            None,
            Some("<theirs-2@example.net>"),
            &["informational"],
        );
        assert!(!has(&res.derived_attrs, "reply_to_thread"), "{res:?}");
        assert!(!has(&res.derived_attrs, "known_contact"), "{res:?}");
        assert!(has(&res.derived_attrs, "cold_email"), "{res:?}");
    }

    /// With no Sent history cached yet, the account's own domain used to derive
    /// `unknown_domain`, which then refused an honest `internal_domain`.
    #[test]
    fn the_account_domain_is_never_an_unknown_domain() {
        let db = relationship_db();
        let res = draft_resolution(
            &db,
            "colleague@example.test",
            None,
            None,
            &["internal_domain"],
        );
        assert!(res.is_attributed(), "{res:?}");
        assert!(!has(&res.derived_attrs, "unknown_domain"), "{res:?}");
        assert!(has(&res.derived_attrs, "cold_email"), "{res:?}");
    }
}
