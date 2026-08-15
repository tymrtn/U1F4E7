// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Message list + read + flag + move + delete + search.

use std::cmp::Ordering;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{DateTime, NaiveDateTime, Utc};
use envelope_email_store::ThreadContext;
use envelope_email_store::models::{
    Account, IndexedMessageInput, IndexedMessageSummary, Message, MessageSummary,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::state::AppState;

const UNIFIED_INBOX_FOLDER: &str = "INBOX";

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default = "default_folder")]
    pub folder: String,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_folder() -> String {
    "INBOX".to_string()
}

fn default_limit() -> u32 {
    50
}

#[derive(Deserialize)]
pub struct UnifiedInboxQuery {
    #[serde(default = "default_limit")]
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnifiedInboxMessage {
    #[serde(flatten)]
    pub summary: MessageSummary,
    pub unread: bool,
    pub thread_context: Option<ThreadContext>,
    pub account_id: String,
    pub account_username: String,
    pub account_display_name: Option<String>,
    pub folder: String,
    pub uidvalidity: u64,
    pub snippet: Option<String>,
    pub thread_id: Option<String>,
    pub indexed_at: Option<String>,
    pub index_freshness: String,
    #[serde(skip)]
    sort_index: usize,
}

impl UnifiedInboxMessage {
    fn from_indexed(
        indexed: IndexedMessageSummary,
        sort_index: usize,
        thread_context: Option<ThreadContext>,
    ) -> Self {
        let unread = summary_is_unread(&indexed.summary);
        Self {
            summary: indexed.summary,
            unread,
            thread_context,
            account_id: indexed.account_id,
            account_username: indexed.account_username,
            account_display_name: indexed.account_display_name,
            folder: indexed.folder,
            uidvalidity: indexed.uidvalidity,
            snippet: indexed.snippet,
            thread_id: indexed.thread_id,
            indexed_at: indexed.indexed_at,
            index_freshness: indexed.freshness,
            sort_index,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UnifiedInboxAccountResult {
    pub account_id: String,
    pub account_username: String,
    pub account_display_name: Option<String>,
    pub folder: String,
    pub ok: bool,
    pub message_count: usize,
    pub unread_count: usize,
    pub latest_message_date: Option<String>,
    pub freshness: UnifiedAccountFreshness,
    pub indexed_at: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnifiedAccountFreshness {
    Fresh,
    Empty,
    Stale,
    Partial,
    Unavailable,
}

impl UnifiedInboxAccountResult {
    fn cached(
        account: &Account,
        folder: &str,
        message_count: usize,
        unread_count: usize,
        latest_message_date: Option<String>,
        indexed_at: Option<String>,
        freshness: &str,
        last_error: Option<String>,
    ) -> Self {
        let (ok, freshness, error) = if let Some(error) = last_error {
            (false, UnifiedAccountFreshness::Unavailable, Some(error))
        } else {
            match freshness {
                "fresh" if message_count == 0 => (true, UnifiedAccountFreshness::Empty, None),
                "fresh" => (true, UnifiedAccountFreshness::Fresh, None),
                "stale" | "expired" => (true, UnifiedAccountFreshness::Stale, None),
                "unavailable" => (
                    false,
                    UnifiedAccountFreshness::Unavailable,
                    Some("cache unavailable; refresh required".to_string()),
                ),
                "missing" => (
                    false,
                    UnifiedAccountFreshness::Unavailable,
                    Some("cache missing; refresh required".to_string()),
                ),
                "empty" => (true, UnifiedAccountFreshness::Empty, None),
                _ => (
                    false,
                    UnifiedAccountFreshness::Unavailable,
                    Some("cache freshness unknown; refresh required".to_string()),
                ),
            }
        };
        Self {
            account_id: account.id.clone(),
            account_username: account.username.clone(),
            account_display_name: account.display_name.clone(),
            folder: folder.to_string(),
            ok,
            message_count,
            unread_count,
            latest_message_date,
            freshness,
            indexed_at,
            error,
        }
    }

    fn err(account: &Account, folder: &str, error: String) -> Self {
        Self {
            account_id: account.id.clone(),
            account_username: account.username.clone(),
            account_display_name: account.display_name.clone(),
            folder: folder.to_string(),
            ok: false,
            message_count: 0,
            unread_count: 0,
            latest_message_date: None,
            freshness: UnifiedAccountFreshness::Unavailable,
            indexed_at: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UnifiedInboxError {
    pub account_id: String,
    pub account_username: String,
    pub account_display_name: Option<String>,
    pub folder: String,
    pub error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnifiedInboxStatus {
    Empty,
    Ok,
    Partial,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnifiedInboxResponse {
    pub scope: &'static str,
    pub status: UnifiedInboxStatus,
    pub folder: String,
    pub limit: u32,
    pub messages: Vec<UnifiedInboxMessage>,
    pub accounts: Vec<UnifiedInboxAccountResult>,
    pub unread_count: usize,
    pub freshness: UnifiedAccountFreshness,
    pub errors: Vec<UnifiedInboxError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashboardMessageSummary {
    #[serde(flatten)]
    pub summary: MessageSummary,
    pub unread: bool,
    pub thread_context: Option<ThreadContext>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashboardMessage {
    #[serde(flatten)]
    pub message: Message,
    pub unread: bool,
    pub thread_context: Option<ThreadContext>,
}

pub async fn unified_inbox(
    State(state): State<AppState>,
    Query(q): Query<UnifiedInboxQuery>,
) -> impl IntoResponse {
    let accounts = {
        let db = state.db.lock().await;
        match db.list_accounts() {
            Ok(accounts) => accounts,
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}"))
                    .into_response();
            }
        }
    };

    let folder = UNIFIED_INBOX_FOLDER.to_string();
    let (messages, account_results) =
        match load_indexed_unified_inbox(&state, &accounts, &folder, q.limit).await {
            Ok(indexed) => indexed,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        };

    Json(build_unified_inbox_response(
        folder,
        q.limit,
        messages,
        account_results,
    ))
    .into_response()
}
pub async fn refresh_unified_inbox(
    State(state): State<AppState>,
    Query(q): Query<UnifiedInboxQuery>,
) -> impl IntoResponse {
    let accounts = {
        let db = state.db.lock().await;
        match db.list_accounts() {
            Ok(accounts) => accounts,
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}"))
                    .into_response();
            }
        }
    };

    let folder = UNIFIED_INBOX_FOLDER.to_string();
    let mut refresh_failures = Vec::new();

    for account in &accounts {
        let (client_arc, _creds) = match state.get_or_create_imap(&account.id).await {
            Ok(c) => c,
            Err(e) => {
                let error = format!("IMAP: {e}");
                persist_refresh_error(&state, &account.id, &folder, &error).await;
                refresh_failures.push(UnifiedInboxAccountResult::err(account, &folder, error));
                continue;
            }
        };
        let mut client = client_arc.lock().await;
        let uidvalidity =
            match envelope_email_transport::imap::examine_folder_info(&mut client, &folder).await {
                Ok(info) => info.uid_validity.unwrap_or(0) as u64,
                Err(e) => {
                    state.evict_imap(&account.id).await;
                    let error = format!("EXAMINE {folder}: {e}");
                    persist_refresh_error(&state, &account.id, &folder, &error).await;
                    refresh_failures.push(UnifiedInboxAccountResult::err(account, &folder, error));
                    continue;
                }
            };

        match envelope_email_transport::imap::fetch_folder_summaries_read_only(
            &mut client,
            &folder,
            q.limit,
        )
        .await
        {
            Ok(summaries) => {
                let inputs: Vec<IndexedMessageInput> = summaries
                    .iter()
                    .map(|summary| IndexedMessageInput {
                        uid: summary.uid,
                        message_id: summary.message_id.clone(),
                        from_addr: summary.from_addr.clone(),
                        to_addr: summary.to_addr.clone(),
                        subject: summary.subject.clone(),
                        date: summary.date.clone(),
                        flags: summary.flags.clone(),
                        size: summary.size,
                        snippet: None,
                        thread_id: None,
                    })
                    .collect();
                let write_result = {
                    let db = state.db.lock().await;
                    db.upsert_indexed_message_summaries(&account.id, &folder, uidvalidity, &inputs)
                };
                if let Err(e) = write_result {
                    let error = format!("index {folder}: {e}");
                    persist_refresh_error(&state, &account.id, &folder, &error).await;
                    refresh_failures.push(UnifiedInboxAccountResult::err(account, &folder, error));
                } else {
                    // Fold the freshly-indexed metadata — plus this account's
                    // sent drafts and any thread-cache rows that arrived since
                    // the last pass — into the contacts address history that
                    // compose autocomplete reads. Purely local SQL over rows
                    // already on disk: no extra IMAP work, and it rides the
                    // explicit refresh rather than the aggregate inbox load.
                    crate::handlers::address_book::catch_up_account(&state, &account.id).await;
                }
            }
            Err(e) => {
                state.evict_imap(&account.id).await;
                let error = format!("fetch {folder}: {e}");
                persist_refresh_error(&state, &account.id, &folder, &error).await;
                refresh_failures.push(UnifiedInboxAccountResult::err(account, &folder, error));
            }
        }
    }

    let (mut messages, mut account_results) =
        match load_indexed_unified_inbox(&state, &accounts, &folder, q.limit).await {
            Ok(indexed) => indexed,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        };

    apply_refresh_failures(&mut messages, &mut account_results, refresh_failures);

    // Publish a metadata-level `new_mail` event per successfully-refreshed
    // account. The unified refresh path has no server-side "new since last"
    // delta, so this fires on every refresh that reached IMAP, carrying only
    // post-refresh counts (no bodies/subjects/recipients). Clients treat it as
    // "the inbox index for this account may have changed — reconcile counts".
    // A dedicated IMAP IDLE push worker is intentionally out of scope this round.
    for result in &account_results {
        if result.ok {
            state
                .events
                .publish(crate::events::DashboardEvent::NewMail {
                    account_id: result.account_id.clone(),
                    message_count: result.message_count,
                    unread_count: result.unread_count,
                });
        }
    }

    Json(build_unified_inbox_response(
        folder,
        q.limit,
        messages,
        account_results,
    ))
    .into_response()
}

/// Persist a per-account/folder refresh-error marker. Persistence failure must
/// not be silent (it would let a stale phantom row survive), but it also must
/// not abort the response: the caller has already recorded the failure
/// in-memory and [`apply_refresh_failures`] fails closed regardless of whether
/// this write lands. We surface the persistence error to the log; the SQLite
/// error string carries no mailbox secrets.
async fn persist_refresh_error(state: &AppState, account_id: &str, folder: &str, error: &str) {
    let result = {
        let db = state.db.lock().await;
        db.record_message_index_error(account_id, folder, error)
    };
    if let Err(persist_err) = result {
        tracing::warn!(
            "unified refresh: failed to persist index error for account {account_id} folder {folder}: {persist_err}"
        );
    }
}

/// Fail closed for the current refresh response: drop every cached message
/// belonging to an account whose in-memory refresh failed, and overwrite its
/// account result with the failure result. This is independent of whether the
/// SQLite error marker persisted — if the marker write was rejected while reads
/// still work, the DB query would otherwise leak the account's stale rows.
fn apply_refresh_failures(
    messages: &mut Vec<UnifiedInboxMessage>,
    account_results: &mut [UnifiedInboxAccountResult],
    failures: Vec<UnifiedInboxAccountResult>,
) {
    let failed_account_ids: std::collections::HashSet<String> = failures
        .iter()
        .map(|failure| failure.account_id.clone())
        .collect();
    messages.retain(|message| !failed_account_ids.contains(&message.account_id));

    for failure in failures {
        if let Some(slot) = account_results
            .iter_mut()
            .find(|result| result.account_id == failure.account_id)
        {
            *slot = failure;
        }
    }
}

async fn load_indexed_unified_inbox(
    state: &AppState,
    accounts: &[Account],
    folder: &str,
    limit: u32,
) -> Result<(Vec<UnifiedInboxMessage>, Vec<UnifiedInboxAccountResult>), String> {
    let db = state.db.lock().await;
    let indexed = db
        .list_indexed_message_summaries(folder, limit)
        .map_err(|e| format!("db error: {e}"))?;
    let freshness = db
        .list_message_index_account_freshness(folder)
        .map_err(|e| format!("db error: {e}"))?;

    let messages: Vec<UnifiedInboxMessage> = indexed
        .into_iter()
        .enumerate()
        .map(|(idx, row)| {
            let thread_context = db
                .get_thread_context_for_uid(row.summary.uid, &row.folder, &row.account_id)
                .ok()
                .flatten();
            UnifiedInboxMessage::from_indexed(row, idx, thread_context)
        })
        .collect();

    let account_results = accounts
        .iter()
        .map(|account| {
            let account_messages: Vec<&UnifiedInboxMessage> = messages
                .iter()
                .filter(|message| message.account_id == account.id)
                .collect();
            let unread_count = account_messages
                .iter()
                .filter(|message| message.unread)
                .count();
            let latest_message_date = account_messages
                .iter()
                .filter_map(|message| message.summary.date.as_ref())
                .max_by(|a, b| compare_message_dates(a, b))
                .cloned();
            let account_freshness = freshness.iter().find(|row| row.account_id == account.id);
            let indexed_message_count = account_freshness
                .map(|row| row.message_count)
                .unwrap_or(account_messages.len());
            UnifiedInboxAccountResult::cached(
                account,
                folder,
                indexed_message_count,
                unread_count,
                latest_message_date,
                account_freshness.and_then(|row| row.indexed_at.clone()),
                account_freshness
                    .map(|row| row.freshness.as_str())
                    .unwrap_or("missing"),
                account_freshness.and_then(|row| row.last_error.clone()),
            )
        })
        .collect();

    Ok((messages, account_results))
}

fn build_unified_inbox_response(
    folder: String,
    limit: u32,
    messages: Vec<UnifiedInboxMessage>,
    accounts: Vec<UnifiedInboxAccountResult>,
) -> UnifiedInboxResponse {
    let status = unified_inbox_status(&accounts);
    let unread_count = accounts.iter().map(|account| account.unread_count).sum();
    let freshness = unified_inbox_freshness(&accounts, status);
    let errors = accounts
        .iter()
        .filter_map(|account| {
            account.error.as_ref().map(|error| UnifiedInboxError {
                account_id: account.account_id.clone(),
                account_username: account.account_username.clone(),
                account_display_name: account.account_display_name.clone(),
                folder: account.folder.clone(),
                error: error.clone(),
            })
        })
        .collect();

    UnifiedInboxResponse {
        scope: "unified_inbox",
        status,
        folder,
        limit,
        messages: merge_unified_messages(messages, limit),
        accounts,
        unread_count,
        freshness,
        errors,
    }
}

fn unified_inbox_freshness(
    accounts: &[UnifiedInboxAccountResult],
    status: UnifiedInboxStatus,
) -> UnifiedAccountFreshness {
    match status {
        UnifiedInboxStatus::Empty => UnifiedAccountFreshness::Empty,
        UnifiedInboxStatus::Partial => UnifiedAccountFreshness::Partial,
        UnifiedInboxStatus::Error => UnifiedAccountFreshness::Unavailable,
        UnifiedInboxStatus::Ok => {
            if accounts
                .iter()
                .all(|account| account.freshness == UnifiedAccountFreshness::Empty)
            {
                return UnifiedAccountFreshness::Empty;
            }

            let has_stale = accounts
                .iter()
                .any(|account| account.freshness == UnifiedAccountFreshness::Stale);
            if has_stale {
                return if accounts
                    .iter()
                    .all(|account| account.freshness == UnifiedAccountFreshness::Stale)
                {
                    UnifiedAccountFreshness::Stale
                } else {
                    UnifiedAccountFreshness::Partial
                };
            }

            if accounts.iter().any(|account| {
                matches!(
                    account.freshness,
                    UnifiedAccountFreshness::Partial | UnifiedAccountFreshness::Unavailable
                )
            }) {
                UnifiedAccountFreshness::Partial
            } else {
                UnifiedAccountFreshness::Fresh
            }
        }
    }
}

fn unified_inbox_status(accounts: &[UnifiedInboxAccountResult]) -> UnifiedInboxStatus {
    if accounts.is_empty() {
        return UnifiedInboxStatus::Empty;
    }

    let successes = accounts.iter().filter(|account| account.ok).count();
    let failures = accounts.len().saturating_sub(successes);

    match (successes, failures) {
        (_, 0) => UnifiedInboxStatus::Ok,
        (0, _) => UnifiedInboxStatus::Error,
        _ => UnifiedInboxStatus::Partial,
    }
}

fn merge_unified_messages(
    mut messages: Vec<UnifiedInboxMessage>,
    limit: u32,
) -> Vec<UnifiedInboxMessage> {
    messages.sort_by(compare_unified_messages);
    messages.truncate(limit as usize);
    messages
}

fn compare_unified_messages(a: &UnifiedInboxMessage, b: &UnifiedInboxMessage) -> Ordering {
    let a_date = parsed_message_date(a.summary.date.as_deref());
    let b_date = parsed_message_date(b.summary.date.as_deref());

    let primary = match (a_date, b_date) {
        (Some(a_date), Some(b_date)) => b_date.cmp(&a_date),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };

    primary.then_with(|| a.sort_index.cmp(&b.sort_index))
}

fn parsed_message_date(raw: Option<&str>) -> Option<DateTime<Utc>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }

    DateTime::parse_from_rfc2822(raw)
        .or_else(|_| DateTime::parse_from_rfc3339(raw))
        .or_else(|_| DateTime::parse_from_str(raw, "%d %b %Y %H:%M:%S %z"))
        .or_else(|_| DateTime::parse_from_str(raw, "%d-%b-%Y %H:%M:%S %z"))
        .ok()
        .map(|date| date.with_timezone(&Utc))
}

fn compare_message_dates(a: &str, b: &str) -> Ordering {
    match (parsed_message_date(Some(a)), parsed_message_date(Some(b))) {
        (Some(a_date), Some(b_date)) => a_date.cmp(&b_date),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => a.cmp(b),
    }
}

fn summary_is_unread(summary: &MessageSummary) -> bool {
    !summary
        .flags
        .iter()
        .any(|flag| flag.to_ascii_lowercase().contains("seen"))
}

fn message_is_unread(message: &Message) -> bool {
    !message
        .flags
        .iter()
        .any(|flag| flag.to_ascii_lowercase().contains("seen"))
}

async fn thread_context_for_uid(
    state: &AppState,
    account_id: &str,
    folder: &str,
    uid: u32,
) -> Option<ThreadContext> {
    let db = state.db.lock().await;
    db.get_thread_context_for_uid(uid, folder, account_id)
        .ok()
        .flatten()
}

async fn thread_contexts_for_summaries(
    state: &AppState,
    account: &Account,
    folder: &str,
    summaries: &[MessageSummary],
) -> Vec<Option<ThreadContext>> {
    let db = state.db.lock().await;
    summaries
        .iter()
        .map(|summary| {
            db.get_thread_context_for_uid(summary.uid, folder, &account.id)
                .ok()
                .flatten()
        })
        .collect()
}

pub async fn list(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    Query(q): Query<ListQuery>,
) -> impl IntoResponse {
    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response();
        }
    };
    let mut client = client_arc.lock().await;

    match envelope_email_transport::imap::fetch_folder_summaries_read_only(
        &mut client,
        &q.folder,
        q.limit,
    )
    .await
    {
        Ok(msgs) => {
            let account = Account {
                id: account_id.clone(),
                name: String::new(),
                username: String::new(),
                domain: String::new(),
                smtp_host: String::new(),
                smtp_port: 0,
                imap_host: String::new(),
                imap_port: 0,
                smtp_username: None,
                imap_username: None,
                display_name: None,
                signature_text: None,
                signature_html: None,
                created_at: String::new(),
            };
            let thread_contexts =
                thread_contexts_for_summaries(&state, &account, &q.folder, &msgs).await;
            let messages: Vec<_> = msgs
                .into_iter()
                .zip(thread_contexts)
                .map(|(summary, thread_context)| DashboardMessageSummary {
                    unread: summary_is_unread(&summary),
                    summary,
                    thread_context,
                })
                .collect();
            Json(json!({ "messages": messages })).into_response()
        }
        Err(e) => {
            state.evict_imap(&account_id).await;
            (StatusCode::BAD_GATEWAY, format!("fetch: {e}")).into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct ReadQuery {
    #[serde(default = "default_folder")]
    pub folder: String,
}

pub async fn read(
    State(state): State<AppState>,
    Path((account_id, uid)): Path<(String, u32)>,
    Query(q): Query<ReadQuery>,
) -> impl IntoResponse {
    // A cached IMAP client can go stale (e.g. a transient
    // "Can't assign requested address" on SELECT against a synced drafts
    // folder). Evict the cached client and retry once with a fresh
    // connection before surfacing a 502. Bounded to a single retry.
    let mut last_err: Option<String> = None;
    for attempt in 0..2 {
        let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
            Ok(c) => c,
            Err(e) => {
                return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response();
            }
        };
        let fetched = {
            let mut client = client_arc.lock().await;
            envelope_email_transport::imap::fetch_message(&mut client, &q.folder, uid).await
        };

        match fetched {
            Ok(Some(msg)) => {
                let thread_context =
                    thread_context_for_uid(&state, &account_id, &q.folder, uid).await;
                let message = DashboardMessage {
                    unread: message_is_unread(&msg),
                    message: msg,
                    thread_context,
                };
                return Json(json!({ "message": message })).into_response();
            }
            Ok(None) => return (StatusCode::NOT_FOUND, "message not found").into_response(),
            Err(e) => {
                // Drop the (possibly stale) cached connection so the next
                // attempt reconnects. On the final attempt, fall through to
                // the 502 below.
                state.evict_imap(&account_id).await;
                last_err = Some(format!("fetch: {e}"));
                if attempt == 0 {
                    continue;
                }
            }
        }
    }
    (
        StatusCode::BAD_GATEWAY,
        last_err.unwrap_or_else(|| "fetch: IMAP error".to_string()),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct FlagsRequest {
    #[serde(default = "default_folder")]
    pub folder: String,
    #[serde(default)]
    pub add: Vec<String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

pub async fn flags(
    State(state): State<AppState>,
    Path((account_id, uid)): Path<(String, u32)>,
    Json(req): Json<FlagsRequest>,
) -> impl IntoResponse {
    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response();
        }
    };
    let mut client = client_arc.lock().await;

    for flag in &req.add {
        if let Err(e) =
            envelope_email_transport::imap::set_flag(&mut client, &req.folder, uid, flag).await
        {
            state.evict_imap(&account_id).await;
            return (StatusCode::BAD_GATEWAY, format!("set_flag {flag}: {e}")).into_response();
        }
    }
    for flag in &req.remove {
        if let Err(e) =
            envelope_email_transport::imap::remove_flag(&mut client, &req.folder, uid, flag).await
        {
            state.evict_imap(&account_id).await;
            return (StatusCode::BAD_GATEWAY, format!("remove_flag {flag}: {e}")).into_response();
        }
    }
    Json(json!({ "ok": true, "uid": uid, "added": req.add, "removed": req.remove })).into_response()
}

#[derive(Deserialize)]
pub struct MoveRequest {
    #[serde(default = "default_folder")]
    pub folder: String,
    /// Destination. Either a literal folder name (operator-picked `Move…`) or a
    /// canonical special-use sentinel (`\Archive`, `\Junk`, `\Trash`) that is
    /// resolved to the account's real provider folder before any move. See
    /// [`envelope_email_transport::folders::canonical_move_key`].
    pub to_folder: String,
}

/// Send-safe canonical folder resolution against the shared (mutex-guarded) DB.
///
/// Mirrors [`envelope_email_transport::folders::detect_folder`] — cache →
/// provider-resolve-and-verify → candidate fallback → `None` — but reuses only
/// the crate's PURE provider machinery (`detect_provider` / `resolve_folder` /
/// `all_candidates_for`) so the DB guard is never held across the IMAP
/// `list_folders` await. `Database` is `!Sync`, so a `&Database` held across an
/// await would make the axum handler future `!Send`; every DB touch here is a
/// short synchronous critical section. `Ok(None)` means the provider has no
/// such special-use folder — callers must NOT fall back to a literal mailbox.
pub(crate) async fn resolve_canonical_folder(
    state: &AppState,
    client: &mut envelope_email_transport::ImapClient,
    account_id: &str,
    canonical_type: &str,
) -> Result<Option<String>, envelope_email_transport::errors::ImapError> {
    use envelope_email_transport::provider::{self, ProviderType};

    // 1. Cache hit — no socket, no await while the guard is held.
    let cached = {
        let db = state.db.lock().await;
        db.get_detected_folders(account_id).ok().and_then(|rows| {
            rows.into_iter()
                .find(|(ftype, _)| ftype == canonical_type)
                .map(|(_, name)| name)
        })
    };
    if let Some(name) = cached {
        return Ok(Some(name));
    }

    // 2. Stored provider type (sync read).
    let stored = {
        let db = state.db.lock().await;
        db.get_provider_type(account_id).ok().flatten()
    };
    let mut provider = stored
        .as_deref()
        .map(ProviderType::from_str_value)
        .unwrap_or(ProviderType::Unknown);

    // 3. Folder inventory — the one IMAP round-trip, with NO DB guard held.
    let folders = envelope_email_transport::imap::list_folders(client).await?;

    // Detect + persist the provider when it was unknown.
    if provider == ProviderType::Unknown {
        provider = provider::detect_provider(&folders);
        if provider != ProviderType::Unknown {
            let db = state.db.lock().await;
            let _ = db.set_provider_type(account_id, provider.as_str());
        }
    }

    // 4. Provider-resolved name, verified to actually exist on the server.
    if provider != ProviderType::Unknown {
        let resolved = provider::resolve_folder(provider, canonical_type).to_string();
        if folders.iter().any(|f| f == &resolved) {
            let db = state.db.lock().await;
            let _ = db.set_detected_folder(account_id, canonical_type, &resolved);
            return Ok(Some(resolved));
        }
    }

    // 5. Candidate fallback across every known provider variant for this type.
    for candidate in provider::all_candidates_for(canonical_type) {
        if folders.iter().any(|f| f == candidate) {
            let db = state.db.lock().await;
            let _ = db.set_detected_folder(account_id, canonical_type, candidate);
            return Ok(Some(candidate.to_string()));
        }
    }

    Ok(None)
}

/// Stable JSON failure for a canonical move target that resolved to no real
/// provider folder. Returned BEFORE any mutation; carries only the requested
/// target token (no recipients, bodies, or folder listings).
fn move_target_unresolved(target: &str) -> (StatusCode, serde_json::Value) {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({
            "code": "folder_not_resolved",
            "reason": format!(
                "no provider folder matched the canonical target {target}; pick a folder with Move… instead"
            ),
        }),
    )
}

pub async fn mv(
    State(state): State<AppState>,
    Path((account_id, uid)): Path<(String, u32)>,
    Json(req): Json<MoveRequest>,
) -> impl IntoResponse {
    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response();
        }
    };
    let mut client = client_arc.lock().await;

    // Resolve a canonical sentinel (`\Archive`/`\Junk`/`\Trash`) to the account's
    // actual provider folder; a literal `Move…` folder passes straight through.
    // Unresolved canonical targets fail with a stable code and NEVER fall back to
    // creating or moving into a literal `Archive`/`Junk`/`Trash`.
    let to_folder = match envelope_email_transport::folders::canonical_move_key(&req.to_folder) {
        Some(canonical_type) => {
            match resolve_canonical_folder(&state, &mut client, &account_id, canonical_type).await {
                Ok(Some(name)) => name,
                Ok(None) => {
                    let (status, body) = move_target_unresolved(&req.to_folder);
                    return (status, Json(body)).into_response();
                }
                Err(e) => {
                    state.evict_imap(&account_id).await;
                    return (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({
                            "code": "folder_resolution_failed",
                            "reason": format!("could not resolve move target: {e}"),
                        })),
                    )
                        .into_response();
                }
            }
        }
        None => req.to_folder.clone(),
    };

    match envelope_email_transport::imap::move_message(&mut client, uid, &req.folder, &to_folder)
        .await
    {
        Ok(()) => Json(json!({ "ok": true, "uid": uid, "moved_to": to_folder })).into_response(),
        Err(e) => {
            state.evict_imap(&account_id).await;
            (StatusCode::BAD_GATEWAY, format!("move: {e}")).into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct DeleteQuery {
    #[serde(default = "default_folder")]
    pub folder: String,
}

pub async fn delete(
    State(state): State<AppState>,
    Path((account_id, uid)): Path<(String, u32)>,
    Query(q): Query<DeleteQuery>,
) -> impl IntoResponse {
    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response();
        }
    };
    let mut client = client_arc.lock().await;

    match envelope_email_transport::imap::delete_message(&mut client, &q.folder, uid).await {
        Ok(()) => Json(json!({ "ok": true, "uid": uid, "deleted_from": q.folder })).into_response(),
        Err(e) => {
            state.evict_imap(&account_id).await;
            (StatusCode::BAD_GATEWAY, format!("delete: {e}")).into_response()
        }
    }
}

/// The app-managed folder snoozed mail is parked in (matches `envelope snooze`).
const SNOOZED_FOLDER: &str = "Snoozed";

/// Normalize a client snooze target into UTC wall-clock (`%Y-%m-%dT%H:%M:%S`) —
/// the exact shape `envelope snooze set` writes and the background unsnooze
/// sweep compares against UTC now. Accepts an RFC3339 instant (`…Z`/offset) or a
/// naive timestamp (treated as UTC). Rejects empty and non-future times.
fn normalize_snooze_return_at(input: &str, now: NaiveDateTime) -> Result<String, &'static str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("return_at is required");
    }
    let naive = if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        dt.with_timezone(&Utc).naive_utc()
    } else if let Ok(ndt) = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S") {
        ndt
    } else if let Ok(ndt) = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M") {
        ndt
    } else {
        return Err("return_at must be an ISO-8601 timestamp (YYYY-MM-DDTHH:MM:SS)");
    };
    if naive <= now {
        return Err("return_at must be in the future");
    }
    Ok(naive.format("%Y-%m-%dT%H:%M:%S").to_string())
}

#[derive(Deserialize)]
pub struct SnoozeRequest {
    #[serde(default = "default_folder")]
    pub folder: String,
    pub return_at: String,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
}

/// POST /api/accounts/{id}/messages/{uid}/snooze
/// Move a message to the Snoozed folder until `return_at`, and record it so the
/// existing background sweep returns it to its origin folder. Reuses the same
/// primitives as `envelope snooze set` (create-folder + move + `create_snoozed`).
pub async fn snooze(
    State(state): State<AppState>,
    Path((account_id, uid)): Path<(String, u32)>,
    Json(req): Json<SnoozeRequest>,
) -> impl IntoResponse {
    // Validate the target time BEFORE any IMAP work so bad input never touches
    // the mailbox and returns a stable, machine-readable error.
    let return_at = match normalize_snooze_return_at(&req.return_at, Utc::now().naive_utc()) {
        Ok(v) => v,
        Err(code) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "code": "invalid_return_at", "error": code })),
            )
                .into_response();
        }
    };

    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response();
        }
    };
    let mut client = client_arc.lock().await;

    // Ensure the Snoozed folder exists (idempotent; may already be present).
    if let Err(e) = envelope_email_transport::imap::create_folder(&mut client, SNOOZED_FOLDER).await
    {
        tracing::warn!("snooze: could not ensure {SNOOZED_FOLDER} folder (may exist): {e}");
    }

    if let Err(e) =
        envelope_email_transport::imap::move_message(&mut client, uid, &req.folder, SNOOZED_FOLDER)
            .await
    {
        state.evict_imap(&account_id).await;
        return (StatusCode::BAD_GATEWAY, format!("snooze move: {e}")).into_response();
    }
    drop(client);

    // Record so the sweep can return it. `account` is the path id — matching how
    // the dashboard snoozed list/unsnooze query rows back.
    let db = state.db.lock().await;
    match db.create_snoozed(
        &account_id,
        uid,
        &req.folder,
        SNOOZED_FOLDER,
        &return_at,
        req.message_id.as_deref(),
        req.subject.as_deref(),
        Some("dashboard"),
        None,
        None,
    ) {
        Ok(_) => Json(json!({
            "ok": true,
            "uid": uid,
            "return_at": return_at,
            "snoozed_folder": SNOOZED_FOLDER
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "code": "snooze_record_failed", "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default = "default_folder")]
    pub folder: String,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

pub async fn search(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    Query(sq): Query<SearchQuery>,
) -> impl IntoResponse {
    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response();
        }
    };
    let mut client = client_arc.lock().await;

    match envelope_email_transport::imap::search(&mut client, &sq.folder, &sq.q, sq.limit).await {
        Ok(results) => {
            let account = Account {
                id: account_id.clone(),
                name: String::new(),
                username: String::new(),
                domain: String::new(),
                smtp_host: String::new(),
                smtp_port: 0,
                imap_host: String::new(),
                imap_port: 0,
                smtp_username: None,
                imap_username: None,
                display_name: None,
                signature_text: None,
                signature_html: None,
                created_at: String::new(),
            };
            let thread_contexts =
                thread_contexts_for_summaries(&state, &account, &sq.folder, &results).await;
            let messages: Vec<_> = results
                .into_iter()
                .zip(thread_contexts)
                .map(|(summary, thread_context)| DashboardMessageSummary {
                    unread: summary_is_unread(&summary),
                    summary,
                    thread_context,
                })
                .collect();
            Json(json!({ "messages": messages, "query": sq.q })).into_response()
        }
        Err(e) => {
            state.evict_imap(&account_id).await;
            (StatusCode::BAD_GATEWAY, format!("search: {e}")).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDateTime;
    use envelope_email_store::{CredentialBackend, Database};
    use serde_json::json;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").unwrap()
    }

    #[test]
    fn move_target_unresolved_returns_stable_code_and_leaks_nothing() {
        // A canonical move target that resolves to no real provider folder must
        // fail with a stable machine code BEFORE any mutation, carrying only the
        // requested target label — never a recipient, body, or folder listing.
        let (status, body) = move_target_unresolved("\\Trash");
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["code"], "folder_not_resolved");
        let reason = body["reason"].as_str().unwrap();
        assert!(reason.contains("\\Trash"));
        // No leakage: the reason is a fixed template plus the target token only.
        assert!(!reason.contains('@'));
    }

    #[test]
    fn snooze_return_at_rfc3339_z_stored_as_utc_wallclock() {
        // The sweep compares against UTC now; storage must be UTC wall-clock,
        // no offset/Z, matching `envelope snooze set`.
        let out =
            normalize_snooze_return_at("2026-08-09T14:30:00Z", at("2026-08-08T12:00:00")).unwrap();
        assert_eq!(out, "2026-08-09T14:30:00");
    }

    #[test]
    fn snooze_return_at_offset_converted_to_utc() {
        // 09:00-05:00 == 14:00 UTC.
        let out =
            normalize_snooze_return_at("2026-08-09T09:00:00-05:00", at("2026-08-08T12:00:00"))
                .unwrap();
        assert_eq!(out, "2026-08-09T14:00:00");
    }

    #[test]
    fn snooze_return_at_naive_kept_as_utc() {
        let out =
            normalize_snooze_return_at("2026-08-09T09:00:00", at("2026-08-08T12:00:00")).unwrap();
        assert_eq!(out, "2026-08-09T09:00:00");
        // datetime-local without seconds is also accepted.
        let out2 =
            normalize_snooze_return_at("2026-08-09T09:00", at("2026-08-08T12:00:00")).unwrap();
        assert_eq!(out2, "2026-08-09T09:00:00");
    }

    #[test]
    fn snooze_return_at_rejects_empty() {
        assert!(normalize_snooze_return_at("   ", at("2026-08-08T12:00:00")).is_err());
    }

    #[test]
    fn snooze_return_at_rejects_past() {
        assert!(
            normalize_snooze_return_at("2026-08-07T09:00:00Z", at("2026-08-08T12:00:00")).is_err()
        );
    }

    #[test]
    fn snooze_return_at_rejects_garbage() {
        assert!(normalize_snooze_return_at("next tuesday", at("2026-08-08T12:00:00")).is_err());
    }

    fn summary(uid: u32, date: Option<&str>) -> MessageSummary {
        MessageSummary {
            uid,
            message_id: Some(format!("<{uid}@example.test>")),
            from_addr: format!("sender-{uid}@example.test"),
            to_addr: "me@example.test".to_string(),
            subject: format!("message {uid}"),
            date: date.map(str::to_string),
            flags: Vec::new(),
            size: 100,
            provider_spam: None,
        }
    }

    fn unified(
        account_id: &str,
        uid: u32,
        date: Option<&str>,
        sort_index: usize,
    ) -> UnifiedInboxMessage {
        UnifiedInboxMessage {
            summary: summary(uid, date),
            unread: true,
            thread_context: None,
            account_id: account_id.to_string(),
            account_username: format!("{account_id}@example.test"),
            account_display_name: None,
            folder: "INBOX".to_string(),
            uidvalidity: 99,
            snippet: Some(format!("snippet {uid}")),
            thread_id: Some(format!("thread-{uid}")),
            indexed_at: Some("2026-05-12T12:00:00Z".to_string()),
            index_freshness: "fresh".to_string(),
            sort_index,
        }
    }

    fn account_result(
        account_id: &str,
        ok: bool,
        error: Option<&str>,
    ) -> UnifiedInboxAccountResult {
        account_result_with_freshness(
            account_id,
            ok,
            if ok {
                UnifiedAccountFreshness::Fresh
            } else {
                UnifiedAccountFreshness::Unavailable
            },
            error,
        )
    }

    fn account_result_with_freshness(
        account_id: &str,
        ok: bool,
        freshness: UnifiedAccountFreshness,
        error: Option<&str>,
    ) -> UnifiedInboxAccountResult {
        UnifiedInboxAccountResult {
            account_id: account_id.to_string(),
            account_username: format!("{account_id}@example.test"),
            account_display_name: None,
            folder: "INBOX".to_string(),
            ok,
            message_count: if ok { 1 } else { 0 },
            unread_count: if ok { 1 } else { 0 },
            latest_message_date: if ok {
                Some("Tue, 12 May 2026 12:00:00 +0000".to_string())
            } else {
                None
            },
            freshness,
            indexed_at: ok.then(|| "2026-05-12T12:00:00Z".to_string()),
            error: error.map(str::to_string),
        }
    }

    #[test]
    fn unified_merge_sorts_newest_first_by_parsed_date_then_stable_fallback() {
        let merged = merge_unified_messages(
            vec![
                unified("acct-a", 10, Some("Tue, 12 May 2026 10:00:00 +0000"), 0),
                unified("acct-b", 20, Some("Tue, 12 May 2026 12:00:00 +0000"), 1),
                unified("acct-a", 30, None, 2),
                unified("acct-b", 40, Some("not a date"), 3),
                unified("acct-c", 50, Some("Tue, 12 May 2026 12:00:00 +0000"), 4),
            ],
            10,
        );

        let ordered: Vec<(&str, u32)> = merged
            .iter()
            .map(|message| (message.account_id.as_str(), message.summary.uid))
            .collect();

        assert_eq!(
            ordered,
            vec![
                ("acct-b", 20),
                ("acct-c", 50),
                ("acct-a", 10),
                ("acct-a", 30),
                ("acct-b", 40),
            ]
        );
    }

    #[test]
    fn apply_refresh_failures_fails_closed_even_without_persisted_marker() {
        // Simulate a refresh whose DB error-marker write was rejected: the DB
        // query still returns the failed account's stale phantom row and still
        // reports it as ok/fresh. The in-memory failure must nonetheless drop
        // the row from the response and mark the account unavailable.
        let mut messages = vec![
            unified("acct-ok", 10, Some("Tue, 12 May 2026 10:00:00 +0000"), 0),
            unified(
                "acct-failed",
                20,
                Some("Thu, 09 Jul 2026 08:42:00 +0000"),
                1,
            ),
        ];
        let mut account_results = vec![
            account_result("acct-ok", true, None),
            // Stale success: as if last_error never persisted.
            account_result("acct-failed", true, None),
        ];
        let failures = vec![account_result(
            "acct-failed",
            false,
            Some("IMAP: auth failed"),
        )];

        apply_refresh_failures(&mut messages, &mut account_results, failures);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].account_id, "acct-ok");
        assert!(messages.iter().all(|m| m.account_id != "acct-failed"));

        let failed = account_results
            .iter()
            .find(|result| result.account_id == "acct-failed")
            .expect("failed account result");
        assert!(!failed.ok);
        assert_eq!(failed.freshness, UnifiedAccountFreshness::Unavailable);
        assert_eq!(failed.message_count, 0);
        assert_eq!(failed.error.as_deref(), Some("IMAP: auth failed"));

        let healthy = account_results
            .iter()
            .find(|result| result.account_id == "acct-ok")
            .expect("healthy account result");
        assert!(healthy.ok);
    }

    #[tokio::test]
    async fn indexed_unified_inbox_loads_from_local_cache_without_imap() {
        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acct-a', 'Account A', 'a@example.test', 'example.test',
                         'smtp.example.test', 587, 'imap.example.test', 993, 'encrypted')",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acct-b', 'Account B', 'b@example.test', 'example.test',
                         'smtp.example.test', 587, 'imap.example.test', 993, 'encrypted')",
                [],
            )
            .unwrap();
        let thread = db
            .create_thread(
                "cached first paint",
                "2026-05-12T12:00:00Z",
                "2026-05-12T13:00:00Z",
                "acct-a",
            )
            .unwrap();
        db.upsert_thread_message(
            &thread.thread_id,
            42,
            Some("<cached@example.test>"),
            None,
            None,
            "INBOX",
            "sender@example.test",
            "me@example.test",
            None,
            None,
            "2026-05-12T12:00:00Z",
            "cached first paint",
            false,
            Some("cached preview"),
        )
        .unwrap();
        db.upsert_thread_message(
            &thread.thread_id,
            7,
            Some("<reply@example.test>"),
            Some("<cached@example.test>"),
            Some("<cached@example.test>"),
            "Sent",
            "me@example.test",
            "sender@example.test",
            None,
            None,
            "2026-05-12T13:00:00Z",
            "Re: cached first paint",
            true,
            Some("sent reply"),
        )
        .unwrap();
        db.refresh_thread_stats(&thread.thread_id).unwrap();
        db.upsert_indexed_message_summaries(
            "acct-a",
            "INBOX",
            88,
            &[IndexedMessageInput {
                uid: 42,
                message_id: Some("<cached@example.test>".to_string()),
                from_addr: "sender@example.test".to_string(),
                to_addr: "me@example.test".to_string(),
                subject: "cached first paint".to_string(),
                date: Some("Tue, 12 May 2026 12:00:00 +0000".to_string()),
                flags: Vec::new(),
                size: 123,
                snippet: Some("cached preview".to_string()),
                thread_id: Some(thread.thread_id.clone()),
            }],
        )
        .unwrap();
        let accounts = db.list_accounts().unwrap();
        let state = AppState::new(db, CredentialBackend::File);

        let (messages, accounts) = load_indexed_unified_inbox(&state, &accounts, "INBOX", 10)
            .await
            .unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].account_id, "acct-a");
        assert_eq!(messages[0].uidvalidity, 88);
        assert_eq!(messages[0].snippet.as_deref(), Some("cached preview"));
        assert_eq!(
            messages[0].thread_id.as_deref(),
            Some(thread.thread_id.as_str())
        );
        let thread_context = messages[0]
            .thread_context
            .as_ref()
            .expect("indexed unified rows should carry cached thread context when available");
        assert_eq!(thread_context.thread_id, thread.thread_id);
        assert_eq!(thread_context.thread_count, 2);
        assert_eq!(thread_context.last_activity, "2026-05-12T13:00:00Z");
        assert!(thread_context.has_reply);
        assert_eq!(thread_context.reply_uid, Some(7));
        assert_eq!(thread_context.reply_folder.as_deref(), Some("Sent"));
        assert_eq!(messages[0].index_freshness, "fresh");
        assert_eq!(accounts.len(), 2);
        assert!(
            accounts
                .iter()
                .any(|account| account.account_id == "acct-a" && account.ok)
        );
        assert!(accounts.iter().any(|account| {
            account.account_id == "acct-b"
                && !account.ok
                && account.freshness == UnifiedAccountFreshness::Unavailable
                && account.error.as_deref() == Some("cache missing; refresh required")
        }));
    }

    #[tokio::test]
    async fn indexed_unified_inbox_surfaces_refreshed_empty_account_as_empty_ok() {
        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acct-empty', 'Empty Account', 'empty@example.test', 'example.test',
                         'smtp.example.test', 587, 'imap.example.test', 993, 'encrypted')",
                [],
            )
            .unwrap();
        db.upsert_indexed_message_summaries("acct-empty", "INBOX", 321, &[])
            .unwrap();
        let accounts = db.list_accounts().unwrap();
        let state = AppState::new(db, CredentialBackend::File);

        let (messages, accounts) = load_indexed_unified_inbox(&state, &accounts, "INBOX", 10)
            .await
            .unwrap();

        assert!(messages.is_empty());
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].account_id, "acct-empty");
        assert!(accounts[0].ok);
        assert_eq!(accounts[0].message_count, 0);
        assert_eq!(accounts[0].freshness, UnifiedAccountFreshness::Empty);
        assert!(accounts[0].indexed_at.is_some());
        assert!(accounts[0].error.is_none());
    }

    #[tokio::test]
    async fn indexed_unified_inbox_uses_freshness_count_when_global_limit_hides_account_rows() {
        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acct-hidden', 'Hidden Account', 'hidden@example.test', 'example.test',
                         'smtp.example.test', 587, 'imap.example.test', 993, 'encrypted')",
                [],
            )
            .unwrap();
        db.upsert_indexed_message_summaries(
            "acct-hidden",
            "INBOX",
            444,
            &[IndexedMessageInput {
                uid: 99,
                message_id: Some("<hidden@example.test>".to_string()),
                from_addr: "sender@example.test".to_string(),
                to_addr: "hidden@example.test".to_string(),
                subject: "hidden by zero limit".to_string(),
                date: Some("Tue, 12 May 2026 12:00:00 +0000".to_string()),
                flags: Vec::new(),
                size: 123,
                snippet: Some("cached preview".to_string()),
                thread_id: None,
            }],
        )
        .unwrap();
        let accounts = db.list_accounts().unwrap();
        let state = AppState::new(db, CredentialBackend::File);

        let (messages, accounts) = load_indexed_unified_inbox(&state, &accounts, "INBOX", 0)
            .await
            .unwrap();

        assert!(messages.is_empty());
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].account_id, "acct-hidden");
        assert!(accounts[0].ok);
        assert_eq!(accounts[0].message_count, 1);
        assert_eq!(accounts[0].freshness, UnifiedAccountFreshness::Fresh);
        assert!(accounts[0].indexed_at.is_some());
        assert!(accounts[0].error.is_none());
    }

    #[test]
    fn unified_response_reports_stale_top_level_when_all_accounts_are_stale() {
        let response = build_unified_inbox_response(
            "INBOX".to_string(),
            50,
            vec![unified(
                "acct-a",
                7,
                Some("Tue, 12 May 2026 12:00:00 +0000"),
                0,
            )],
            vec![
                account_result_with_freshness("acct-a", true, UnifiedAccountFreshness::Stale, None),
                account_result_with_freshness("acct-b", true, UnifiedAccountFreshness::Stale, None),
            ],
        );

        assert_eq!(response.status, UnifiedInboxStatus::Ok);
        assert_eq!(response.freshness, UnifiedAccountFreshness::Stale);
    }

    #[test]
    fn unified_response_reports_partial_top_level_when_freshness_is_mixed() {
        let response = build_unified_inbox_response(
            "INBOX".to_string(),
            50,
            vec![unified(
                "acct-fresh",
                7,
                Some("Tue, 12 May 2026 12:00:00 +0000"),
                0,
            )],
            vec![
                account_result_with_freshness(
                    "acct-fresh",
                    true,
                    UnifiedAccountFreshness::Fresh,
                    None,
                ),
                account_result_with_freshness(
                    "acct-stale",
                    true,
                    UnifiedAccountFreshness::Stale,
                    None,
                ),
            ],
        );

        assert_eq!(response.status, UnifiedInboxStatus::Ok);
        assert_eq!(response.freshness, UnifiedAccountFreshness::Partial);
    }

    #[test]
    fn unified_response_preserves_partial_failure_shape() {
        let response = build_unified_inbox_response(
            "INBOX".to_string(),
            50,
            vec![unified(
                "acct-ok",
                7,
                Some("Tue, 12 May 2026 12:00:00 +0000"),
                0,
            )],
            vec![
                account_result("acct-ok", true, None),
                account_result("acct-bad", false, Some("IMAP: login failed")),
            ],
        );

        assert_eq!(response.status, UnifiedInboxStatus::Partial);
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["status"], "partial");
        assert_eq!(value["messages"][0]["account_id"], "acct-ok");
        assert_eq!(value["messages"][0]["folder"], "INBOX");
        assert_eq!(
            value["accounts"],
            json!([
                {
                    "account_id": "acct-ok",
                    "account_username": "acct-ok@example.test",
                    "account_display_name": null,
                    "folder": "INBOX",
                    "ok": true,
                    "message_count": 1,
                    "unread_count": 1,
                    "latest_message_date": "Tue, 12 May 2026 12:00:00 +0000",
                    "freshness": "fresh",
                    "indexed_at": "2026-05-12T12:00:00Z",
                    "error": null
                },
                {
                    "account_id": "acct-bad",
                    "account_username": "acct-bad@example.test",
                    "account_display_name": null,
                    "folder": "INBOX",
                    "ok": false,
                    "message_count": 0,
                    "unread_count": 0,
                    "latest_message_date": null,
                    "freshness": "unavailable",
                    "indexed_at": null,
                    "error": "IMAP: login failed"
                }
            ])
        );
        assert_eq!(value["errors"][0]["account_id"], "acct-bad");
        assert_eq!(value["errors"][0]["error"], "IMAP: login failed");
    }

    #[test]
    fn unified_response_distinguishes_total_account_failure() {
        let response = build_unified_inbox_response(
            "INBOX".to_string(),
            50,
            Vec::new(),
            vec![
                account_result("acct-a", false, Some("IMAP: auth failed")),
                account_result("acct-b", false, Some("fetch INBOX: unavailable")),
            ],
        );

        assert_eq!(response.status, UnifiedInboxStatus::Error);
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["status"], "error");
        assert_eq!(value["errors"].as_array().unwrap().len(), 2);
    }
}
