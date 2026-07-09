// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Bulk message operations with partial-failure semantics.
//!
//! One bulk call resolves a set of target UIDs (either given directly or via an
//! IMAP search), then applies a single operation — move, copy, flag add/remove,
//! delete, or tag — across all of them. A single failing UID never aborts the
//! rest: the return value reports exactly which UIDs succeeded and which failed
//! with a stable machine code.
//!
//! ## Efficiency
//! UIDs are coalesced into IMAP ranges (`1:5,9,12:14`) and chunked so each
//! batched `UID MOVE/COPY/STORE` stays well under the server command-length
//! limit. When a batch fails, that chunk is retried one UID at a time so the
//! blast radius of a bad UID is a single message, not the whole chunk.
//!
//! ## MCP note
//! Every public type here is `serde`-serializable and every entry point is
//! `pub`, so an MCP handler can call [`execute`] directly and re-emit
//! [`BulkResult`] without going through the CLI. [`execute`] takes an already
//! connected [`ImapClient`] plus a [`Database`] handle; it performs no
//! credential resolution and no action logging — the caller owns those (the CLI
//! logs; an MCP handler should log equivalently).

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use envelope_email_store::Database;

use crate::errors::ImapError;
// The small IMAP-formatting helpers (`validate_imap_input`, `imap_mailbox_arg`,
// `map_flag_name`) and the capability-aware `expunge_uids` all live in `imap.rs`
// so there is exactly one implementation shared by the single-message and bulk
// paths.
use crate::imap::{
    self, ImapClient, expunge_uids, imap_mailbox_arg, map_flag_name, validate_imap_input,
};

/// Move every UID in `seq_set` from `from` to `to`: UID COPY, then mark
/// `\Deleted` and scoped-EXPUNGE — mirroring `imap::move_message` exactly.
async fn move_messages(
    client: &mut ImapClient,
    seq_set: &str,
    from: &str,
    to: &str,
) -> Result<(), ImapError> {
    validate_imap_input(from)?;
    validate_imap_input(to)?;
    validate_imap_input(seq_set)?;

    let session = client.session_mut();
    session
        .select(from)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {from}: {e}")))?;

    let quoted_to = imap_mailbox_arg(to);
    session
        .uid_copy(seq_set, &quoted_to)
        .await
        .map_err(|e| ImapError::Protocol(format!("UID COPY {seq_set} to {to}: {e}")))?;

    {
        let mut store_stream = session
            .uid_store(seq_set, "+FLAGS (\\Deleted)")
            .await
            .map_err(|e| {
                ImapError::Protocol(format!("UID STORE +FLAGS \\Deleted {seq_set}: {e}"))
            })?;
        while let Some(_item) = store_stream.next().await {}
    }

    // Scope the expunge to exactly the UIDs we flagged so a per-UID retry never
    // repeatedly nukes another session's \Deleted messages (see expunge_uids).
    expunge_uids(client, seq_set).await?;
    Ok(())
}

/// Copy every UID in `seq_set` from `from` to `to` (UID COPY only).
async fn copy_messages(
    client: &mut ImapClient,
    seq_set: &str,
    from: &str,
    to: &str,
) -> Result<(), ImapError> {
    validate_imap_input(from)?;
    validate_imap_input(to)?;
    validate_imap_input(seq_set)?;

    let session = client.session_mut();
    session
        .select(from)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {from}: {e}")))?;
    let quoted_to = imap_mailbox_arg(to);
    session
        .uid_copy(seq_set, &quoted_to)
        .await
        .map_err(|e| ImapError::Protocol(format!("UID COPY {seq_set} to {to}: {e}")))?;
    Ok(())
}

/// Delete every UID in `seq_set` from `folder`: mark `\Deleted` + EXPUNGE —
/// mirroring `imap::delete_message` (no trash move).
async fn delete_messages(
    client: &mut ImapClient,
    folder: &str,
    seq_set: &str,
) -> Result<(), ImapError> {
    validate_imap_input(folder)?;
    validate_imap_input(seq_set)?;

    let session = client.session_mut();
    session
        .select(folder)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {folder}: {e}")))?;
    {
        let mut store_stream = session
            .uid_store(seq_set, "+FLAGS (\\Deleted)")
            .await
            .map_err(|e| {
                ImapError::Protocol(format!("UID STORE +FLAGS \\Deleted {seq_set}: {e}"))
            })?;
        while let Some(_item) = store_stream.next().await {}
    }

    // Scope the expunge to exactly the UIDs we flagged so a per-UID retry never
    // repeatedly nukes another session's \Deleted messages (see expunge_uids).
    expunge_uids(client, seq_set).await?;
    Ok(())
}

/// Add (`add == true`) or remove a flag across every UID in `seq_set` —
/// mirroring `imap::set_flag` / `imap::remove_flag`.
async fn store_flag_messages(
    client: &mut ImapClient,
    folder: &str,
    seq_set: &str,
    flag: &str,
    add: bool,
) -> Result<(), ImapError> {
    validate_imap_input(folder)?;
    validate_imap_input(seq_set)?;

    let imap_flag = map_flag_name(flag);
    validate_imap_input(&imap_flag)?;
    let op = if add { "+FLAGS" } else { "-FLAGS" };
    let store_query = format!("{op} ({imap_flag})");

    let session = client.session_mut();
    session
        .select(folder)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {folder}: {e}")))?;
    let store_stream = session
        .uid_store(seq_set, &store_query)
        .await
        .map_err(|e| ImapError::Protocol(format!("UID STORE {store_query} {seq_set}: {e}")))?;
    let mut stream = store_stream;
    while let Some(_item) = stream.next().await {}
    Ok(())
}

/// Hard cap on how many UIDs a single bulk call will touch. Beyond this the
/// call is rejected with [`BulkError::LimitExceeded`] (stable code
/// `bulk_limit_exceeded`) rather than silently truncating.
pub const BULK_UID_LIMIT: usize = 500;

/// Max UIDs packed into a single coalesced sequence-set chunk. Keeps the
/// generated `UID` command comfortably under the ~8 KB line limits common on
/// IMAP servers even in the worst case (all singletons: `<=~200 uids -> <2KB`).
pub const CHUNK_SIZE: usize = 200;

/// What to operate on: explicit UIDs, or an IMAP search that is resolved to
/// UIDs first (reusing the same search path the `search` command uses).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BulkTarget {
    Uids(Vec<u32>),
    Search(String),
}

/// The single operation applied across every resolved UID.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BulkOp {
    Move { to_folder: String },
    Copy { to_folder: String },
    FlagAdd { flag: String },
    FlagRemove { flag: String },
    Delete,
    Tag { tag: String },
}

impl BulkOp {
    /// Stable action-type label for action logging.
    pub fn action_type(&self) -> &'static str {
        match self {
            BulkOp::Move { .. } => "bulk_move",
            BulkOp::Copy { .. } => "bulk_copy",
            BulkOp::FlagAdd { .. } => "bulk_flag_add",
            BulkOp::FlagRemove { .. } => "bulk_flag_remove",
            BulkOp::Delete => "bulk_delete",
            BulkOp::Tag { .. } => "bulk_tag",
        }
    }
}

/// A fully-specified bulk request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulkRequest {
    pub target: BulkTarget,
    pub op: BulkOp,
    /// Source folder the UIDs live in.
    pub folder: String,
    /// When true, resolve targets and report what WOULD happen with zero
    /// mutations.
    #[serde(default)]
    pub dry_run: bool,
}

/// One UID that could not be processed, with a stable machine code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkFailure {
    pub uid: u32,
    /// Stable machine code, e.g. `imap_error`, `no_message_id`, `fetch_failed`.
    pub code: String,
    pub reason: String,
}

/// Outcome of a bulk call. `succeeded + failed.len() == requested` for a
/// non-dry-run; a dry run leaves both empty and lists `resolved_uids` only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkResult {
    /// How many UIDs were resolved as targets.
    pub requested: usize,
    /// The resolved UIDs (always populated, including on dry runs).
    pub resolved_uids: Vec<u32>,
    /// UIDs that were mutated successfully (empty on dry run).
    pub succeeded: Vec<u32>,
    /// UIDs that failed, with per-UID codes (empty on dry run).
    pub failed: Vec<BulkFailure>,
    /// True when this was a dry run (no mutations performed).
    pub dry_run: bool,
}

/// Errors that abort the whole bulk call before any mutation (as opposed to
/// per-UID failures, which are recorded in [`BulkResult::failed`]).
#[derive(Debug, thiserror::Error)]
pub enum BulkError {
    #[error("bulk_limit_exceeded: {count} UIDs exceeds the {limit} per-call cap")]
    LimitExceeded { count: usize, limit: usize },
    #[error("no_targets: the request resolved to zero UIDs")]
    NoTargets,
    #[error(transparent)]
    Imap(#[from] ImapError),
    #[error("store_error: {0}")]
    Store(String),
}

impl BulkError {
    /// Stable machine code for JSON output.
    pub fn code(&self) -> &'static str {
        match self {
            BulkError::LimitExceeded { .. } => "bulk_limit_exceeded",
            BulkError::NoTargets => "no_targets",
            BulkError::Imap(_) => "imap_error",
            BulkError::Store(_) => "store_error",
        }
    }
}

/// Coalesce a sorted-or-unsorted UID list into IMAP sequence-set runs, e.g.
/// `[1,2,3,4,5,9,12,13,14] -> "1:5,9,12:14"`. Duplicates are collapsed.
pub fn coalesce_uids(uids: &[u32]) -> String {
    let mut sorted: Vec<u32> = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        let mut end = start;
        while i + 1 < sorted.len() && sorted[i + 1] == end + 1 {
            end = sorted[i + 1];
            i += 1;
        }
        if start == end {
            parts.push(start.to_string());
        } else {
            parts.push(format!("{start}:{end}"));
        }
        i += 1;
    }
    parts.join(",")
}

/// Split a UID list into chunks of at most [`CHUNK_SIZE`], each already
/// deduplicated + sorted, ready to be coalesced independently.
pub fn chunk_uids(uids: &[u32]) -> Vec<Vec<u32>> {
    let mut sorted: Vec<u32> = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    sorted.chunks(CHUNK_SIZE).map(|c| c.to_vec()).collect()
}

/// Resolve a [`BulkTarget`] into a concrete UID list. `Search` reuses
/// [`imap::search`] (capped at [`BULK_UID_LIMIT`] + 1 so the cap check can fire).
pub async fn resolve_target(
    client: &mut ImapClient,
    folder: &str,
    target: &BulkTarget,
) -> Result<Vec<u32>, BulkError> {
    match target {
        BulkTarget::Uids(uids) => {
            let mut u = uids.clone();
            u.sort_unstable();
            u.dedup();
            Ok(u)
        }
        BulkTarget::Search(query) => {
            let summaries =
                imap::search(client, folder, query, (BULK_UID_LIMIT + 1) as u32).await?;
            let mut u: Vec<u32> = summaries.into_iter().map(|s| s.uid).collect();
            u.sort_unstable();
            u.dedup();
            Ok(u)
        }
    }
}

/// Execute a bulk request against an already-connected IMAP client.
///
/// Never all-or-nothing: one failing UID is isolated and recorded, the rest
/// proceed. On a dry run, targets are resolved and returned with zero
/// mutations. The caller owns action logging.
pub async fn execute(
    client: &mut ImapClient,
    db: &Database,
    account_id: &str,
    req: &BulkRequest,
) -> Result<BulkResult, BulkError> {
    let uids = resolve_target(client, &req.folder, &req.target).await?;

    if uids.is_empty() {
        return Err(BulkError::NoTargets);
    }
    if uids.len() > BULK_UID_LIMIT {
        return Err(BulkError::LimitExceeded {
            count: uids.len(),
            limit: BULK_UID_LIMIT,
        });
    }

    if req.dry_run {
        return Ok(BulkResult {
            requested: uids.len(),
            resolved_uids: uids.clone(),
            succeeded: Vec::new(),
            failed: Vec::new(),
            dry_run: true,
        });
    }

    // Tag is a store-only op (no IMAP mutation); handle it separately because it
    // needs a per-UID Message-ID lookup, so it can't be batched over a seq-set.
    if let BulkOp::Tag { tag } = &req.op {
        return execute_tag(client, db, account_id, &req.folder, tag, uids).await;
    }

    let mut succeeded: Vec<u32> = Vec::new();
    let mut failed: Vec<BulkFailure> = Vec::new();

    for chunk in chunk_uids(&uids) {
        let seq_set = coalesce_uids(&chunk);
        match apply_imap_op(client, &req.folder, &req.op, &seq_set).await {
            Ok(()) => succeeded.extend_from_slice(&chunk),
            Err(_batch_err) => {
                // Batch failed; isolate the culprit(s) one UID at a time so a
                // single bad UID doesn't sink its whole chunk.
                for uid in chunk {
                    let single = coalesce_uids(&[uid]);
                    match apply_imap_op(client, &req.folder, &req.op, &single).await {
                        Ok(()) => succeeded.push(uid),
                        Err(e) => failed.push(BulkFailure {
                            uid,
                            code: "imap_error".to_string(),
                            reason: e.to_string(),
                        }),
                    }
                }
            }
        }
    }

    Ok(BulkResult {
        requested: uids.len(),
        resolved_uids: uids,
        succeeded,
        failed,
        dry_run: false,
    })
}

/// Apply one non-tag op to a coalesced sequence-set (one chunk).
async fn apply_imap_op(
    client: &mut ImapClient,
    folder: &str,
    op: &BulkOp,
    seq_set: &str,
) -> Result<(), ImapError> {
    match op {
        BulkOp::Move { to_folder } => move_messages(client, seq_set, folder, to_folder).await,
        BulkOp::Copy { to_folder } => copy_messages(client, seq_set, folder, to_folder).await,
        BulkOp::FlagAdd { flag } => store_flag_messages(client, folder, seq_set, flag, true).await,
        BulkOp::FlagRemove { flag } => {
            store_flag_messages(client, folder, seq_set, flag, false).await
        }
        BulkOp::Delete => delete_messages(client, folder, seq_set).await,
        BulkOp::Tag { .. } => unreachable!("tag handled by execute_tag"),
    }
}

/// Tag every resolved UID through the same store path the single-message
/// `tag set` command uses: fetch each message to resolve UID -> Message-ID,
/// then `db.add_tag(...)`. Per-UID failures are isolated.
async fn execute_tag(
    client: &mut ImapClient,
    db: &Database,
    account_id: &str,
    folder: &str,
    tag: &str,
    uids: Vec<u32>,
) -> Result<BulkResult, BulkError> {
    let mut succeeded: Vec<u32> = Vec::new();
    let mut failed: Vec<BulkFailure> = Vec::new();

    for uid in &uids {
        let msg = match imap::fetch_message(client, folder, *uid).await {
            Ok(Some(m)) => m,
            Ok(None) => {
                failed.push(BulkFailure {
                    uid: *uid,
                    code: "not_found".to_string(),
                    reason: format!("message UID {uid} not found in {folder}"),
                });
                continue;
            }
            Err(e) => {
                failed.push(BulkFailure {
                    uid: *uid,
                    code: "fetch_failed".to_string(),
                    reason: e.to_string(),
                });
                continue;
            }
        };

        let Some(message_id) = msg.message_id.as_deref() else {
            failed.push(BulkFailure {
                uid: *uid,
                code: "no_message_id".to_string(),
                reason: format!("message UID {uid} has no Message-ID header"),
            });
            continue;
        };

        match db.add_tag(account_id, message_id, tag, Some(*uid as i64), Some(folder)) {
            Ok(()) => succeeded.push(*uid),
            Err(e) => failed.push(BulkFailure {
                uid: *uid,
                code: "store_error".to_string(),
                reason: e.to_string(),
            }),
        }
    }

    Ok(BulkResult {
        requested: uids.len(),
        resolved_uids: uids,
        succeeded,
        failed,
        dry_run: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesce_contiguous_and_gaps() {
        let uids = vec![1, 2, 3, 4, 5, 9, 12, 13, 14];
        assert_eq!(coalesce_uids(&uids), "1:5,9,12:14");
    }

    #[test]
    fn coalesce_sorts_and_dedups() {
        let uids = vec![14, 1, 3, 2, 3, 13, 12];
        // sorted+dedup -> 1,2,3,12,13,14 -> "1:3,12:14"
        assert_eq!(coalesce_uids(&uids), "1:3,12:14");
    }

    #[test]
    fn coalesce_all_singletons() {
        assert_eq!(coalesce_uids(&[1, 3, 5, 7]), "1,3,5,7");
    }

    #[test]
    fn coalesce_single_uid() {
        assert_eq!(coalesce_uids(&[42]), "42");
    }

    #[test]
    fn coalesce_empty() {
        assert_eq!(coalesce_uids(&[]), "");
    }

    #[test]
    fn chunk_respects_chunk_size() {
        let uids: Vec<u32> = (1..=450).collect();
        let chunks = chunk_uids(&uids);
        assert_eq!(chunks.len(), 3); // 200 + 200 + 50
        assert_eq!(chunks[0].len(), CHUNK_SIZE);
        assert_eq!(chunks[1].len(), CHUNK_SIZE);
        assert_eq!(chunks[2].len(), 50);
    }

    #[test]
    fn chunk_dedups_across_input() {
        let uids = vec![5, 5, 5, 1, 1];
        let chunks = chunk_uids(&uids);
        assert_eq!(chunks, vec![vec![1, 5]]);
    }

    #[test]
    fn chunk_empty_yields_no_chunks() {
        assert!(chunk_uids(&[]).is_empty());
    }

    #[test]
    fn limit_exceeded_error_code_is_stable() {
        let e = BulkError::LimitExceeded {
            count: 501,
            limit: BULK_UID_LIMIT,
        };
        assert_eq!(e.code(), "bulk_limit_exceeded");
    }

    #[test]
    fn no_targets_error_code_is_stable() {
        assert_eq!(BulkError::NoTargets.code(), "no_targets");
    }

    #[test]
    fn dry_run_result_has_no_mutations() {
        let r = BulkResult {
            requested: 3,
            resolved_uids: vec![1, 2, 3],
            succeeded: vec![],
            failed: vec![],
            dry_run: true,
        };
        assert!(r.succeeded.is_empty());
        assert!(r.failed.is_empty());
        assert_eq!(r.resolved_uids.len(), r.requested);
    }

    #[test]
    fn op_action_types_are_stable() {
        assert_eq!(
            BulkOp::Move {
                to_folder: "Archive".into()
            }
            .action_type(),
            "bulk_move"
        );
        assert_eq!(
            BulkOp::Copy {
                to_folder: "X".into()
            }
            .action_type(),
            "bulk_copy"
        );
        assert_eq!(
            BulkOp::FlagAdd {
                flag: "seen".into()
            }
            .action_type(),
            "bulk_flag_add"
        );
        assert_eq!(
            BulkOp::FlagRemove {
                flag: "seen".into()
            }
            .action_type(),
            "bulk_flag_remove"
        );
        assert_eq!(BulkOp::Delete.action_type(), "bulk_delete");
        assert_eq!(BulkOp::Tag { tag: "vip".into() }.action_type(), "bulk_tag");
    }

    #[test]
    fn mailbox_arg_quotes_and_escapes() {
        assert_eq!(imap_mailbox_arg("Junk E-mail"), "\"Junk E-mail\"");
        assert_eq!(imap_mailbox_arg("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn flag_name_maps_system_flags() {
        assert_eq!(map_flag_name("seen"), "\\Seen");
        assert_eq!(map_flag_name("\\Flagged"), "\\Flagged");
        assert_eq!(map_flag_name("custom"), "custom");
    }

    #[test]
    fn validate_rejects_injection() {
        assert!(validate_imap_input("1:5").is_ok());
        assert!(validate_imap_input("1\r\nDELETE").is_err());
        assert!(validate_imap_input("a{3}").is_err());
    }

    #[test]
    fn bulk_result_serializes_with_stable_shape() {
        let r = BulkResult {
            requested: 2,
            resolved_uids: vec![1, 2],
            succeeded: vec![1],
            failed: vec![BulkFailure {
                uid: 2,
                code: "imap_error".to_string(),
                reason: "boom".to_string(),
            }],
            dry_run: false,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["requested"], 2);
        assert_eq!(v["succeeded"][0], 1);
        assert_eq!(v["failed"][0]["uid"], 2);
        assert_eq!(v["failed"][0]["code"], "imap_error");
        assert_eq!(v["dry_run"], false);
    }
}
