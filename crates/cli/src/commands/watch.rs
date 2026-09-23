// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use anyhow::{Context, Result};
use async_imap::extensions::idle::IdleResponse;
use envelope_email_store::CredentialBackend;
use envelope_email_store::flag_transitions::{ObservedFlags, SEEN_SOURCE_WATCH};
use envelope_email_store::models::{Event, EventRoute};
use envelope_email_transport::code_extractor::{
    OtpPatternId, extract_code_with_pattern, parse_expiry_hint, redact_codes,
};
use envelope_email_transport::event_delivery::{DeliveryLimits, deliver_due_events};
use envelope_email_transport::http::{Allowance, client_for};
use envelope_email_transport::rule_exec::{
    self, ActionAttribution, ActionSource, ImapRuleMailbox, RuleMailbox, RuleRunReport, RunAccount,
};
use futures_util::StreamExt;
use tracing::{info, warn};

use super::common::setup_credentials;
use super::provenance;

#[tokio::main]
pub async fn run(
    folder: &str,
    account: Option<&str>,
    webhook: Option<&str>,
    run_rules: bool,
    deliver: bool,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;
    let account_id = creds.account.id.clone();

    // Refuse a private or unresolvable --webhook up front. Each POST re-checks
    // the host, since DNS can change over a long-running watch.
    if let Some(url) = webhook {
        client_for(url, &Allowance::Public)
            .await
            .with_context(|| format!("--webhook {url} refused"))?;
    }

    if !json {
        eprintln!(
            "Watching {} on {}... (Ctrl-C to stop)",
            folder, creds.account.username
        );
    }

    // Rule actions run on their own connection so the IDLE session keeps its
    // SELECTed state. Log in once up front so bad credentials fail loudly;
    // each batch then opens a fresh connection, because an idle side
    // connection is dropped by the server long before the next new mail.
    if run_rules {
        envelope_email_transport::imap::connect(&creds)
            .await
            .context("IMAP connection for --run-rules failed")?;
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

    let condstore = match session.capabilities().await {
        Ok(caps) => caps.has_str("CONDSTORE"),
        Err(e) => {
            warn!("CAPABILITY failed ({e}); tracking flags without CONDSTORE");
            false
        }
    };
    let mut flag_watch = FlagWatch::new(condstore);
    sync_flags(
        &mut flag_watch,
        &mut session,
        &db,
        &account_id,
        folder,
        current_uid_validity,
        selected_mailbox.exists,
    )
    .await;

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
                        Ok(true) => {
                            emit_event(&event, webhook);
                            if deliver {
                                enqueue_deliveries_for_event(&db, &event);
                            }
                        }
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
                                Ok(true) => {
                                    emit_event(&otp_event, webhook);
                                    if deliver {
                                        enqueue_deliveries_for_event(&db, &otp_event);
                                    }
                                }
                                Ok(false) => {}
                                Err(e) => warn!("failed to persist OTP event: {e}"),
                            }
                        }
                    }
                }

                if run_rules && !new_msgs.is_empty() {
                    match envelope_email_transport::imap::connect(&creds).await {
                        Ok(mut client) => {
                            let mut mbox = ImapRuleMailbox {
                                client: &mut client,
                                db: &db,
                                account_id: &account_id,
                            };
                            let account = RunAccount {
                                id: &account_id,
                                email: &creds.account.username,
                            };
                            match run_rules_on_new_messages(
                                &mut mbox, &db, &account, folder, &new_msgs,
                            )
                            .await
                            {
                                Ok(report) => log_rule_report(&report),
                                Err(e) => warn!("--run-rules failed for this batch: {e:#}"),
                            }
                        }
                        Err(e) => warn!("--run-rules could not connect for this batch: {e}"),
                    }
                }

                // Opportunistically drain any due deliveries after this batch.
                if deliver {
                    match deliver_due_events(
                        &db,
                        &Allowance::Public,
                        chrono::Utc::now(),
                        DeliveryLimits::default(),
                    )
                    .await
                    {
                        Ok(report) => {
                            if report.examined > 0 {
                                info!(
                                    "deliveries: {} delivered, {} retried, {} dead-lettered",
                                    report.delivered, report.retried, report.dead_lettered
                                );
                            }
                        }
                        Err(e) => warn!("delivery executor error: {e}"),
                    }
                }

                info!("processed {} new message(s)", new_msgs.len());

                sync_flags(
                    &mut flag_watch,
                    &mut session,
                    &db,
                    &account_id,
                    folder,
                    uid_validity,
                    selected_mailbox.exists,
                )
                .await;
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
                sync_flags(
                    &mut flag_watch,
                    &mut session,
                    &db,
                    &account_id,
                    folder,
                    selected_mailbox.uid_validity,
                    selected_mailbox.exists,
                )
                .await;
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

/// How many of the newest messages `watch` tracks for `\Seen` transitions.
const FLAG_WATCH_WINDOW: u32 = 200;

/// One bounded FLAGS fetch. Only FLAGS (plus UID and MODSEQ) are requested,
/// never a body, so the fetch itself cannot set `\Seen`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FlagFetch {
    /// First pass after SELECT: the newest [`FLAG_WATCH_WINDOW`] messages by
    /// sequence number (`exists` is the SELECT's EXISTS count).
    Seed { exists: u32 },
    /// After an IDLE wake: every UID from the oldest tracked one up, narrowed
    /// to changed messages when the server supports CONDSTORE.
    Since {
        from_uid: u32,
        changed_since: Option<u64>,
    },
}

impl FlagFetch {
    /// `(uid_command, set, query)` for this fetch.
    fn command(&self, condstore: bool) -> (bool, String, String) {
        let items = if condstore {
            "(UID FLAGS MODSEQ)"
        } else {
            "(UID FLAGS)"
        };
        match self {
            FlagFetch::Seed { exists } => {
                let start = exists.saturating_sub(FLAG_WATCH_WINDOW - 1).max(1);
                (false, format!("{start}:{exists}"), items.to_string())
            }
            FlagFetch::Since {
                from_uid,
                changed_since,
            } => {
                let query = match (condstore, changed_since) {
                    (true, Some(modseq)) => format!("{items} (CHANGEDSINCE {modseq})"),
                    _ => items.to_string(),
                };
                (true, format!("{}:*", from_uid.max(&1)), query)
            }
        }
    }
}

#[derive(Debug, Clone)]
struct FetchedFlags {
    uid: u32,
    flags: Vec<String>,
    modseq: Option<u64>,
}

/// Where FLAGS come from: the live IMAP session, or a fake in tests.
trait FlagSource {
    async fn fetch_flags(
        &mut self,
        fetch: &FlagFetch,
        condstore: bool,
    ) -> Result<Vec<FetchedFlags>>;
}

impl FlagSource for envelope_email_transport::imap::ImapSession {
    async fn fetch_flags(
        &mut self,
        fetch: &FlagFetch,
        condstore: bool,
    ) -> Result<Vec<FetchedFlags>> {
        let (uid_command, set, query) = fetch.command(condstore);
        let context = format!(
            "{}FETCH {set} {query}",
            if uid_command { "UID " } else { "" }
        );
        if uid_command {
            let stream = self
                .uid_fetch(&set, &query)
                .await
                .map_err(|e| anyhow::anyhow!("{context}: {e}"))?;
            collect_fetched_flags(stream, &context).await
        } else {
            let stream = self
                .fetch(&set, &query)
                .await
                .map_err(|e| anyhow::anyhow!("{context}: {e}"))?;
            collect_fetched_flags(stream, &context).await
        }
    }
}

async fn collect_fetched_flags(
    stream: impl futures_util::Stream<Item = async_imap::error::Result<async_imap::types::Fetch>>,
    context: &str,
) -> Result<Vec<FetchedFlags>> {
    let mut stream = std::pin::pin!(stream);
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let fetch = item.map_err(|e| anyhow::anyhow!("{context}: {e}"))?;
        let Some(uid) = fetch.uid else { continue };
        out.push(FetchedFlags {
            uid,
            flags: fetch.flags().map(|f| format!("{f:?}")).collect(),
            modseq: fetch.modseq,
        });
    }
    Ok(out)
}

/// Per-folder `uid -> flags` memory for `watch`, so a server with no dashboard
/// index still yields `message_seen` events when another client reads mail.
struct FlagWatch {
    condstore: bool,
    uidvalidity: Option<u32>,
    seeded: bool,
    known: BTreeMap<u32, Vec<String>>,
    highest_modseq: Option<u64>,
}

impl FlagWatch {
    fn new(condstore: bool) -> Self {
        Self {
            condstore,
            uidvalidity: None,
            seeded: false,
            known: BTreeMap::new(),
            highest_modseq: None,
        }
    }

    fn next_fetch(&self, exists: u32) -> FlagFetch {
        if !self.seeded {
            return FlagFetch::Seed { exists };
        }
        FlagFetch::Since {
            from_uid: self.known.keys().next().copied().unwrap_or(1),
            changed_since: self.highest_modseq,
        }
    }

    /// Fetch FLAGS, persist them through the message index, and emit
    /// `message_seen` for tracked UIDs that gained `\Seen`. The seeding pass
    /// emits only what the index itself can prove (a row it held unseen).
    /// Returns the number of new events.
    async fn sync<S: FlagSource>(
        &mut self,
        source: &mut S,
        db: &envelope_email_store::Database,
        account_id: &str,
        folder: &str,
        uidvalidity: Option<u32>,
        exists: u32,
    ) -> Result<usize> {
        let uidvalidity = uidvalidity.context(
            "server reported no UIDVALIDITY; cannot key message_seen events for this folder",
        )?;
        if self.uidvalidity != Some(uidvalidity) {
            *self = Self::new(self.condstore);
            self.uidvalidity = Some(uidvalidity);
        }
        if !self.seeded && exists == 0 {
            self.seeded = true;
            return Ok(0);
        }

        let fetch = self.next_fetch(exists);
        let rows = source.fetch_flags(&fetch, self.condstore).await?;

        let prior: HashMap<u32, Vec<String>> = if self.seeded {
            self.known
                .iter()
                .map(|(uid, flags)| (*uid, flags.clone()))
                .collect()
        } else {
            HashMap::new()
        };
        let observed: Vec<ObservedFlags> = rows
            .iter()
            .map(|row| ObservedFlags {
                uid: row.uid,
                message_id: None,
                flags: row.flags.clone(),
            })
            .collect();
        let emitted = db
            .record_observed_flags(
                account_id,
                folder,
                u64::from(uidvalidity),
                &prior,
                &observed,
                SEEN_SOURCE_WATCH,
            )
            .context("failed to record observed flags")?;

        for row in rows {
            if let Some(modseq) = row.modseq {
                self.highest_modseq = Some(self.highest_modseq.map_or(modseq, |m| m.max(modseq)));
            }
            self.known.insert(row.uid, row.flags);
        }
        while self.known.len() > FLAG_WATCH_WINDOW as usize {
            self.known.pop_first();
        }
        self.seeded = true;
        Ok(emitted)
    }
}

/// Run one flag sync; a failure is logged and the watch carries on, since
/// new-mail notification must not stop over read tracking.
async fn sync_flags(
    flag_watch: &mut FlagWatch,
    session: &mut envelope_email_transport::imap::ImapSession,
    db: &envelope_email_store::Database,
    account_id: &str,
    folder: &str,
    uidvalidity: Option<u32>,
    exists: u32,
) {
    match flag_watch
        .sync(session, db, account_id, folder, uidvalidity, exists)
        .await
    {
        Ok(0) => {}
        Ok(n) => info!("observed {n} message(s) read on another client in {folder}"),
        Err(e) => warn!("flag sync for {folder} failed: {e:#}"),
    }
}

/// A minimal representation of a newly fetched message.
struct NewMessage {
    uid: u32,
    message_id: Option<String>,
    from_addr: Option<String>,
    to_addr: Option<String>,
    subject: Option<String>,
    snippet: Option<String>,
}

/// Run enabled rules over one batch of new messages through the unified
/// executor, attributed as `source: rule`.
async fn run_rules_on_new_messages<M: RuleMailbox>(
    mbox: &mut M,
    db: &envelope_email_store::Database,
    account: &RunAccount<'_>,
    folder: &str,
    msgs: &[NewMessage],
) -> Result<RuleRunReport> {
    let summaries: Vec<envelope_email_store::MessageSummary> = msgs
        .iter()
        .map(|m| envelope_email_store::MessageSummary {
            uid: m.uid,
            message_id: m.message_id.clone(),
            from_addr: m.from_addr.clone().unwrap_or_default(),
            to_addr: m.to_addr.clone().unwrap_or_default(),
            subject: m.subject.clone().unwrap_or_default(),
            date: None,
            flags: vec![],
            size: 0,
            provider_spam: None,
        })
        .collect();
    rule_exec::apply_rules_to_summaries(
        mbox,
        db,
        account,
        folder,
        &summaries,
        &ActionAttribution::new(ActionSource::Rule),
    )
    .await
}

fn log_rule_report(report: &RuleRunReport) {
    for skipped in &report.skipped_rules {
        warn!("rule '{}' skipped: {}", skipped.rule_name, skipped.reason);
    }
    for entry in &report.log {
        if entry["status"] == "error" {
            warn!("rule run: {entry}");
        } else {
            info!("rule run: {entry}");
        }
    }
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

/// Enqueue a pending delivery for every enabled route (in this event's account)
/// whose match expression accepts the event type. Deterministic delivery ids
/// keep enqueue idempotent so re-processing the same event never double-sends.
fn enqueue_deliveries_for_event(db: &envelope_email_store::Database, event: &Event) {
    let routes = match db.list_enabled_event_routes(Some(&event.account_id)) {
        Ok(routes) => routes,
        Err(e) => {
            warn!("failed to load event routes: {e}");
            return;
        }
    };
    let now = chrono::Utc::now().to_rfc3339();
    for route in &routes {
        if !route_matches(route, &event.event_type) {
            continue;
        }
        let delivery_row_id = format!("{}:{}", event.id, route.id);
        let delivery_marker = stable_hash(&delivery_row_id);
        if let Err(e) = db.enqueue_delivery(
            &delivery_row_id,
            &event.id,
            &route.id,
            &delivery_marker,
            &now,
        ) {
            warn!("failed to enqueue delivery: {e}");
        }
    }
}

/// Does a route's `match_expr` accept this event type? The expression is JSON:
/// `{"event_types": ["new_message", ...]}` matches only the listed types; an
/// empty object / missing `event_types` matches everything.
fn route_matches(route: &EventRoute, event_type: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&route.match_expr) else {
        // Unparseable match expression fails closed (no delivery) rather than
        // spamming every route target.
        return false;
    };
    match value.get("event_types").and_then(|v| v.as_array()) {
        Some(types) => types.iter().any(|t| t.as_str() == Some(event_type)),
        None => true,
    }
}

fn emit_event(event: &Event, webhook: Option<&str>) {
    // A watch is notification, not an instruction channel. The legacy event
    // fields are retained inside the additive safe event representation.
    let json_line =
        serde_json::to_string(&provenance::event_json(event)).unwrap_or_else(|_| "{}".to_string());
    println!("{json_line}");

    if let Some(url) = webhook {
        let url = url.to_string();
        let body = json_line;
        tokio::spawn(async move {
            let (client, target) = match client_for(&url, &Allowance::Public).await {
                Ok(guarded) => guarded,
                Err(e) => {
                    warn!("webhook POST refused: {e}");
                    return;
                }
            };
            if client
                .post(target)
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
                .is_err()
            {
                warn!("webhook POST failed");
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

                let (message_id, from_addr, to_addr, subject) = if let Some(env) = fetch.envelope()
                {
                    let mid = env
                        .message_id
                        .as_ref()
                        .map(|m| String::from_utf8_lossy(m).to_string());
                    let first_address =
                        |addrs: &Vec<async_imap::imap_proto::types::Address<'_>>| {
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
                        };
                    let from = env.from.as_ref().and_then(first_address);
                    let to = env.to.as_ref().and_then(first_address);
                    let subj = env
                        .subject
                        .as_ref()
                        .map(|s| String::from_utf8_lossy(s).to_string());
                    (mid, from, to, subj)
                } else {
                    (None, None, None, None)
                };

                let snippet = fetch.text().map(|t| snippet_preview(t, 150));

                messages.push(NewMessage {
                    uid,
                    message_id,
                    from_addr,
                    to_addr,
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
            to_addr: Some("me@example.com".to_string()),
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
        let agent_event = provenance::event_json(&event);
        assert_eq!(agent_event["trust"]["schema"], "envelope.inbound-trust.v1");
        assert_eq!(
            agent_event["untrusted_content"]["snippet"],
            "Use code *** to finish signing in."
        );
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

    /// Scripted FLAGS responses, one per sync, recording each fetch asked for.
    struct FakeFlagSource {
        responses: std::collections::VecDeque<Vec<FetchedFlags>>,
        fetches: Vec<(FlagFetch, bool)>,
    }

    impl FakeFlagSource {
        fn new(responses: Vec<Vec<FetchedFlags>>) -> Self {
            Self {
                responses: responses.into(),
                fetches: Vec::new(),
            }
        }
    }

    impl FlagSource for FakeFlagSource {
        async fn fetch_flags(
            &mut self,
            fetch: &FlagFetch,
            condstore: bool,
        ) -> Result<Vec<FetchedFlags>> {
            self.fetches.push((fetch.clone(), condstore));
            Ok(self.responses.pop_front().expect("unscripted fetch"))
        }
    }

    fn row(uid: u32, flags: &[&str], modseq: Option<u64>) -> FetchedFlags {
        FetchedFlags {
            uid,
            flags: flags.iter().map(|f| f.to_string()).collect(),
            modseq,
        }
    }

    fn seen_events(db: &envelope_email_store::Database) -> Vec<Event> {
        db.list_events(None, 100)
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == "message_seen")
            .collect()
    }

    #[tokio::test]
    async fn first_wake_seeds_and_second_wake_emits_one_seen() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        let mut source = FakeFlagSource::new(vec![
            vec![row(7, &[], None), row(8, &["Seen"], None)],
            vec![row(7, &["Seen"], None), row(8, &["Seen"], None)],
            vec![row(7, &["Seen"], None), row(8, &["Seen"], None)],
        ]);
        let mut watch = FlagWatch::new(false);

        let seeded = watch
            .sync(&mut source, &db, "acc", "INBOX", Some(42), 2)
            .await
            .unwrap();
        assert_eq!(seeded, 0, "seeding never emits without an index prior");
        assert!(seen_events(&db).is_empty());

        let emitted = watch
            .sync(&mut source, &db, "acc", "INBOX", Some(42), 2)
            .await
            .unwrap();
        assert_eq!(emitted, 1);
        let events = seen_events(&db);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid, Some(7));
        assert_eq!(
            events[0].idempotency_key.as_deref(),
            Some("seen:acc:INBOX:42:7")
        );
        let payload: serde_json::Value =
            serde_json::from_str(events[0].payload.as_deref().unwrap()).unwrap();
        assert_eq!(payload["source"], "watch");

        let again = watch
            .sync(&mut source, &db, "acc", "INBOX", Some(42), 2)
            .await
            .unwrap();
        assert_eq!(again, 0);
        assert_eq!(seen_events(&db).len(), 1);

        assert_eq!(source.fetches[0].0, FlagFetch::Seed { exists: 2 });
        assert_eq!(
            source.fetches[1].0,
            FlagFetch::Since {
                from_uid: 7,
                changed_since: None
            }
        );
    }

    #[tokio::test]
    async fn own_mark_seen_in_the_index_is_not_a_foreign_read() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        db.upsert_indexed_message_summaries(
            "acc",
            "INBOX",
            42,
            &[envelope_email_store::models::IndexedMessageInput {
                uid: 7,
                message_id: Some("<7@x>".into()),
                from_addr: "p@example.test".into(),
                to_addr: "me@example.test".into(),
                subject: "s".into(),
                date: None,
                flags: vec![],
                size: 1,
                snippet: None,
                thread_id: None,
            }],
        )
        .unwrap();
        let mut source =
            FakeFlagSource::new(vec![vec![row(7, &[], None)], vec![row(7, &["Seen"], None)]]);
        let mut watch = FlagWatch::new(false);
        watch
            .sync(&mut source, &db, "acc", "INBOX", Some(42), 1)
            .await
            .unwrap();
        envelope_email_transport::imap::record_own_flag_change(
            &db,
            "acc",
            "INBOX",
            &[7],
            "\\Seen",
            true,
        )
        .unwrap();
        let emitted = watch
            .sync(&mut source, &db, "acc", "INBOX", Some(42), 1)
            .await
            .unwrap();
        assert_eq!(emitted, 0);
        assert!(seen_events(&db).is_empty());
    }

    #[tokio::test]
    async fn condstore_narrows_wakes_to_changed_since_the_highest_modseq() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        let mut source = FakeFlagSource::new(vec![
            vec![row(300, &[], Some(10)), row(301, &[], Some(12))],
            vec![row(301, &["Seen"], Some(15))],
            vec![],
        ]);
        let mut watch = FlagWatch::new(true);
        for _ in 0..3 {
            watch
                .sync(&mut source, &db, "acc", "INBOX", Some(1), 500)
                .await
                .unwrap();
        }
        assert_eq!(seen_events(&db).len(), 1);
        assert_eq!(
            source.fetches[1].0,
            FlagFetch::Since {
                from_uid: 300,
                changed_since: Some(12)
            }
        );
        assert_eq!(
            source.fetches[2].0,
            FlagFetch::Since {
                from_uid: 300,
                changed_since: Some(15)
            }
        );
        assert!(source.fetches.iter().all(|(_, condstore)| *condstore));
    }

    #[tokio::test]
    async fn uidvalidity_change_reseeds_without_emitting() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        let mut source =
            FakeFlagSource::new(vec![vec![row(7, &[], None)], vec![row(7, &["Seen"], None)]]);
        let mut watch = FlagWatch::new(false);
        watch
            .sync(&mut source, &db, "acc", "INBOX", Some(1), 1)
            .await
            .unwrap();
        let emitted = watch
            .sync(&mut source, &db, "acc", "INBOX", Some(2), 1)
            .await
            .unwrap();
        assert_eq!(emitted, 0);
        assert_eq!(source.fetches[1].0, FlagFetch::Seed { exists: 1 });
    }

    #[test]
    fn flag_fetch_commands_are_bounded_and_body_free() {
        assert_eq!(
            FlagFetch::Seed { exists: 1000 }.command(false),
            (false, "801:1000".to_string(), "(UID FLAGS)".to_string())
        );
        assert_eq!(
            FlagFetch::Seed { exists: 5 }.command(true),
            (false, "1:5".to_string(), "(UID FLAGS MODSEQ)".to_string())
        );
        assert_eq!(
            FlagFetch::Since {
                from_uid: 90,
                changed_since: Some(7)
            }
            .command(true),
            (
                true,
                "90:*".to_string(),
                "(UID FLAGS MODSEQ) (CHANGEDSINCE 7)".to_string()
            )
        );
        assert_eq!(
            FlagFetch::Since {
                from_uid: 90,
                changed_since: Some(7)
            }
            .command(false),
            (true, "90:*".to_string(), "(UID FLAGS)".to_string())
        );
        for (_, _, query) in [
            FlagFetch::Seed { exists: 3 }.command(true),
            FlagFetch::Since {
                from_uid: 1,
                changed_since: None,
            }
            .command(false),
        ] {
            assert!(!query.contains("BODY"), "{query}");
            assert!(!query.contains("RFC822"), "{query}");
        }
    }

    #[test]
    fn uid_validity_change_detects_real_resets_only() {
        assert!(uid_validity_changed(Some(10), Some(11)));
        assert!(!uid_validity_changed(Some(10), Some(10)));
        assert!(!uid_validity_changed(None, Some(10)));
        assert!(!uid_validity_changed(Some(10), None));
    }

    /// Records mailbox calls; never opens a socket.
    #[derive(Default)]
    struct FakeSession {
        calls: Vec<String>,
    }

    impl RuleMailbox for FakeSession {
        async fn resolve_folder(&mut self, dest: &str) -> Result<String> {
            Ok(dest.to_string())
        }
        async fn move_message(&mut self, folder: &str, uid: u32, dest: &str) -> Result<()> {
            self.calls.push(format!("move {folder}/{uid} -> {dest}"));
            Ok(())
        }
        async fn set_flag(&mut self, folder: &str, uid: u32, flag: &str) -> Result<()> {
            self.calls.push(format!("flag {folder}/{uid} {flag}"));
            Ok(())
        }
        async fn remove_flag(&mut self, folder: &str, uid: u32, flag: &str) -> Result<()> {
            self.calls.push(format!("unflag {folder}/{uid} {flag}"));
            Ok(())
        }
        async fn delete_message(&mut self, folder: &str, uid: u32) -> Result<()> {
            self.calls.push(format!("delete {folder}/{uid}"));
            Ok(())
        }
        async fn ensure_folder(&mut self, name: &str) -> Result<()> {
            self.calls.push(format!("create {name}"));
            Ok(())
        }
        async fn list_unsubscribe_headers(
            &mut self,
            _folder: &str,
            _uid: u32,
        ) -> Result<(Option<String>, Option<String>)> {
            Ok((None, None))
        }
    }

    #[tokio::test]
    async fn run_rules_executes_enabled_rules_on_new_messages() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        db.create_rule(
            "acc-1",
            "tag-noreply",
            r#"{"from":"noreply@example.com"}"#,
            r#"{"add_tag":"automated"}"#,
            10,
            false,
        )
        .unwrap();
        db.create_rule(
            "acc-1",
            "flag-noreply",
            r#"{"from":"noreply@example.com"}"#,
            r#"{"flag":"flagged"}"#,
            20,
            false,
        )
        .unwrap();
        let account = RunAccount {
            id: "acc-1",
            email: "me@example.com",
        };
        let mut session = FakeSession::default();

        let report =
            run_rules_on_new_messages(&mut session, &db, &account, "INBOX", &[fixture_message()])
                .await
                .unwrap();

        assert_eq!(report.actions, 2, "{report:?}");
        assert_eq!(session.calls, vec!["flag INBOX/42 flagged".to_string()]);
        let tags = db.get_tags("acc-1", "fixture@example.com").unwrap();
        assert_eq!(tags[0].tag, "automated");
        let rows = db.list_actions("acc-1", 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|r| r.action_taken.contains("\"source\":\"rule\""))
        );

        // A second IDLE wake that re-sees the same UID mutates nothing.
        let again =
            run_rules_on_new_messages(&mut session, &db, &account, "INBOX", &[fixture_message()])
                .await
                .unwrap();
        assert_eq!(again.actions, 0);
        assert_eq!(session.calls.len(), 1);
    }
}
