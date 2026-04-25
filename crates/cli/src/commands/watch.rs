// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::time::Duration;

use anyhow::{Context, Result};
use async_imap::extensions::idle::IdleResponse;
use envelope_email_store::CredentialBackend;
use envelope_email_store::models::Event;
use envelope_email_transport::code_extractor::{
    OtpPatternId, extract_code_with_pattern, parse_expiry_hint, redact_codes,
};
use futures_util::StreamExt;
use tracing::{info, warn};

use super::common::setup_credentials;

#[tokio::main]
pub async fn run(
    folder: &str,
    account: Option<&str>,
    webhook: Option<&str>,
    _run_rules: bool,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;
    let account_id = creds.account.id.clone();

    let http_client = webhook.map(|_| reqwest::Client::new());

    if !json {
        eprintln!(
            "Watching {} on {}... (Ctrl-C to stop)",
            folder, creds.account.username
        );
    }

    // Graceful shutdown via Ctrl-C
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    let mut session = envelope_email_transport::idle::connect_session(&creds)
        .await
        .context("IMAP connection failed")?;

    let selected_mailbox = session
        .select(folder)
        .await
        .map_err(|e| anyhow::anyhow!("SELECT {folder}: {e}"))?;
    let mut current_uid_validity = selected_mailbox.uid_validity;

    // Track highest UID we've seen so we only fetch genuinely new messages
    let mut last_uid: u32 = highest_uid(&mut session, folder).await.unwrap_or(0);

    loop {
        // Enter IDLE
        let mut handle = session.idle();
        handle
            .init()
            .await
            .map_err(|e| anyhow::anyhow!("IDLE init: {e}"))?;

        let (idle_fut, _interrupt) = handle.wait_with_timeout(Duration::from_secs(25 * 60));

        let response = idle_fut
            .await
            .map_err(|e| anyhow::anyhow!("IDLE wait: {e}"))?;

        match response {
            IdleResponse::NewData(_data) => {
                // End IDLE to regain session ownership
                session = handle
                    .done()
                    .await
                    .map_err(|e| anyhow::anyhow!("IDLE done: {e}"))?;

                // Re-SELECT to refresh EXISTS and UIDVALIDITY
                let selected_mailbox = session
                    .select(folder)
                    .await
                    .map_err(|e| anyhow::anyhow!("SELECT {folder}: {e}"))?;
                let uid_validity = selected_mailbox.uid_validity;
                if uid_validity_changed(current_uid_validity, uid_validity) {
                    warn!(
                        "mailbox UIDVALIDITY changed for {folder}; resetting watch UID watermark"
                    );
                    current_uid_validity = uid_validity;
                    last_uid = 0;
                }

                // Fetch messages newer than our watermark
                let new_msgs = fetch_new_messages(&mut session, last_uid).await?;

                for msg in &new_msgs {
                    let uid = msg.uid;
                    if uid > last_uid {
                        last_uid = uid;
                    }

                    let created_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
                    let event = redacted_watch_event(
                        &account_id,
                        folder,
                        msg,
                        uid_validity,
                        "new_message",
                        None,
                        false,
                        created_at.clone(),
                    );

                    match db.insert_event_idempotent(&event) {
                        Ok(true) => emit_event(&event, webhook, http_client.as_ref()),
                        Ok(false) => continue,
                        Err(e) => warn!("failed to persist event: {e}"),
                    }

                    let scan_text = format!(
                        "{}\n{}",
                        msg.subject.as_deref().unwrap_or_default(),
                        msg.snippet.as_deref().unwrap_or_default()
                    );
                    if let Some((code, pattern)) = extract_code_with_pattern(&scan_text, None) {
                        let confidence = confidence_for_pattern(pattern);
                        if confidence >= 0.5 {
                            let payload = redacted_otp_payload(&scan_text, &code, pattern);
                            let otp_event = redacted_watch_event(
                                &account_id,
                                folder,
                                msg,
                                uid_validity,
                                "otp_detected",
                                Some(payload.to_string()),
                                true,
                                created_at,
                            );
                            match db.insert_event_idempotent(&otp_event) {
                                Ok(true) => emit_event(&otp_event, webhook, http_client.as_ref()),
                                Ok(false) => {}
                                Err(e) => warn!("failed to persist OTP event: {e}"),
                            }
                        }
                    }
                }

                info!("processed {} new message(s)", new_msgs.len());
            }
            IdleResponse::Timeout => {
                // Re-IDLE after timeout (keeps connection alive)
                session = handle
                    .done()
                    .await
                    .map_err(|e| anyhow::anyhow!("IDLE done after timeout: {e}"))?;

                // Re-SELECT to keep the mailbox session alive and catch UIDVALIDITY resets.
                let selected_mailbox = session
                    .select(folder)
                    .await
                    .map_err(|e| anyhow::anyhow!("SELECT {folder}: {e}"))?;
                if uid_validity_changed(current_uid_validity, selected_mailbox.uid_validity) {
                    warn!(
                        "mailbox UIDVALIDITY changed for {folder}; resetting watch UID watermark"
                    );
                    current_uid_validity = selected_mailbox.uid_validity;
                    last_uid = 0;
                }
            }
            IdleResponse::ManualInterrupt => {
                let _ = handle.done().await;
                break;
            }
        }

        // Check if Ctrl-C was pressed
        if futures_util::FutureExt::now_or_never(&mut shutdown).is_some() {
            if !json {
                eprintln!("Shutting down...");
            }
            break;
        }
    }

    Ok(())
}

/// A minimal representation of a newly fetched message.
struct NewMessage {
    uid: u32,
    message_id: Option<String>,
    from_addr: Option<String>,
    subject: Option<String>,
    snippet: Option<String>,
}

fn redacted_watch_event(
    account_id: &str,
    folder: &str,
    msg: &NewMessage,
    uid_validity: Option<u32>,
    event_type: &str,
    payload: Option<String>,
    secure_pending: bool,
    created_at: String,
) -> Event {
    Event {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: account_id.to_string(),
        event_type: event_type.to_string(),
        folder: folder.to_string(),
        uid: Some(i64::from(msg.uid)),
        message_id: msg.message_id.clone(),
        from_addr: msg.from_addr.clone(),
        subject: msg.subject.as_deref().map(redact_codes),
        snippet: msg.snippet.as_deref().map(redact_codes),
        payload,
        idempotency_key: Some(idempotency_key(
            account_id,
            folder,
            uid_validity,
            msg,
            event_type,
        )),
        secure_pending,
        acked_at: None,
        created_at,
    }
}

fn redacted_otp_payload(scan_text: &str, code: &str, pattern: OtpPatternId) -> serde_json::Value {
    serde_json::json!({
        "code_length": code.len(),
        "confidence": confidence_for_pattern(pattern),
        "source_pattern": pattern,
        "expires_hint_secs": parse_expiry_hint(scan_text),
    })
}

fn idempotency_key(
    account_id: &str,
    folder: &str,
    uid_validity: Option<u32>,
    msg: &NewMessage,
    event_type: &str,
) -> String {
    let uid_validity = uid_validity
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unavailable".to_string());
    let message_marker = msg
        .message_id
        .as_deref()
        .map(stable_hash)
        .unwrap_or_else(|| "no-message-id".to_string());
    format!(
        "{account_id}:{folder}:uidvalidity-{uid_validity}:uid-{}:msg-{message_marker}:{event_type}",
        msg.uid
    )
}

fn stable_hash(input: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn uid_validity_changed(current: Option<u32>, next: Option<u32>) -> bool {
    matches!((current, next), (Some(current), Some(next)) if current != next)
}

fn confidence_for_pattern(pattern: OtpPatternId) -> f32 {
    match pattern {
        OtpPatternId::ExplicitLabel => 0.95,
        OtpPatternId::OtpStyle => 0.9,
        OtpPatternId::HtmlProminent => 0.7,
        OtpPatternId::Fallback => 0.4,
    }
}

fn emit_event(event: &Event, webhook: Option<&str>, http_client: Option<&reqwest::Client>) {
    let json_line = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_string());
    println!("{json_line}");

    if let (Some(url), Some(client)) = (webhook, http_client) {
        let url = url.to_string();
        let client = client.clone();
        let body = json_line;
        tokio::spawn(async move {
            if let Err(e) = client
                .post(&url)
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
            {
                warn!("webhook POST failed: {e}");
            }
        });
    }
}

fn snippet_preview(bytes: &[u8], max_chars: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut chars = text.chars();
    let preview = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

/// Return the highest UID currently in the selected folder.
async fn highest_uid(
    session: &mut envelope_email_transport::imap::ImapSession,
    _folder: &str,
) -> Result<u32> {
    // SEARCH for all messages to find max UID
    let uids = session
        .uid_search("ALL")
        .await
        .map_err(|e| anyhow::anyhow!("UID SEARCH ALL: {e}"))?;
    Ok(uids.into_iter().max().unwrap_or(0))
}

/// Fetch messages with UID > last_uid from the already-selected folder.
async fn fetch_new_messages(
    session: &mut envelope_email_transport::imap::ImapSession,
    last_uid: u32,
) -> Result<Vec<NewMessage>> {
    let start = last_uid + 1;
    let range = format!("{start}:*");

    let fetches = session
        .uid_fetch(&range, "(UID ENVELOPE BODY.PEEK[TEXT]<0.200>)")
        .await
        .map_err(|e| anyhow::anyhow!("UID FETCH {range}: {e}"))?;

    let mut messages = Vec::new();
    let mut stream = fetches;
    while let Some(item) = stream.next().await {
        match item {
            Ok(fetch) => {
                let uid = fetch.uid.unwrap_or(0);
                if uid <= last_uid {
                    // UID FETCH N:* always returns at least UID N even if
                    // there are no new messages.
                    continue;
                }

                let (message_id, from_addr, subject) = if let Some(env) = fetch.envelope() {
                    let mid = env
                        .message_id
                        .as_ref()
                        .map(|m| String::from_utf8_lossy(m).to_string());
                    let from = env.from.as_ref().and_then(|addrs| {
                        addrs.first().map(|a| {
                            let mailbox = a
                                .mailbox
                                .as_ref()
                                .map(|m| String::from_utf8_lossy(m).to_string())
                                .unwrap_or_default();
                            let host = a
                                .host
                                .as_ref()
                                .map(|h| String::from_utf8_lossy(h).to_string())
                                .unwrap_or_default();
                            format!("{mailbox}@{host}")
                        })
                    });
                    let subj = env
                        .subject
                        .as_ref()
                        .map(|s| String::from_utf8_lossy(s).to_string());
                    (mid, from, subj)
                } else {
                    (None, None, None)
                };

                let snippet = fetch.text().map(|t| snippet_preview(t, 150));

                messages.push(NewMessage {
                    uid,
                    message_id,
                    from_addr,
                    subject,
                    snippet,
                });
            }
            Err(e) => {
                warn!("FETCH parse error (skipping): {e}");
            }
        }
    }

    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_message() -> NewMessage {
        NewMessage {
            uid: 42,
            message_id: Some("<fixture@example.com>".to_string()),
            from_addr: Some("noreply@example.com".to_string()),
            subject: Some("Your verification code is 482910".to_string()),
            snippet: Some("Use code 482910 to finish signing in.".to_string()),
        }
    }

    #[test]
    fn redacted_watch_event_serialization_omits_fixture_code() {
        let event = redacted_watch_event(
            "acc-1",
            "INBOX",
            &fixture_message(),
            Some(777),
            "new_message",
            None,
            false,
            "2026-04-25T12:00:00".to_string(),
        );

        let serialized = serde_json::to_string(&event).unwrap();
        assert!(!serialized.contains("482910"));
        assert_eq!(
            event.subject.as_deref(),
            Some("Your verification code is ***")
        );
        assert_eq!(
            event.snippet.as_deref(),
            Some("Use code *** to finish signing in.")
        );
    }

    #[test]
    fn otp_payload_exposes_metadata_without_secret() {
        let payload = redacted_otp_payload(
            "Your OTP code is 482910. Valid for 30 seconds.",
            "482910",
            OtpPatternId::OtpStyle,
        );

        let serialized = payload.to_string();
        assert!(!serialized.contains("482910"));
        assert_eq!(payload.get("code_length").and_then(|v| v.as_u64()), Some(6));
        let confidence = payload.get("confidence").and_then(|v| v.as_f64()).unwrap();
        assert!((confidence - 0.9).abs() < 1e-6);
        assert_eq!(
            payload.get("source_pattern").and_then(|v| v.as_str()),
            Some("otp_style")
        );
        assert_eq!(
            payload.get("expires_hint_secs").and_then(|v| v.as_u64()),
            Some(30)
        );
    }

    #[test]
    fn idempotency_key_is_stable_kind_specific_and_uidvalidity_scoped() {
        let msg = fixture_message();
        let first = idempotency_key("acc-1", "INBOX", Some(99), &msg, "new_message");
        let second = idempotency_key("acc-1", "INBOX", Some(99), &msg, "new_message");
        let different_kind = idempotency_key("acc-1", "INBOX", Some(99), &msg, "otp_detected");
        let different_uidvalidity =
            idempotency_key("acc-1", "INBOX", Some(100), &msg, "new_message");

        assert_eq!(first, second);
        assert_ne!(first, different_kind);
        assert_ne!(first, different_uidvalidity);
        assert!(first.contains("uidvalidity-99"));
        assert!(!first.contains("fixture@example.com"));
    }

    #[test]
    fn snippet_preview_truncates_on_utf8_char_boundaries() {
        let body = "é".repeat(151);
        let preview = snippet_preview(body.as_bytes(), 150);
        assert_eq!(preview, format!("{}...", "é".repeat(150)));
    }

    #[test]
    fn uid_validity_change_detects_real_resets_only() {
        assert!(uid_validity_changed(Some(10), Some(11)));
        assert!(!uid_validity_changed(Some(10), Some(10)));
        assert!(!uid_validity_changed(None, Some(10)));
        assert!(!uid_validity_changed(Some(10), None));
    }
}
