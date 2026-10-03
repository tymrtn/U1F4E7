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

use crate::handlers::message_target::{TargetExpectation, has_flag, verify_target};
use crate::mailbox_sync::{AccountSyncOutcome, SyncReport, SyncTarget, sync_accounts};
use crate::state::AppState;

pub(crate) const UNIFIED_INBOX_FOLDER: &str = "INBOX";
/// `detected_folders.folder_type` key for the Sent smart mailbox: each
/// account's real Sent folder name is resolved (and cached) per provider, so
/// no single literal folder name exists — index reads join through the
/// detection table instead.
const SENT_FOLDER_TYPE: &str = "sent";
/// Response `folder` label for the cross-account Sent surface, where each row
/// carries its own real per-account folder.
const SENT_SCOPE_LABEL: &str = "sent";

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
    /// Keyset cursor (all three or none): continue after this row.
    pub before_epoch: Option<i64>,
    pub before_uid: Option<u32>,
    pub before_account: Option<String>,
}

/// Query for the sync (refresh) endpoints. `account_id` scopes the provider
/// pass to one account (id or address); the response is still the whole view.
#[derive(Deserialize)]
pub struct SyncQuery {
    #[serde(default = "default_limit")]
    pub limit: u32,
    pub account_id: Option<String>,
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
    /// Parsed message date (unix seconds) from the index; drives the keyset
    /// pagination cursor. None when the header date was unreadable.
    pub date_epoch: Option<i64>,
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
            date_epoch: indexed.date_epoch,
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
            // The last sync failed: cached rows are still shown, as stale.
            (false, failed_freshness(message_count), Some(error))
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
}

/// Freshness of an account whose last sync failed: its cached rows are stale
/// when it has any, and there is nothing to show when it has none.
fn failed_freshness(message_count: usize) -> UnifiedAccountFreshness {
    if message_count > 0 {
        UnifiedAccountFreshness::Stale
    } else {
        UnifiedAccountFreshness::Unavailable
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
    /// Present when the page is full: pass these back as `before_*` query
    /// params to continue exactly where this page ended.
    pub next_cursor: Option<UnifiedNextCursor>,
    /// When the index was read (server clock). Clients drop any response
    /// older than the view they already show.
    pub generated_at: String,
    /// Present only on sync (refresh) responses: what the provider pass did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnifiedNextCursor {
    pub date_epoch: Option<i64>,
    pub uid: u32,
    pub account_id: String,
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
    let cursor = match (&q.before_epoch, q.before_uid, &q.before_account) {
        (_, Some(uid), Some(account_id)) => {
            Some(envelope_email_store::message_index::UnifiedPageCursor {
                date_epoch: q.before_epoch,
                uid,
                account_id: account_id.clone(),
            })
        }
        _ => None,
    };
    let (messages, account_results) = match load_indexed_unified_inbox(
        &state,
        &accounts,
        &folder,
        q.limit,
        cursor.as_ref(),
    )
    .await
    {
        Ok(indexed) => indexed,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    Json(build_inbox_response(
        "unified_inbox",
        folder,
        q.limit,
        messages,
        account_results,
    ))
    .into_response()
}

/// One account's index refresh: connect, EXAMINE, fetch summaries read-only,
/// upsert into the local index. Returns the failure string on any step; the
/// sync flight persists errors and callers build the response uniformly.
pub(crate) async fn refresh_one_account(
    state: AppState,
    account: Account,
    folder: String,
    limit: u32,
) -> Result<(), String> {
    let (client_arc, _creds) = state
        .get_or_create_imap(&account.id)
        .await
        .map_err(|e| format!("IMAP: {e}"))?;
    let mut client = client_arc.lock().await;
    let uidvalidity =
        match envelope_email_transport::imap::examine_folder_info(&mut client, &folder).await {
            Ok(info) => info.uid_validity.unwrap_or(0) as u64,
            Err(e) => {
                state.evict_imap(&account.id).await;
                return Err(format!("EXAMINE {folder}: {e}"));
            }
        };
    match envelope_email_transport::imap::fetch_folder_summaries_read_only(
        &mut client,
        &folder,
        limit,
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
            match write_result {
                Ok(()) => {
                    crate::handlers::address_book::catch_up_account(&state, &account.id).await;
                    Ok(())
                }
                Err(e) => Err(format!("index {folder}: {e}")),
            }
        }
        Err(e) => {
            state.evict_imap(&account.id).await;
            Err(format!("fetch {folder}: {e}"))
        }
    }
}

/// One account's Sent index refresh: resolve the account's real Sent folder
/// (cached in `detected_folders`), then reuse the standard mailbox index
/// refresh against it. Resolution failure is an account-level error — never
/// fall back to a literal "Sent" guess (the account may genuinely have none).
pub(crate) async fn refresh_one_account_sent(
    state: AppState,
    account: Account,
    limit: u32,
) -> Result<(), String> {
    let (client_arc, _creds) = state
        .get_or_create_imap(&account.id)
        .await
        .map_err(|e| format!("IMAP: {e}"))?;
    let folder = {
        let mut client = client_arc.lock().await;
        match resolve_canonical_folder(&state, &mut client, &account.id, SENT_FOLDER_TYPE).await {
            Ok(folder) => folder,
            Err(e) => {
                state.evict_imap(&account.id).await;
                return Err(format!("resolve sent folder: {e}"));
            }
        }
    };
    let Some(folder) = folder else {
        return Err("no Sent folder detected on this account".to_string());
    };
    refresh_one_account(state, account, folder, limit).await
}

/// GET /api/messages/sent — the cross-account Sent list served from the local
/// index (no IMAP on the read path). Populated by the hourly background sweep
/// and POST /api/messages/sent/refresh.
pub async fn sent_inbox(
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

    let cursor = match (&q.before_epoch, q.before_uid, &q.before_account) {
        (_, Some(uid), Some(account_id)) => {
            Some(envelope_email_store::message_index::UnifiedPageCursor {
                date_epoch: q.before_epoch,
                uid,
                account_id: account_id.clone(),
            })
        }
        _ => None,
    };
    let (messages, account_results) =
        match load_indexed_sent(&state, &accounts, q.limit, cursor.as_ref()).await {
            Ok(indexed) => indexed,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        };

    Json(build_inbox_response(
        "sent",
        SENT_SCOPE_LABEL.to_string(),
        q.limit,
        messages,
        account_results,
    ))
    .into_response()
}

/// Run one read-only provider sync for a view: every account, or only the one
/// named by `account_id`. Returns all accounts (the view is always whole) and
/// the per-account outcomes of the accounts actually synced.
async fn run_view_sync(
    state: &AppState,
    target: SyncTarget,
    q: &SyncQuery,
) -> Result<(Vec<Account>, SyncReport, Vec<AccountSyncOutcome>), axum::response::Response> {
    let accounts = {
        let db = state.db.lock().await;
        match db.list_accounts() {
            Ok(accounts) => accounts,
            Err(e) => {
                return Err(
                    (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response(),
                );
            }
        }
    };
    let targets: Vec<Account> = match q.account_id.as_deref() {
        None => accounts.clone(),
        Some(wanted) => {
            let found: Vec<Account> = accounts
                .iter()
                .filter(|a| a.id == wanted || a.username.eq_ignore_ascii_case(wanted))
                .cloned()
                .collect();
            if found.is_empty() {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(json!({
                        "code": "account_not_found",
                        "error": "account_not_found",
                        "message": "No such account to sync.",
                    })),
                )
                    .into_response());
            }
            found
        }
    };
    let scoped_id = q.account_id.as_ref().map(|_| targets[0].id.clone());
    let started_at = chrono::Utc::now().to_rfc3339();
    let outcomes = sync_accounts(state, targets, target, q.limit, state.sync_limits).await;
    let report = SyncReport::new(target, scoped_id, started_at, &outcomes);
    Ok((accounts, report, outcomes))
}

/// POST /api/messages/sent/refresh — read-only Sent sync (all accounts, or one
/// via `account_id`), then serve the refreshed index.
pub async fn refresh_sent_inbox(
    State(state): State<AppState>,
    Query(q): Query<SyncQuery>,
) -> impl IntoResponse {
    let (accounts, report, _outcomes) = match run_view_sync(&state, SyncTarget::Sent, &q).await {
        Ok(synced) => synced,
        Err(response) => return response,
    };

    let (mut messages, mut account_results) =
        match load_indexed_sent(&state, &accounts, q.limit, None).await {
            Ok(indexed) => indexed,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        };
    apply_sync_failures(&mut messages, &mut account_results, &report);

    let mut response = build_inbox_response(
        "sent",
        SENT_SCOPE_LABEL.to_string(),
        q.limit,
        messages,
        account_results,
    );
    response.sync = Some(report);
    Json(response).into_response()
}

/// Hourly background pass that keeps the Sent index warm, so opening the Sent
/// box reads the local cache instead of fanning 25 IMAP round-trips from the
/// browser. Lower concurrency and a generous per-account budget: this runs on
/// a timer, never against a spinner. It shares sync flights with the manual
/// path, so a sweep and a Sync now click never double-fetch an account.
pub async fn run_sent_index_sweep(state: &AppState) -> anyhow::Result<()> {
    let accounts = {
        let db = state.db.lock().await;
        db.list_accounts()?
    };
    if accounts.is_empty() {
        return Ok(());
    }

    const SWEEP_LIMITS: crate::mailbox_sync::SyncLimits = crate::mailbox_sync::SyncLimits {
        concurrency: 4,
        account_timeout: std::time::Duration::from_secs(30),
    };
    const SWEEP_LIMIT: u32 = 50;

    for outcome in sync_accounts(state, accounts, SyncTarget::Sent, SWEEP_LIMIT, SWEEP_LIMITS).await
    {
        if let Err(error) = outcome.result {
            tracing::warn!("sent index sweep [{}]: {error}", outcome.account.username);
        }
    }
    Ok(())
}

async fn load_indexed_sent(
    state: &AppState,
    accounts: &[Account],
    limit: u32,
    cursor: Option<&envelope_email_store::message_index::UnifiedPageCursor>,
) -> Result<(Vec<UnifiedInboxMessage>, Vec<UnifiedInboxAccountResult>), String> {
    let db = state.db.lock().await;
    let indexed = db
        .list_indexed_detected_folder_page(SENT_FOLDER_TYPE, limit, cursor)
        .map_err(|e| format!("db error: {e}"))?;
    let freshness = db
        .list_message_index_detected_folder_freshness(SENT_FOLDER_TYPE)
        .map_err(|e| format!("db error: {e}"))?;

    let mut messages: Vec<UnifiedInboxMessage> = indexed
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
                account_freshness
                    .map(|row| row.folder.as_str())
                    .unwrap_or(SENT_SCOPE_LABEL),
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
        .collect::<Vec<_>>();
    mark_failed_account_rows_stale(&mut messages, &account_results);

    Ok((messages, account_results))
}

/// POST /api/messages/unified/refresh — read-only Inbox sync (all accounts, or
/// one via `account_id`), then serve the refreshed unified index.
pub async fn refresh_unified_inbox(
    State(state): State<AppState>,
    Query(q): Query<SyncQuery>,
) -> impl IntoResponse {
    let (accounts, report, outcomes) = match run_view_sync(&state, SyncTarget::Inbox, &q).await {
        Ok(synced) => synced,
        Err(response) => return response,
    };

    let folder = UNIFIED_INBOX_FOLDER.to_string();
    let (mut messages, mut account_results) =
        match load_indexed_unified_inbox(&state, &accounts, &folder, q.limit, None).await {
            Ok(indexed) => indexed,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        };
    apply_sync_failures(&mut messages, &mut account_results, &report);

    // Publish a metadata-level `new_mail` event for each account THIS request
    // synced (a joined run was announced by the request that started it; an
    // account outside the scope was not synced at all). It carries only
    // post-sync counts — no bodies, subjects or recipients. Clients treat it
    // as "this account's index may have changed — reload the cached view".
    for outcome in outcomes
        .iter()
        .filter(|outcome| outcome.result.is_ok() && !outcome.joined)
    {
        if let Some(result) = account_results
            .iter()
            .find(|result| result.account_id == outcome.account.id)
        {
            state
                .events
                .publish(crate::events::DashboardEvent::NewMail {
                    account_id: result.account_id.clone(),
                    message_count: result.message_count,
                    unread_count: result.unread_count,
                });
        }
    }

    let mut response =
        build_inbox_response("unified_inbox", folder, q.limit, messages, account_results);
    response.sync = Some(report);
    Json(response).into_response()
}

/// Apply this sync's failures to the response in memory, independent of
/// whether the flight's error marker persisted. A failed account keeps its
/// cached rows, now labeled stale, and reports the error; nothing is dropped
/// and nothing is reported as an empty success.
fn apply_sync_failures(
    messages: &mut [UnifiedInboxMessage],
    account_results: &mut [UnifiedInboxAccountResult],
    report: &SyncReport,
) {
    for failure in report.accounts.iter().filter(|a| !a.ok) {
        if let Some(slot) = account_results
            .iter_mut()
            .find(|result| result.account_id == failure.account_id)
        {
            slot.ok = false;
            slot.freshness = failed_freshness(slot.message_count);
            slot.error = failure.error.clone();
        }
    }
    mark_failed_account_rows_stale(messages, account_results);
}

/// Rows of an account whose last sync failed are served, but as stale.
fn mark_failed_account_rows_stale(
    messages: &mut [UnifiedInboxMessage],
    account_results: &[UnifiedInboxAccountResult],
) {
    for message in messages.iter_mut() {
        if account_results
            .iter()
            .any(|result| result.account_id == message.account_id && result.error.is_some())
        {
            message.index_freshness = "stale".to_string();
        }
    }
}

async fn load_indexed_unified_inbox(
    state: &AppState,
    accounts: &[Account],
    folder: &str,
    limit: u32,
    cursor: Option<&envelope_email_store::message_index::UnifiedPageCursor>,
) -> Result<(Vec<UnifiedInboxMessage>, Vec<UnifiedInboxAccountResult>), String> {
    let db = state.db.lock().await;
    let indexed = db
        .list_indexed_message_summaries_page(folder, limit, cursor)
        .map_err(|e| format!("db error: {e}"))?;
    let freshness = db
        .list_message_index_account_freshness(folder)
        .map_err(|e| format!("db error: {e}"))?;

    let mut messages: Vec<UnifiedInboxMessage> = indexed
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
        .collect::<Vec<_>>();
    mark_failed_account_rows_stale(&mut messages, &account_results);

    Ok((messages, account_results))
}

fn build_inbox_response(
    scope: &'static str,
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

    let messages = merge_unified_messages(messages, limit);
    let next_cursor = if messages.len() == limit as usize {
        messages.last().map(|last| UnifiedNextCursor {
            date_epoch: last.date_epoch,
            uid: last.summary.uid,
            account_id: last.account_id.clone(),
        })
    } else {
        None
    };
    UnifiedInboxResponse {
        scope,
        status,
        folder,
        limit,
        messages,
        accounts,
        unread_count,
        freshness,
        errors,
        next_cursor,
        generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        sync: None,
    }
}

fn unified_inbox_freshness(
    accounts: &[UnifiedInboxAccountResult],
    status: UnifiedInboxStatus,
) -> UnifiedAccountFreshness {
    match status {
        UnifiedInboxStatus::Empty => UnifiedAccountFreshness::Empty,
        UnifiedInboxStatus::Partial => UnifiedAccountFreshness::Partial,
        // Every account failed; whatever cached rows exist are stale.
        UnifiedInboxStatus::Error => {
            if accounts
                .iter()
                .any(|account| account.freshness == UnifiedAccountFreshness::Stale)
            {
                UnifiedAccountFreshness::Stale
            } else {
                UnifiedAccountFreshness::Unavailable
            }
        }
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
        let (client_arc, creds) = match state.get_or_create_imap(&account_id).await {
            Ok(c) => c,
            Err(e) => {
                return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response();
            }
        };
        let fetched = {
            let mut client = client_arc.lock().await;
            envelope_email_transport::imap::fetch_message_with_raw(&mut client, &q.folder, uid)
                .await
        };

        match fetched {
            Ok(Some((msg, raw))) => {
                let thread_context =
                    thread_context_for_uid(&state, &account_id, &q.folder, uid).await;
                // Scan on open when there is no current verdict. A failed scan
                // is reported to the reader, never shown as a clean message.
                let threat = {
                    let db = state.db.lock().await;
                    match envelope_email_transport::threat::ThreatConfig::load().and_then(
                        |config| {
                            crate::handlers::threat::verdict_for_open(
                                &db,
                                &account_id,
                                &creds.account.username,
                                &q.folder,
                                uid,
                                raw.as_deref(),
                                &config,
                            )
                        },
                    ) {
                        Ok(view) => json!(view),
                        Err(e) => json!({"level": "unavailable", "error": format!("{e:#}")}),
                    }
                };
                let attachment_blocks = {
                    let db = state.db.lock().await;
                    attachment_blocks_view(&db, &account_id, &msg, raw.as_deref())
                };
                let message = DashboardMessage {
                    unread: message_is_unread(&msg),
                    message: msg,
                    thread_context,
                };
                return Json(json!({
                    "message": message,
                    "threat": threat,
                    "attachment_blocks": attachment_blocks,
                }))
                .into_response();
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

/// Attachments the download route would refuse, so the reader can show them
/// as blocked with the reason instead of a link that fails. Asks the same gate
/// as the download chokepoint. Without whole-message bytes (a message read
/// part by part) it gates on the metadata alone. If the gate itself fails,
/// every attachment is reported blocked with that error: the download route
/// would refuse them too.
fn attachment_blocks_view(
    db: &envelope_email_store::Database,
    account_id: &str,
    msg: &Message,
    raw: Option<&[u8]>,
) -> Vec<serde_json::Value> {
    use envelope_email_transport::threat::persist;
    let blocked = match raw {
        Some(raw) => persist::blocked_attachments(db, account_id, raw),
        None => msg
            .attachments
            .iter()
            .filter_map(|a| {
                persist::attachment_block(
                    db,
                    account_id,
                    msg.message_id.as_deref(),
                    None,
                    &a.filename,
                    &a.content_type,
                    &[],
                )
                .map(|block| block.map(|b| (a.filename.clone(), b)))
                .transpose()
            })
            .collect(),
    };
    match blocked {
        Ok(blocked) => blocked
            .into_iter()
            .map(|(filename, block)| {
                json!({ "filename": filename, "code": block.code, "reason": block.reason })
            })
            .collect(),
        Err(e) => msg
            .attachments
            .iter()
            .map(|a| {
                json!({
                    "filename": a.filename,
                    "code": persist::ATTACHMENT_BLOCKED,
                    "reason": format!("threat check failed: {e:#}"),
                })
            })
            .collect(),
    }
}

#[derive(Deserialize)]
pub struct FlagsRequest {
    #[serde(default = "default_folder")]
    pub folder: String,
    #[serde(default)]
    pub add: Vec<String>,
    #[serde(default)]
    pub remove: Vec<String>,
    /// UIDVALIDITY the client saw when it rendered the row; a reset refuses.
    #[serde(default)]
    pub uidvalidity: Option<u32>,
    /// Message-ID the client saw at this UID; a different message refuses.
    #[serde(default)]
    pub message_id: Option<String>,
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

    let expect = TargetExpectation {
        uidvalidity: req.uidvalidity,
        message_id: req.message_id.clone(),
    };
    if let Err(refusal) =
        verify_target(&state, &mut client, &account_id, &req.folder, uid, &expect).await
    {
        return refusal;
    }

    for flag in &req.add {
        if let Err(e) =
            envelope_email_transport::imap::set_flag(&mut client, &req.folder, uid, flag).await
        {
            state.evict_imap(&account_id).await;
            return (StatusCode::BAD_GATEWAY, format!("set_flag {flag}: {e}")).into_response();
        }
        let patched = {
            let db = state.db.lock().await;
            envelope_email_transport::imap::record_own_flag_change(
                &db,
                &account_id,
                &req.folder,
                &[uid],
                flag,
                true,
            )
        };
        if let Err(e) = patched {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("set_flag {flag}: changed on the server, but updating the local message index failed: {e}"),
            )
                .into_response();
        }
    }
    for flag in &req.remove {
        if let Err(e) =
            envelope_email_transport::imap::remove_flag(&mut client, &req.folder, uid, flag).await
        {
            state.evict_imap(&account_id).await;
            return (StatusCode::BAD_GATEWAY, format!("remove_flag {flag}: {e}")).into_response();
        }
        let patched = {
            let db = state.db.lock().await;
            envelope_email_transport::imap::record_own_flag_change(
                &db,
                &account_id,
                &req.folder,
                &[uid],
                flag,
                false,
            )
        };
        if let Err(e) = patched {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("remove_flag {flag}: changed on the server, but updating the local message index failed: {e}"),
            )
                .into_response();
        }
    }

    // Read the flags back so the client renders the server's state, not its
    // own guess. The STOREs above already succeeded: a failed read-back is
    // reported as unconfirmed, never as a failed write.
    let confirmed = envelope_email_transport::imap::probe_uid(&mut client, &req.folder, uid)
        .await
        .ok()
        .and_then(|probe| probe.flags);
    Json(flags_response(
        uid,
        &req.add,
        &req.remove,
        confirmed.as_deref(),
    ))
    .into_response()
}

/// Response for a completed flag change. `flags`/`seen`/`flagged` are the
/// server's read-back; all three are null when the read-back failed.
fn flags_response(
    uid: u32,
    added: &[String],
    removed: &[String],
    confirmed: Option<&[String]>,
) -> serde_json::Value {
    json!({
        "ok": true,
        "uid": uid,
        "added": added,
        "removed": removed,
        "confirmed": confirmed.is_some(),
        "flags": confirmed,
        "seen": confirmed.map(|f| has_flag(f, "seen")),
        "flagged": confirmed.map(|f| has_flag(f, "flagged")),
    })
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
    #[serde(default)]
    pub uidvalidity: Option<u32>,
    #[serde(default)]
    pub message_id: Option<String>,
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

    // 4–5. Provider-resolved name, else any known variant — both verified to
    // exist on the server.
    let picked = pick_canonical_folder(provider, canonical_type, &folders);
    if let Some(name) = &picked {
        let db = state.db.lock().await;
        let _ = db.set_detected_folder(account_id, canonical_type, name);
    }
    Ok(picked)
}

/// Pure folder choice for a canonical type against a real folder inventory:
/// the provider's own name when it exists, else the first known variant that
/// exists, else `None`. Never returns a folder the server did not list.
pub(crate) fn pick_canonical_folder(
    provider: envelope_email_transport::provider::ProviderType,
    canonical_type: &str,
    folders: &[String],
) -> Option<String> {
    use envelope_email_transport::provider::{self, ProviderType};
    if provider != ProviderType::Unknown {
        let resolved = provider::resolve_folder(provider, canonical_type);
        if folders.iter().any(|f| f == resolved) {
            return Some(resolved.to_string());
        }
    }
    provider::all_candidates_for(canonical_type)
        .iter()
        .find(|candidate| folders.iter().any(|f| f == *candidate))
        .map(|c| c.to_string())
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

    let expect = TargetExpectation {
        uidvalidity: req.uidvalidity,
        message_id: req.message_id.clone(),
    };
    let probe =
        match verify_target(&state, &mut client, &account_id, &req.folder, uid, &expect).await {
            Ok(p) => p,
            Err(refusal) => return refusal,
        };

    // The destination's UIDNEXT before the move bounds where the moved copy
    // can land, so finding it afterwards reads only what the move added.
    let dest_before = envelope_email_transport::imap::select_folder_info(&mut client, &to_folder)
        .await
        .ok();

    if let Err(e) =
        envelope_email_transport::imap::move_message(&mut client, uid, &req.folder, &to_folder)
            .await
    {
        state.evict_imap(&account_id).await;
        return (StatusCode::BAD_GATEWAY, format!("move: {e}")).into_response();
    }

    // The message has left the source folder: drop it from the local index so
    // a cache-first reload shows the server's state instead of a ghost row.
    {
        let db = state.db.lock().await;
        if let Err(e) = db.forget_indexed_message(&account_id, &req.folder, uid) {
            tracing::warn!("move: moved on the server, but index cleanup failed: {e}");
        }
    }

    // Name the message's new home exactly, so the client can offer "Move
    // back" against a real UID. The move already succeeded; a failed lookup
    // only means no exact handle, reported as null.
    let (moved_uid, moved_uidvalidity) = match dest_before {
        Some(before) => {
            locate_moved(&mut client, &to_folder, probe.message_id.as_deref(), before).await
        }
        None => (None, None),
    };
    Json(json!({
        "ok": true,
        "uid": uid,
        "from_folder": req.folder,
        "moved_to": to_folder,
        "moved_uid": moved_uid,
        "moved_uidvalidity": moved_uidvalidity,
    }))
    .into_response()
}

/// Exact `(UID, UIDVALIDITY)` of a just-moved message in `folder`, by unique
/// Message-ID among UIDs at or above the pre-move UIDNEXT. `(None, None)` when
/// the message has no Message-ID, the mailbox was reset during the move, the
/// match is not unique, or the lookup failed.
async fn locate_moved(
    client: &mut envelope_email_transport::ImapClient,
    folder: &str,
    message_id: Option<&str>,
    before: envelope_email_transport::imap::SelectedMailbox,
) -> (Option<u32>, Option<u32>) {
    let (Some(mid), Some(uid_next)) = (message_id, before.uid_next) else {
        return (None, None);
    };
    let uid = envelope_email_transport::imap::find_unique_uid_by_message_id_from(
        client, folder, mid, uid_next,
    )
    .await
    .ok()
    .flatten();
    let Some(uid) = uid else {
        return (None, None);
    };
    // The lookup just SELECTed `folder`; its UIDVALIDITY must still be the
    // one the floor was read under, or the UID means nothing.
    match envelope_email_transport::imap::select_folder_info(client, folder).await {
        Ok(now) if now.uid_validity.is_some() && now.uid_validity == before.uid_validity => {
            (Some(uid), now.uid_validity)
        }
        _ => (None, None),
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
    #[serde(default)]
    pub uidvalidity: Option<u32>,
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

    let expect = TargetExpectation {
        uidvalidity: req.uidvalidity,
        message_id: req.message_id.clone(),
    };
    let probe =
        match verify_target(&state, &mut client, &account_id, &req.folder, uid, &expect).await {
            Ok(p) => p,
            Err(refusal) => return refusal,
        };
    // The Message-ID is how the sweep and Unsnooze find the message again in
    // the Snoozed folder, where it has a new UID. Without one there is no
    // exact way back, so refuse before moving anything.
    let Some(message_id) = probe.message_id.clone() else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "code": "message_id_missing",
                "reason": "this message has no Message-ID, so a snooze could not find it again; archive or flag it instead",
            })),
        )
            .into_response();
    };

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
    if let Err(e) = db.forget_indexed_message(&account_id, &req.folder, uid) {
        tracing::warn!("snooze: moved on the server, but index cleanup failed: {e}");
    }
    match db.create_snoozed(
        &account_id,
        uid,
        &req.folder,
        SNOOZED_FOLDER,
        &return_at,
        Some(&message_id),
        req.subject.as_deref(),
        Some("dashboard"),
        None,
        None,
    ) {
        Ok(record) => Json(json!({
            "ok": true,
            "id": record.id,
            "uid": uid,
            "original_folder": record.original_folder,
            "return_at": crate::handlers::snoozed::utc_rfc3339(&record.return_at),
            "snoozed_folder": SNOOZED_FOLDER,
            "message_id": message_id,
        }))
        .into_response(),
        // The message is already in Snoozed. Say exactly that, so the client
        // never reports the move itself as failed.
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "code": "snooze_record_failed",
                "error": format!("moved to {SNOOZED_FOLDER}, but recording the return time failed: {e}"),
            })),
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

    fn inventory(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn junk_resolves_to_each_providers_real_spam_folder() {
        use envelope_email_transport::provider::{ProviderType, detect_provider};
        let cases: &[(&str, &[&str], &str)] = &[
            (
                "gmail",
                &["INBOX", "[Gmail]/Spam", "[Gmail]/Trash", "[Gmail]/All Mail"],
                "[Gmail]/Spam",
            ),
            (
                "exchange/workmail",
                &["INBOX", "Deleted Items", "Junk E-mail", "Sent Items"],
                "Junk E-mail",
            ),
            (
                "microsoft 365",
                &["INBOX", "Deleted Items", "Junk Email", "Sent Items"],
                "Junk Email",
            ),
            (
                "generic (Migadu)",
                &["INBOX", "Junk", "Trash", "Archive", "Sent"],
                "Junk",
            ),
            (
                "dovecot",
                &["INBOX", "INBOX.Junk", "INBOX.Trash"],
                "INBOX.Junk",
            ),
            ("spam-named", &["INBOX", "Spam", "Trash"], "Spam"),
        ];
        for (label, names, want) in cases {
            let folders = inventory(names);
            let provider = detect_provider(&folders);
            assert_eq!(
                pick_canonical_folder(provider, "spam", &folders).as_deref(),
                Some(*want),
                "{label}"
            );
            // Unknown provider still lands on a real, listed folder.
            assert_eq!(
                pick_canonical_folder(ProviderType::Unknown, "spam", &folders).as_deref(),
                Some(*want),
                "{label} (unknown provider)"
            );
        }
    }

    #[test]
    fn junk_with_no_spam_folder_resolves_to_nothing_rather_than_a_literal() {
        let folders = inventory(&["INBOX", "Trash", "Sent"]);
        let provider = envelope_email_transport::provider::detect_provider(&folders);
        assert_eq!(pick_canonical_folder(provider, "spam", &folders), None);
    }

    #[test]
    fn flags_response_reports_the_server_read_back() {
        let flags = vec!["Seen".to_string(), "Flagged".to_string()];
        let body = flags_response(5, &["\\Flagged".into()], &[], Some(&flags));
        assert_eq!(body["confirmed"], true);
        assert_eq!(body["seen"], true);
        assert_eq!(body["flagged"], true);
    }

    #[test]
    fn flags_response_after_failed_read_back_is_unconfirmed_not_failed() {
        let body = flags_response(5, &[], &["\\Seen".into()], None);
        assert_eq!(body["ok"], true, "the STORE succeeded");
        assert_eq!(body["confirmed"], false);
        assert!(body["seen"].is_null());
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
            date_epoch: None,
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

    // ── pagination cursor (sweep blocker #4) ────────────────────────────

    #[test]
    fn full_page_carries_a_next_cursor_from_its_last_row() {
        let mut m1 = unified("acct-a", 10, Some("Tue, 12 May 2026 12:00:00 +0000"), 0);
        m1.date_epoch = Some(1_000_000);
        let mut m2 = unified("acct-b", 3, Some("Tue, 12 May 2026 11:00:00 +0000"), 1);
        m2.date_epoch = Some(999_000);
        let res = build_inbox_response(
            "unified_inbox",
            "INBOX".to_string(),
            2,
            vec![m1, m2],
            vec![
                account_result("acct-a", true, None),
                account_result("acct-b", true, None),
            ],
        );
        let cursor = res.next_cursor.expect("full page implies more may exist");
        assert_eq!(cursor.date_epoch, Some(999_000));
        assert_eq!(cursor.uid, 3);
        assert_eq!(cursor.account_id, "acct-b");
    }

    #[test]
    fn short_page_has_no_next_cursor() {
        let m1 = unified("acct-a", 10, Some("Tue, 12 May 2026 12:00:00 +0000"), 0);
        let res = build_inbox_response(
            "unified_inbox",
            "INBOX".to_string(),
            50,
            vec![m1],
            vec![account_result("acct-a", true, None)],
        );
        assert!(res.next_cursor.is_none());
    }

    // Bounded fan-out + timeouts are tested in `mailbox_sync`.

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
    fn apply_sync_failures_keeps_rows_as_stale_even_without_persisted_marker() {
        // Simulate a sync whose DB error-marker write was rejected: the DB
        // query still reports the failed account as ok/fresh. The in-memory
        // failure must still mark it failed and its rows stale — and keep the
        // rows, never swapping them for an empty success (#171).
        let mut messages = vec![
            unified("acct-ok", 10, Some("Tue, 12 May 2026 10:00:00 +0000"), 0),
            unified(
                "acct-failed",
                20,
                Some("Thu, 09 Jul 2026 08:42:00 +0000"),
                1,
            ),
        ];
        let mut failed_cached = account_result("acct-failed", true, None);
        failed_cached.message_count = 1;
        let mut account_results = vec![account_result("acct-ok", true, None), failed_cached];
        let report = SyncReport {
            target: SyncTarget::Inbox,
            account_id: None,
            status: crate::mailbox_sync::SyncStatus::Partial,
            started_at: String::new(),
            finished_at: String::new(),
            accounts: vec![
                crate::mailbox_sync::AccountSyncResult {
                    account_id: "acct-ok".into(),
                    account_username: "acct-ok@example.test".into(),
                    ok: true,
                    joined: false,
                    error: None,
                },
                crate::mailbox_sync::AccountSyncResult {
                    account_id: "acct-failed".into(),
                    account_username: "acct-failed@example.test".into(),
                    ok: false,
                    joined: false,
                    error: Some("IMAP: auth failed".into()),
                },
            ],
        };

        apply_sync_failures(&mut messages, &mut account_results, &report);

        assert_eq!(messages.len(), 2, "no cached row is dropped");
        let failed_row = messages
            .iter()
            .find(|m| m.account_id == "acct-failed")
            .unwrap();
        assert_eq!(failed_row.index_freshness, "stale");
        let ok_row = messages.iter().find(|m| m.account_id == "acct-ok").unwrap();
        assert_ne!(ok_row.index_freshness, "stale");

        let failed = account_results
            .iter()
            .find(|result| result.account_id == "acct-failed")
            .expect("failed account result");
        assert!(!failed.ok);
        assert_eq!(failed.freshness, UnifiedAccountFreshness::Stale);
        assert_eq!(failed.message_count, 1);
        assert_eq!(failed.error.as_deref(), Some("IMAP: auth failed"));

        let healthy = account_results
            .iter()
            .find(|result| result.account_id == "acct-ok")
            .expect("healthy account result");
        assert!(healthy.ok);

        let response = build_inbox_response(
            "unified_inbox",
            "INBOX".into(),
            50,
            messages,
            account_results,
        );
        assert_eq!(response.status, UnifiedInboxStatus::Partial);
        assert_eq!(response.freshness, UnifiedAccountFreshness::Partial);
        assert_eq!(response.errors.len(), 1);
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

        let (messages, accounts) = load_indexed_unified_inbox(&state, &accounts, "INBOX", 10, None)
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

        let (messages, accounts) = load_indexed_unified_inbox(&state, &accounts, "INBOX", 10, None)
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

        let (messages, accounts) = load_indexed_unified_inbox(&state, &accounts, "INBOX", 0, None)
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
        let response = build_inbox_response(
            "unified_inbox",
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
        let response = build_inbox_response(
            "unified_inbox",
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
        let response = build_inbox_response(
            "unified_inbox",
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
        let response = build_inbox_response(
            "unified_inbox",
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
