// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result};
use envelope_email_store::{CredentialBackend, Database, Event};
use envelope_email_transport::attribution_persist::success_attribution_block;
use envelope_email_transport::outbound::{
    GovernorConfig, GovernorMode, IMMEDIATE_SEND_CONFIRM_CODE, OUTBOX_COOLDOWN_REASON,
    OUTBOX_COOLDOWN_REASON_CODE, SendDisposition, SendSurface, resolve_cooldown_seconds,
    resolve_disposition,
};
use envelope_email_transport::smtp::Attachment;
use envelope_email_transport::smtp_submit::AccountConnector;
use envelope_email_transport::{
    SendMode, SendPolicyDecision, SendPolicyInput, audit_event_for, evaluate,
};
use std::str::FromStr;

use super::attachments::{attachment_summaries, snapshot_attachments};
use super::authored_body::{AuthoredBody, attach_notice};
use super::common::setup_credentials;
use super::datetime::parse_send_at;
use super::drafts::{persist_from_override, validate_from_override};
use super::governor_gate::{account_domain, governor_request, precheck_attribution};
use super::re_subject_guard::check_new_re_subject_guard;
use super::send_attempt::{GovernorRefused, Queued, SendNotConfirmed, SendRequest, queue_request};
use super::ui;

/// Build lightweight attachment metadata (filename + content type, no bytes) for
/// the attribution precheck. The full bytes are only read later on an actual
/// transmit; attribution needs only the count and filename classification.
fn attachment_metadata(attach_paths: &[String]) -> Vec<Attachment> {
    attach_paths
        .iter()
        .map(|p| {
            let path = std::path::Path::new(p);
            let filename = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("attachment")
                .to_string();
            let content_type = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();
            Attachment {
                filename,
                content_type,
                data: Vec::new(),
            }
        })
        .collect()
}

/// Print a send result, carrying the input-normalization notice when the
/// authored body needed repair. Every JSON outcome an agent can act on goes
/// through here so the repair is never invisible.
fn emit_json(mut value: serde_json::Value, authored: &AuthoredBody) {
    attach_notice(&mut value, authored);
    println!("{value}");
}

/// Send an email immediately, or schedule it for later with `--at`.
#[tokio::main]
pub async fn run(
    to: &str,
    subject: &str,
    body: Option<&str>,
    html: Option<&str>,
    from: Option<&str>,
    cc: Option<&str>,
    bcc: Option<&str>,
    reply_to: Option<&str>,
    attach_paths: &[String],
    attr: &[String],
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
    at: Option<&str>,
    send_mode: &str,
    confirm_send: bool,
    allow_recipients: &[String],
    confirm_new_re_subject: bool,
    cooldown_seconds: Option<i64>,
    send_now: bool,
    confirm_send_now: bool,
    idempotency_key: Option<&str>,
) -> Result<()> {
    check_new_re_subject_guard(Some(subject), false, confirm_new_re_subject, json)?;
    if let Some(key) = idempotency_key {
        envelope_email_store::send_attempts::validate_idempotency_key(key)?;
    }

    // Repair a body whose line breaks arrived as literal `\n` text before it
    // reaches the draft record, RFC822, or SMTP. In JSON mode the notice rides
    // on the result object; in human mode it is printed here, up front, because
    // the operator should see it whether or not the send goes through.
    let authored = AuthoredBody::new(body, html);
    let body = authored.text();
    let html = authored.html();
    if !json {
        authored.print_notice(None);
    }

    let (db, creds) = setup_credentials(account, backend)?;
    let from = validate_from_override(from)?;
    let mode = SendMode::from_str(send_mode).map_err(|e| anyhow::anyhow!(e))?;
    let policy_input = SendPolicyInput {
        to,
        cc,
        bcc,
        confirm_send,
        allow_recipients,
    };
    let decision = evaluate(mode, &policy_input);
    record_send_policy_event(&db, &creds.account.id, mode, &decision, &policy_input)?;

    match &decision {
        SendPolicyDecision::Allowed => {}
        SendPolicyDecision::DraftOnly => {
            let draft_attachments = snapshot_attachments(attach_paths)?;
            let draft = db
                .create_draft(
                    &creds.account.id,
                    to,
                    Some(subject),
                    body,
                    html,
                    None,
                    cc,
                    bcc,
                    Some("cli"),
                )
                .context("failed to create send-policy draft")?;
            if !draft_attachments.is_empty() {
                db.update_draft_attachments(&draft.id, &draft_attachments)
                    .context("failed to persist draft attachments")?;
            }
            persist_from_override(&db, &draft.id, from)?;
            let attachment_summary = attachment_summaries(&draft_attachments);
            if json {
                emit_json(
                    crate::commands::contract::send_body::cli_drafted(
                        serde_json::json!(mode),
                        &draft.id,
                        to,
                        subject,
                        serde_json::json!(attachment_summary),
                        ui::draft_ui(&creds.account.id, &draft.id),
                    ),
                    &authored,
                );
            } else {
                println!(
                    "Drafted instead of sending ({mode}). Draft ID: {}",
                    draft.id
                );
                if !attachment_summary.is_empty() {
                    println!("Attachments: {}", attachment_summary.len());
                    for a in &attachment_summary {
                        println!(
                            "  - {} ({} bytes, {})",
                            a["filename"].as_str().unwrap_or("attachment"),
                            a["size"].as_u64().unwrap_or(0),
                            a["content_type"]
                                .as_str()
                                .unwrap_or("application/octet-stream"),
                        );
                    }
                }
            }
            return Ok(());
        }
        SendPolicyDecision::Denied(denial) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "denied",
                        "error": denial,
                        "send_mode": mode,
                        "ui": ui::account_ui(&creds.account.id),
                    })
                );
            }
            anyhow::bail!("send denied by policy: {} ({})", denial.reason, denial.code);
        }
    }

    // ── Attribution precheck (before ANY side effect) ──
    //
    // A bot-originated send must carry at least one factual declared attribute.
    // This runs before the scheduled/queued draft is ever created, so a missing
    // or invalid declaration produces the canonical recovery payload with no
    // draft, no SMTP, and no Governor spawn.
    let declared: Vec<String> = attr.to_vec();
    let precheck_attachments = attachment_metadata(attach_paths);
    let precheck_req = governor_request(
        &db,
        &creds.account.id,
        account_domain(&creds.account.username),
        subject,
        to,
        cc,
        bcc,
        SendSurface::Cli,
        None,
        &precheck_attachments,
        None,
        body,
        html,
        &declared,
    );
    if let Some(outcome) = precheck_attribution(&db, &creds.account.id, &precheck_req, None)? {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "status": outcome.status_str(),
                    "error": outcome.error_json(),
                    "ui": ui::account_ui(&creds.account.id),
                })
            );
        }
        anyhow::bail!("{}", outcome.reason_string());
    }

    // The validated resolution for the additive success `attribution` block. On
    // queued/scheduled acceptance the real Governor decision runs later at the
    // sweep, so the block is marked deferred (governor null; the off verdict in
    // a build without the `governor` feature).
    let queued_attribution = precheck_req
        .resolution
        .as_ref()
        .map(|r| success_attribution_block(r, None, None, true));

    // Every accepted request below is first written as a durable send intent
    // (content, attachment bytes and declaration in one row), so rerunning the
    // same command answers from that record instead of sending again. The
    // validated declaration is bound to the queued/scheduled row in ONE atomic
    // store CAS (declaration + schedule + due status) via
    // `queue_bot_draft_for_send`, so the sweep gates on the SAME declaration.
    let snapshots = snapshot_attachments(attach_paths)?;
    let request = SendRequest {
        surface: SendSurface::Cli,
        label: "cli_send",
        principal: "local".to_string(),
        agent_id: None,
        idempotency_key,
        to,
        cc,
        bcc,
        reply_to,
        subject,
        text: body,
        html,
        from,
        in_reply_to: None,
        references: &[],
        attachments: &snapshots,
        declared: &declared,
        metadata: serde_json::json!({}),
        created_by: "cli",
    };

    // ── Scheduled send path ──
    if let Some(at_str) = at {
        let send_at = parse_send_at(at_str).context("failed to parse --at value")?;
        let (draft, replay) = match queue_or_report(
            &db,
            &creds.account.id,
            &request,
            &send_at,
            None,
            json,
            &authored,
        )? {
            Some(queued) => queued,
            None => return Ok(()),
        };
        if json {
            let mut body = crate::commands::contract::send_body::cli_scheduled_at(
                &draft.id,
                draft.send_after.as_deref().unwrap_or(&send_at),
                serde_json::json!(attachment_summaries(&draft.attachments)),
                serde_json::json!(queued_attribution),
                ui::draft_ui(&creds.account.id, &draft.id),
            );
            body["idempotent_replay"] = serde_json::json!(replay);
            emit_json(body, &authored);
        } else {
            println!(
                "Scheduled for {}. Draft ID: {}",
                draft.send_after.as_deref().unwrap_or(&send_at),
                draft.id
            );
            print_attachment_summary(&draft.attachments);
        }
        return Ok(());
    }

    // ── Default actual-send cooldown (outbox queueing) ──
    //
    // An allowed send does NOT transmit immediately. By default it queues into
    // the existing scheduled-send / outbox mechanism with a cooldown, and real
    // SMTP only happens later when the scheduled-send sweep finds it due (and
    // only after the Governor gate permits it). Immediate transmission is an
    // explicit, confirmed emergency bypass.
    let cooldown = resolve_cooldown_seconds(cooldown_seconds);
    match resolve_disposition(cooldown, send_now, confirm_send_now) {
        SendDisposition::NeedsConfirmation => {
            let denial = serde_json::json!({
                "code": IMMEDIATE_SEND_CONFIRM_CODE,
                "reason": "immediate send bypasses the outbox cooldown; pass --send-now together with --confirm-send-now",
            });
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "denied",
                        "error": denial,
                        "ui": ui::account_ui(&creds.account.id),
                    })
                );
            }
            anyhow::bail!(
                "immediate send requires confirmation: pass --send-now together with --confirm-send-now"
            );
        }
        SendDisposition::Queue {
            cooldown_seconds: cd,
        } => {
            let send_at = (chrono::Utc::now() + chrono::Duration::seconds(cd))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string();
            let (draft, replay) = match queue_or_report(
                &db,
                &creds.account.id,
                &request,
                &send_at,
                Some(cd),
                json,
                &authored,
            )? {
                Some(queued) => queued,
                None => return Ok(()),
            };
            let send_after = draft.send_after.clone().unwrap_or(send_at);
            if json {
                let mut body = crate::commands::contract::send_body::cli_queued(
                    serde_json::json!(mode),
                    &draft.id,
                    &send_after,
                    cd,
                    OUTBOX_COOLDOWN_REASON_CODE,
                    OUTBOX_COOLDOWN_REASON,
                    serde_json::json!(attachment_summaries(&draft.attachments)),
                    serde_json::json!(queued_attribution),
                    ui::draft_ui(&creds.account.id, &draft.id),
                );
                body["idempotent_replay"] = serde_json::json!(replay);
                emit_json(body, &authored);
            } else {
                if replay {
                    println!("Already queued by an earlier identical request.");
                }
                println!(
                    "Queued for send after {cd}s cooldown (at {send_after}). Draft ID: {}",
                    draft.id
                );
                println!("Reason: {OUTBOX_COOLDOWN_REASON}");
                if GovernorConfig::smtp().mode == GovernorMode::Off {
                    println!(
                        "Real send happens via the scheduled-send sweep. Governor gate: not built in (sends are not scored)."
                    );
                } else {
                    println!(
                        "Real send happens via the scheduled-send sweep, after the Governor gate."
                    );
                }
            }
            return Ok(());
        }
        SendDisposition::Immediate => {
            // Explicit confirmed bypass — fall through to immediate send, but
            // only after the Governor gate permits it (inside the attempt).
        }
    }

    // ── Immediate send path (explicit confirmed bypass) ──
    let result = crate::commands::send_attempt::send_now(
        &db,
        &creds,
        &request,
        &AccountConnector::new(&creds),
    )
    .await;
    let mut body = match result {
        Ok(body) => body,
        Err(e) => {
            if let Some(refused) = e.downcast_ref::<GovernorRefused>() {
                let outcome = &refused.outcome;
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "status": outcome.status_str(),
                            "error": outcome.error_json(),
                            "ui": ui::account_ui(&creds.account.id),
                        })
                    );
                }
                anyhow::bail!("{}", outcome.reason_string());
            }
            if json && let Some(not_confirmed) = e.downcast_ref::<SendNotConfirmed>() {
                emit_json(not_confirmed.body.clone(), &authored);
            }
            return Err(e);
        }
    };
    if let Some(map) = body.as_object_mut() {
        // `send` reports the message, not the draft row that carried it.
        for key in ["sent", "imap_draft_deleted", "draft_ui"] {
            map.remove(key);
        }
    }
    let unrecorded = body.get("warnings").is_some();
    if json {
        emit_json(body.clone(), &authored);
    } else {
        print_sent(&body);
    }
    if unrecorded {
        anyhow::bail!(
            "the server accepted the message (Message-ID {}) but Envelope could not record it \
             as sent; it will never be re-sent automatically",
            body["message_id"].as_str().unwrap_or("unknown")
        );
    }
    Ok(())
}

/// Queue `request` (or find it already queued). `Ok(None)` means an earlier
/// identical request was already sent and its record has been printed.
fn queue_or_report(
    db: &Database,
    account_id: &str,
    request: &SendRequest<'_>,
    send_after: &str,
    cooldown_seconds: Option<i64>,
    json: bool,
    authored: &AuthoredBody,
) -> Result<Option<(envelope_email_store::Draft, bool)>> {
    match queue_request(db, account_id, request, send_after, cooldown_seconds) {
        Ok(Queued::Queued { draft, replay }) => Ok(Some((*draft, replay))),
        Ok(Queued::Sent(body)) => {
            if json {
                emit_json(body, authored);
            } else {
                print_sent(&body);
            }
            Ok(None)
        }
        Err(e) => {
            if json && let Some(not_confirmed) = e.downcast_ref::<SendNotConfirmed>() {
                emit_json(not_confirmed.body.clone(), authored);
            }
            Err(e)
        }
    }
}

fn print_sent(body: &serde_json::Value) {
    if body["idempotent_replay"] == true {
        println!("Already sent by an earlier identical request.");
    }
    println!("Sent to {}", body["to"].as_str().unwrap_or(""));
    println!("Subject: {}", body["subject"].as_str().unwrap_or(""));
    println!("Message-ID: {}", body["message_id"].as_str().unwrap_or(""));
    match (body["sent_folder"].as_str(), body["sent_uid"].as_u64()) {
        (Some(folder), Some(uid)) => {
            println!("Sent UID: {uid} ({folder})");
            if let Some(url) = body["sent_message_url"].as_str() {
                println!("Sent URL: {url}");
            }
        }
        (Some(folder), None) => println!(
            "Sent UID: unavailable in {folder} ({})",
            body["sent_mail"]["lookup_status"]
                .as_str()
                .unwrap_or("unknown")
        ),
        (None, _) => println!(
            "Sent UID: unavailable ({})",
            body["sent_mail"]["lookup_status"]
                .as_str()
                .unwrap_or("unknown")
        ),
    }
    print_attachment_summary(
        body["attachments"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]),
    );
}

fn print_attachment_summary(attachments: &[serde_json::Value]) {
    let summary = attachment_summaries(attachments);
    if summary.is_empty() {
        return;
    }
    println!("Attachments: {}", summary.len());
    for a in &summary {
        println!(
            "  - {} ({} bytes, {})",
            a["filename"].as_str().unwrap_or("attachment"),
            a["size"].as_u64().unwrap_or(0),
            a["content_type"]
                .as_str()
                .unwrap_or("application/octet-stream"),
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::commands::drafts::{
        SentMailProof, provider_auto_saves_sent, sent_copy_convenience_objects,
    };

    // Regression: CLI immediate send must resolve the Sent copy through
    // resolve_sent_copy_after_send (pre-lookup before append), which the shared
    // attempt core calls, not the old append helper.
    #[test]
    fn cli_send_no_longer_calls_append_helper_directly() {
        let send = include_str!("send.rs");
        let core = include_str!("send_attempt.rs");
        let old_helper = concat!("append_sent_copy_for_immediate_", "send");
        assert!(
            !send.contains(old_helper) && !core.contains(old_helper),
            "CLI immediate send must go through resolve_sent_copy_after_send so pre-append lookup runs first"
        );
        assert!(send.contains("send_now("));
        assert!(core.contains("resolve_sent_copy_after_send("));
    }

    #[test]
    fn cli_send_json_output_shape_includes_sent_copy_source_fields() {
        // Simulate the proof that resolve_sent_copy_after_send would return for a
        // provider-auto-save path (e.g. Gmail). provider_sent_copy should be Some,
        // client_appended_copy should be None. Uses the shared projection the CLI
        // send path actually calls, so the two can never drift.
        let mut proof = SentMailProof::new(Some("Sent Mail".to_string()), Some(42), "found", None);
        proof.copy_source = "provider";

        let (provider_sent_copy, client_appended_copy) =
            sent_copy_convenience_objects("acct@example.com", &proof);

        assert!(
            provider_sent_copy.is_some(),
            "provider path: provider_sent_copy must be Some"
        );
        assert!(
            client_appended_copy.is_none(),
            "provider path: client_appended_copy must be None"
        );
        assert_eq!(
            provider_sent_copy.as_ref().unwrap()["copy_source"],
            "provider"
        );
    }

    #[test]
    fn cli_send_client_appended_path_populates_client_appended_copy() {
        let mut proof = SentMailProof::new(Some("Sent".to_string()), Some(99), "found", None);
        proof.copy_source = "client_appended";

        let (provider_sent_copy, client_appended_copy) =
            sent_copy_convenience_objects("acct@example.com", &proof);

        assert!(
            provider_sent_copy.is_none(),
            "client_appended path: provider_sent_copy must be None"
        );
        assert!(
            client_appended_copy.is_some(),
            "client_appended path: client_appended_copy must be Some"
        );
        assert_eq!(
            client_appended_copy.as_ref().unwrap()["copy_source"],
            "client_appended"
        );
    }

    #[test]
    fn cli_send_unresolved_never_reports_provider_sent_copy() {
        // Blocker regression: a generic-provider APPEND failure resolves as
        // `unresolved`; the CLI send output must not present it as provider proof.
        let mut proof = SentMailProof::new(Some("Sent".to_string()), None, "not_found", None);
        proof.copy_source = "unresolved";

        let (provider_sent_copy, client_appended_copy) =
            sent_copy_convenience_objects("acct@example.com", &proof);

        assert!(
            provider_sent_copy.is_none(),
            "unresolved must never be presented as provider_sent_copy"
        );
        assert!(client_appended_copy.is_none());
    }

    #[test]
    fn provider_auto_saves_sent_is_accessible_from_send_module() {
        // Verify the send module can access provider detection (used by
        // resolve_sent_copy_after_send for pre-lookup routing).
        assert!(provider_auto_saves_sent(Some("gmail"), "smtp.gmail.com"));
        assert!(!provider_auto_saves_sent(None, "smtp.migadu.com"));
    }

    #[test]
    fn every_new_draft_send_path_persists_explicit_from_identity() {
        let src = include_str!("send.rs");
        let obsolete_rejection = concat!("scheduled send does not persist sender ", "override yet");
        assert!(
            !src.contains(obsolete_rejection),
            "scheduled sends must accept a validated --from override"
        );
        // The draft-only downgrade persists From on its plain draft; every
        // other path records it in the send intent the rows are created from.
        let persistence_call = concat!("persist_from_override", "(&db, &draft.id, from)?;");
        assert_eq!(src.matches(persistence_call).count(), 1);
        assert!(src.contains("        from,\n        in_reply_to: None,"));
    }
}

fn record_send_policy_event(
    db: &Database,
    account_id: &str,
    mode: SendMode,
    decision: &SendPolicyDecision,
    input: &SendPolicyInput<'_>,
) -> Result<()> {
    let audit = audit_event_for(mode, decision, input);
    let event = Event {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: account_id.to_string(),
        event_type: audit.event.to_string(),
        folder: "policy".to_string(),
        uid: None,
        message_id: None,
        from_addr: None,
        subject: None,
        snippet: None,
        payload: Some(audit.payload.to_string()),
        idempotency_key: None,
        secure_pending: false,
        acked_at: Some(chrono::Utc::now().to_rfc3339()),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    db.insert_event(&event)
        .context("audit_unavailable: could not record the send-policy decision; nothing was sent")
}
