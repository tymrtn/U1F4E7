// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Read-only mailbox sync (#171): the only dashboard path that pulls provider
//! state into the local message index.
//!
//! Every provider pass runs through [`sync_accounts`], which gives it three
//! guarantees the list surfaces rely on:
//!
//! - **Single flight per account and mailbox.** A second request for an
//!   account whose sync is already running (another tab, an SSE-driven reload,
//!   the hourly sweep, a double click) joins that run instead of opening a
//!   second IMAP pass. The run is a spawned task, so a caller that disconnects
//!   does not cancel it for the others.
//! - **Bounded.** A fixed number of accounts at once, each under its own time
//!   budget; a timed-out account's pooled connection is evicted because the
//!   dropped future may have left it mid-command.
//! - **Truthful failure.** A failed or timed-out account gets an error marker
//!   on its index row, which reports its cached rows as stale. The rows are
//!   never deleted by a failure.
//!
//! The provider work itself is the injected [`AccountSyncer`]; production uses
//! [`imap_syncer`], which is `EXAMINE` + `FETCH (… BODY.PEEK[…])` only.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use envelope_email_store::models::Account;
use futures_util::StreamExt;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use serde::Serialize;

use crate::state::AppState;

/// The mailbox a sync pass refreshes on each account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncTarget {
    /// The account's INBOX (the unified Inbox view).
    Inbox,
    /// The account's detected Sent folder (the cross-account Sent view).
    Sent,
}

/// Provider work for one account: fetch the target mailbox read-only and write
/// the local index. Returns the failure text on any step.
pub type AccountSyncer = Arc<
    dyn Fn(AppState, Account, SyncTarget, u32) -> BoxFuture<'static, Result<(), String>>
        + Send
        + Sync,
>;

/// The production syncer: read-only IMAP into the local index.
pub fn imap_syncer() -> AccountSyncer {
    Arc::new(|state, account, target, limit| match target {
        SyncTarget::Inbox => crate::handlers::messages::refresh_one_account(
            state,
            account,
            crate::handlers::messages::UNIFIED_INBOX_FOLDER.to_string(),
            limit,
        )
        .boxed(),
        SyncTarget::Sent => {
            crate::handlers::messages::refresh_one_account_sent(state, account, limit).boxed()
        }
    })
}

/// Fan-out bounds for one sync pass.
#[derive(Debug, Clone, Copy)]
pub struct SyncLimits {
    pub concurrency: usize,
    pub account_timeout: Duration,
}

impl Default for SyncLimits {
    fn default() -> Self {
        Self {
            concurrency: 6,
            account_timeout: Duration::from_secs(10),
        }
    }
}

type Flight = Shared<BoxFuture<'static, Result<(), String>>>;
type FlightKey = (String, SyncTarget);

/// Registry of in-progress account syncs, keyed by account and mailbox.
#[derive(Default)]
pub struct SyncFlights {
    next_id: AtomicU64,
    inflight: StdMutex<HashMap<FlightKey, (u64, Flight)>>,
}

impl SyncFlights {
    /// Number of account syncs currently running.
    pub fn in_flight(&self) -> usize {
        self.inflight
            .lock()
            .expect("sync flight registry poisoned")
            .len()
    }
}

/// What happened to one account in a sync pass.
#[derive(Debug, Clone)]
pub struct AccountSyncOutcome {
    pub account: Account,
    /// True when this request joined a run another caller had already started.
    pub joined: bool,
    pub result: Result<(), String>,
}

/// Sync one account's mailbox, joining the run already in progress for it if
/// there is one.
pub async fn sync_account(
    state: &AppState,
    account: Account,
    target: SyncTarget,
    limit: u32,
    budget: Duration,
) -> AccountSyncOutcome {
    let key: FlightKey = (account.id.clone(), target);
    let (flight, joined) = {
        let mut inflight = state
            .sync_flights
            .inflight
            .lock()
            .expect("sync flight registry poisoned");
        match inflight.get(&key) {
            Some((_, flight)) => (flight.clone(), true),
            None => {
                let id = state.sync_flights.next_id.fetch_add(1, Ordering::Relaxed);
                let task = tokio::spawn(run_flight(
                    state.clone(),
                    account.clone(),
                    target,
                    limit,
                    budget,
                    id,
                ));
                let flight: Flight = async move {
                    task.await
                        .unwrap_or_else(|e| Err(format!("sync task failed: {e}")))
                }
                .boxed()
                .shared();
                inflight.insert(key, (id, flight.clone()));
                (flight, false)
            }
        }
    };
    let result = flight.await;
    AccountSyncOutcome {
        account,
        joined,
        result,
    }
}

async fn run_flight(
    state: AppState,
    account: Account,
    target: SyncTarget,
    limit: u32,
    budget: Duration,
    id: u64,
) -> Result<(), String> {
    let syncer = state.syncer.clone();
    let result = match tokio::time::timeout(
        budget,
        syncer(state.clone(), account.clone(), target, limit),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            // The dropped future may have left the pooled client mid-command.
            state.evict_imap(&account.id).await;
            Err(format!("timed out after {}s", budget.as_secs().max(1)))
        }
    };
    if let Err(error) = &result {
        persist_sync_error(&state, &account.id, target, error).await;
    }
    let key: FlightKey = (account.id.clone(), target);
    let mut inflight = state
        .sync_flights
        .inflight
        .lock()
        .expect("sync flight registry poisoned");
    if inflight.get(&key).is_some_and(|(owner, _)| *owner == id) {
        inflight.remove(&key);
    }
    result
}

/// Mark the account's cached mailbox as failed. Persistence failure only logs:
/// the caller also applies the failure to its response in memory.
async fn persist_sync_error(state: &AppState, account_id: &str, target: SyncTarget, error: &str) {
    let db = state.db.lock().await;
    let folder = match target {
        SyncTarget::Inbox => Some(crate::handlers::messages::UNIFIED_INBOX_FOLDER.to_string()),
        SyncTarget::Sent => db.get_detected_folders(account_id).ok().and_then(|rows| {
            rows.into_iter()
                .find(|(folder_type, _)| folder_type == "sent")
                .map(|(_, name)| name)
        }),
    };
    // An account with no detected Sent folder has no index row to mark.
    let Some(folder) = folder else { return };
    if let Err(e) = db.record_message_index_error(account_id, &folder, error) {
        tracing::warn!(
            "mailbox sync: failed to persist index error for account {account_id} folder {folder}: {e}"
        );
    }
}

/// Sync every account with bounded concurrency. Every account appears exactly
/// once in the result, in completion order.
pub async fn sync_accounts(
    state: &AppState,
    accounts: Vec<Account>,
    target: SyncTarget,
    limit: u32,
    limits: SyncLimits,
) -> Vec<AccountSyncOutcome> {
    futures_util::stream::iter(accounts)
        .map(|account| sync_account(state, account, target, limit, limits.account_timeout))
        .buffer_unordered(limits.concurrency.max(1))
        .collect()
        .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    /// Every requested account synced.
    Ok,
    /// Some accounts synced, some failed.
    Partial,
    /// No requested account synced.
    Error,
    /// There was nothing to sync.
    Empty,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountSyncResult {
    pub account_id: String,
    pub account_username: String,
    pub ok: bool,
    pub joined: bool,
    pub error: Option<String>,
}

/// The provider-sync half of a refresh response. Only a response carrying one
/// with `ok` accounts may be presented as "synced".
#[derive(Debug, Clone, Serialize)]
pub struct SyncReport {
    pub target: SyncTarget,
    /// Set when the request was scoped to one account.
    pub account_id: Option<String>,
    pub status: SyncStatus,
    pub started_at: String,
    pub finished_at: String,
    pub accounts: Vec<AccountSyncResult>,
}

impl SyncReport {
    pub fn new(
        target: SyncTarget,
        account_id: Option<String>,
        started_at: String,
        outcomes: &[AccountSyncOutcome],
    ) -> Self {
        let accounts: Vec<AccountSyncResult> = outcomes
            .iter()
            .map(|outcome| AccountSyncResult {
                account_id: outcome.account.id.clone(),
                account_username: outcome.account.username.clone(),
                ok: outcome.result.is_ok(),
                joined: outcome.joined,
                error: outcome.result.as_ref().err().cloned(),
            })
            .collect();
        let ok = accounts.iter().filter(|a| a.ok).count();
        let status = match (ok, accounts.len() - ok) {
            (0, 0) => SyncStatus::Empty,
            (_, 0) => SyncStatus::Ok,
            (0, _) => SyncStatus::Error,
            _ => SyncStatus::Partial,
        };
        Self {
            target,
            account_id,
            status,
            started_at,
            finished_at: chrono::Utc::now().to_rfc3339(),
            accounts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use envelope_email_store::{CredentialBackend, Database};
    use std::sync::atomic::AtomicUsize;

    fn account(id: &str) -> Account {
        Account {
            id: id.to_string(),
            name: String::new(),
            username: format!("{id}@example.test"),
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
        }
    }

    fn state_with(syncer: AccountSyncer, limits: SyncLimits) -> AppState {
        AppState::new(Database::open_memory().unwrap(), CredentialBackend::File)
            .with_syncer(syncer)
            .with_sync_limits(limits)
    }

    #[tokio::test]
    async fn slow_account_times_out_without_holding_the_pass_hostage() {
        let syncer: AccountSyncer = Arc::new(|_, account, _, _| {
            async move {
                if account.id == "b" {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                Ok(())
            }
            .boxed()
        });
        let state = state_with(
            syncer,
            SyncLimits {
                concurrency: 2,
                account_timeout: Duration::from_millis(300),
            },
        );

        let started = std::time::Instant::now();
        let outcomes = sync_accounts(
            &state,
            vec![account("a"), account("b"), account("c")],
            SyncTarget::Inbox,
            50,
            state.sync_limits,
        )
        .await;

        assert_eq!(outcomes.len(), 3);
        let by_id = |id: &str| outcomes.iter().find(|o| o.account.id == id).unwrap();
        assert!(by_id("a").result.is_ok());
        assert!(by_id("c").result.is_ok());
        let err = by_id("b").result.as_ref().unwrap_err();
        assert!(err.contains("timed out"), "got: {err}");
        assert!(started.elapsed() <= Duration::from_secs(2));
        assert_eq!(state.sync_flights.in_flight(), 0);
    }

    #[tokio::test]
    async fn fan_out_never_exceeds_the_concurrency_bound() {
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let syncer: AccountSyncer = {
            let running = running.clone();
            let peak = peak.clone();
            Arc::new(move |_, _, _, _| {
                let running = running.clone();
                let peak = peak.clone();
                async move {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    running.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                }
                .boxed()
            })
        };
        let state = state_with(
            syncer,
            SyncLimits {
                concurrency: 2,
                account_timeout: Duration::from_secs(5),
            },
        );
        let accounts = (0..6).map(|i| account(&format!("acct-{i}"))).collect();

        let outcomes =
            sync_accounts(&state, accounts, SyncTarget::Inbox, 50, state.sync_limits).await;

        assert_eq!(outcomes.len(), 6);
        assert!(outcomes.iter().all(|o| o.result.is_ok()));
        assert_eq!(peak.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn concurrent_requests_for_one_account_share_a_single_provider_run() {
        let calls = Arc::new(AtomicUsize::new(0));
        let syncer: AccountSyncer = {
            let calls = calls.clone();
            Arc::new(move |_, _, _, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                async {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    Err("IMAP: auth failed".to_string())
                }
                .boxed()
            })
        };
        let state = state_with(syncer, SyncLimits::default());
        let budget = Duration::from_secs(5);

        let (first, second) = tokio::join!(
            sync_account(&state, account("a"), SyncTarget::Inbox, 50, budget),
            sync_account(&state, account("a"), SyncTarget::Inbox, 50, budget),
        );

        assert_eq!(calls.load(Ordering::SeqCst), 1, "one provider run");
        assert_eq!(
            [first.joined, second.joined].iter().filter(|j| **j).count(),
            1
        );
        // The joiner sees the same truthful failure, not an empty success.
        assert_eq!(first.result, Err("IMAP: auth failed".to_string()));
        assert_eq!(second.result, Err("IMAP: auth failed".to_string()));

        // A different mailbox on the same account is a different run.
        sync_account(&state, account("a"), SyncTarget::Sent, 50, budget).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // Once finished, the next request starts a fresh run.
        sync_account(&state, account("a"), SyncTarget::Inbox, 50, budget).await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(state.sync_flights.in_flight(), 0);
    }

    #[tokio::test]
    async fn caller_disconnect_does_not_cancel_the_shared_run() {
        let finished = Arc::new(AtomicUsize::new(0));
        let syncer: AccountSyncer = {
            let finished = finished.clone();
            Arc::new(move |_, _, _, _| {
                let finished = finished.clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    finished.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
                .boxed()
            })
        };
        let state = state_with(syncer, SyncLimits::default());

        let dropped = tokio::time::timeout(
            Duration::from_millis(10),
            sync_account(
                &state,
                account("a"),
                SyncTarget::Inbox,
                50,
                Duration::from_secs(5),
            ),
        )
        .await;
        assert!(dropped.is_err(), "caller gave up early");

        let joined = sync_account(
            &state,
            account("a"),
            SyncTarget::Inbox,
            50,
            Duration::from_secs(5),
        )
        .await;
        assert!(joined.joined, "second caller joins the still-running pass");
        assert!(joined.result.is_ok());
        assert_eq!(finished.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn report_status_distinguishes_partial_from_total_failure() {
        let ok = AccountSyncOutcome {
            account: account("a"),
            joined: false,
            result: Ok(()),
        };
        let failed = AccountSyncOutcome {
            account: account("b"),
            joined: false,
            result: Err("IMAP: auth failed".into()),
        };
        let now = chrono::Utc::now().to_rfc3339();
        let status = |outcomes: &[AccountSyncOutcome]| {
            SyncReport::new(SyncTarget::Inbox, None, now.clone(), outcomes).status
        };
        assert_eq!(status(&[ok.clone()]), SyncStatus::Ok);
        assert_eq!(status(&[ok, failed.clone()]), SyncStatus::Partial);
        assert_eq!(status(&[failed]), SyncStatus::Error);
        assert_eq!(status(&[]), SyncStatus::Empty);
    }

    /// The production syncer's mailbox calls, read from source: sync may only
    /// EXAMINE and fetch summaries with BODY.PEEK. Anything that selects,
    /// stores flags, moves, expunges, appends, sends or runs rules is a
    /// read-only violation (#171, CLAUDE.md evidence invariants).
    #[test]
    fn imap_syncer_path_is_examine_and_peek_only() {
        let source = include_str!("handlers/messages.rs");
        let body = |signature: &str| {
            let start = source
                .find(signature)
                .unwrap_or_else(|| panic!("{signature} not found"));
            let end = source[start..].find("\n}\n").expect("function end") + start;
            &source[start..end]
        };
        let paths = [
            body("pub(crate) async fn refresh_one_account("),
            body("pub(crate) async fn refresh_one_account_sent("),
        ];
        assert!(paths[0].contains("examine_folder_info"));
        assert!(paths[0].contains("fetch_folder_summaries_read_only"));
        for path in paths {
            for forbidden in [
                ".select(",
                "select_folder",
                "store_flags",
                "uid_store",
                "add_flags",
                "remove_flags",
                "set_flags",
                "move_message",
                "uid_mv",
                "expunge",
                "append",
                "delete_message",
                "smtp",
                "send_",
                "rule",
                "snooze",
            ] {
                assert!(
                    !path.contains(forbidden),
                    "sync path must stay read-only; found `{forbidden}`"
                );
            }
        }
        let descriptor = envelope_email_transport::imap::FETCH_SUMMARY_DESCRIPTOR;
        assert!(descriptor.contains("BODY.PEEK["));
        assert!(!descriptor.replace("BODY.PEEK[", "").contains("BODY["));
    }
}
