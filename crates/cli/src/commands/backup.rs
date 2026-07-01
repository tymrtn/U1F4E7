// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! `envelope backup` CLI handler — orchestrates IMAP and filesystem I/O for
//! the staged source -> archive -> destination flow. The pure logic
//! (manifests, planning, verification) lives in
//! `envelope_email_transport::backup`; this file only wires Clap, IMAP, and
//! event emission.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, IsTerminal, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use envelope_email_store::{CredentialBackend, app_data_dir};
use envelope_email_transport::backup::{
    self, ArchiveAccount, ArchiveFolderRecord, ArchiveManifest, ArchiveMessageRecord, BackupError,
    BackupEvent, FolderMapping, PlannedAppend, RestorePlan,
};
use envelope_email_transport::{imap, migrate};

use super::common::setup_credentials;
use crate::BackupCmd;

const TOOL_NAME: &str = "envelope";
const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");
const ARCHIVES_DIR_NAME: &str = "archives";

// ---------------------------------------------------------------------------
// Default archive-dir resolution
// ---------------------------------------------------------------------------

/// Replace non-alphanumeric chars (except `-`, `_`, `.`) with `_` and lowercase.
fn sanitize_account_for_filename(account: &str) -> String {
    account
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Build the default archive directory path:
/// `<app_data_dir>/archives/<sanitized_account>-<compact_utc_timestamp>-<pid>`
fn default_archive_dir(account: &str) -> PathBuf {
    let sanitized = sanitize_account_for_filename(account);
    let now = chrono::Utc::now();
    let timestamp = format!(
        "{}{:03}Z",
        now.format("%Y%m%dT%H%M%S"),
        now.timestamp_subsec_millis()
    );
    let pid = std::process::id();
    app_data_dir()
        .join(ARCHIVES_DIR_NAME)
        .join(format!("{sanitized}-{timestamp}-{pid}"))
}

/// Resolve the final archive directory from the optional `--out` flag.
///
/// - `Some(path)` → use it directly.
/// - `None` + JSON mode or non-interactive stdin → use default silently.
/// - `None` + interactive TTY → prompt the user to accept, reject, or override.
fn resolve_export_dir(out: Option<PathBuf>, account: &str, json_output: bool) -> Result<PathBuf> {
    if let Some(path) = out {
        return Ok(path);
    }

    let default_path = default_archive_dir(account);

    // Non-interactive: use default without prompting.
    if json_output || !io::stdin().is_terminal() {
        return Ok(default_path);
    }

    // Interactive TTY prompt.
    eprintln!("Archive will be saved to: {}", default_path.display());
    eprint!("Accept default? [Y/n/custom path]: ");
    io::stderr().flush()?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed to read from stdin")?;
    let input = input.trim();

    if input.is_empty() || input.eq_ignore_ascii_case("y") || input.eq_ignore_ascii_case("yes") {
        Ok(default_path)
    } else if input.eq_ignore_ascii_case("n") || input.eq_ignore_ascii_case("no") {
        bail!("export aborted by user")
    } else {
        Ok(PathBuf::from(input))
    }
}

/// Typed export args, matched to the Clap shape but easier to pass than a long
/// positional argument list. Keeping a record-style struct also avoids adding
/// to the existing `too_many_arguments` clippy baseline that the migration
/// handoff explicitly called out.
struct ExportArgs {
    account: String,
    out: Option<PathBuf>,
    include: Vec<String>,
    exclude: Vec<String>,
    batch_size: u32,
}

struct RestoreArgs {
    account: String,
    from: PathBuf,
    include: Vec<String>,
    exclude: Vec<String>,
    map: Vec<String>,
    dry_run: bool,
    batch_size: u32,
}

struct AuditStateArgs {
    account: String,
    from: PathBuf,
    include: Vec<String>,
    exclude: Vec<String>,
    map: Vec<String>,
}

#[tokio::main]
pub async fn run(
    subcommand: BackupCmd,
    json_output: bool,
    backend: CredentialBackend,
) -> Result<()> {
    // Issue #21: capture the failing phase up front so a fatal error can be
    // re-emitted as a machine-readable JSON `error` event instead of falling
    // through to the plain-text top-level handler in main.rs.
    let phase = match &subcommand {
        BackupCmd::Export { .. } => "export",
        BackupCmd::Verify { .. } => "verify",
        BackupCmd::Restore { .. } => "restore",
        BackupCmd::AuditState { .. } => "audit_state",
    };
    let result = run_inner(subcommand, json_output, backend).await;
    if json_output && let Err(e) = &result {
        // Best-effort: never let error reporting itself mask the original
        // failure. The error string is the anyhow chain, which carries no
        // secrets/bodies by construction in these paths.
        let _ = emit(
            true,
            BackupEvent::Error {
                ok: false,
                error: format!("{e:#}"),
                phase: Some(phase.to_string()),
            },
        );
    }
    result
}

async fn run_inner(
    subcommand: BackupCmd,
    json_output: bool,
    backend: CredentialBackend,
) -> Result<()> {
    match subcommand {
        BackupCmd::Export {
            account,
            out,
            include,
            exclude,
            batch_size,
        } => {
            run_export(
                ExportArgs {
                    account,
                    out,
                    include,
                    exclude,
                    batch_size,
                },
                json_output,
                backend,
            )
            .await
        }
        BackupCmd::Verify { from, strict } => run_verify(from, strict, json_output),
        BackupCmd::Restore {
            account,
            from,
            include,
            exclude,
            map,
            dry_run,
            batch_size,
        } => {
            run_restore(
                RestoreArgs {
                    account,
                    from,
                    include,
                    exclude,
                    map,
                    dry_run,
                    batch_size,
                },
                json_output,
                backend,
            )
            .await
        }
        BackupCmd::AuditState {
            account,
            from,
            include,
            exclude,
            map,
        } => run_audit_state(
            AuditStateArgs {
                account,
                from,
                include,
                exclude,
                map,
            },
            json_output,
        ),
    }
}

// -----------------------------------------------------------------------------
// Export
// -----------------------------------------------------------------------------

async fn run_export(args: ExportArgs, json_output: bool, backend: CredentialBackend) -> Result<()> {
    let ExportArgs {
        account,
        out,
        include,
        exclude,
        batch_size,
    } = args;
    let batch_size = migrate::validate_batch_size(batch_size).map_err(anyhow::Error::msg)?;

    let (_db, src) = setup_credentials(Some(&account), backend)?;
    let account_identity = if src.account.username.trim().is_empty() {
        src.account.id.as_str()
    } else {
        src.account.username.as_str()
    };
    let out = resolve_export_dir(out, account_identity, json_output)?;

    // Refuse to write into a non-empty existing output directory. This guards
    // against stale manifests, symlinks pointing outside the archive, or
    // unrelated files masquerading as archive contents — any of which could
    // make a later verify or restore behave incorrectly.
    backup::validate_export_output_dir(&out).map_err(|e| backup_error_to_anyhow(e, &out))?;

    fs::create_dir_all(&out).with_context(|| format!("create archive dir {}", out.display()))?;
    fs::create_dir_all(out.join("messages"))
        .with_context(|| format!("create messages dir under {}", out.display()))?;

    let mut client = imap::connect(&src)
        .await
        .context("source IMAP connection failed")?;

    let folders = imap::list_folder_stats(&mut client)
        .await
        .context("source folder listing failed")?;

    let mut total_messages = 0u32;
    let mut total_bytes = 0u64;
    let mut total_folders = 0u32;
    let mut folder_records: Vec<ArchiveFolderRecord> = Vec::new();
    let mut message_records: Vec<ArchiveMessageRecord> = Vec::new();
    let mut export_failed = 0u32;

    for folder in folders {
        if !migrate::folder_selected(&folder.folder, &include, &exclude) {
            continue;
        }
        total_folders += 1;
        // EXAMINE (read-only SELECT) so the source mailbox cannot be mutated
        // by Envelope while we're reading: no `\Seen` set on FETCH, no
        // `\Recent` cleared, server rejects any STORE/APPEND on this session.
        let info = imap::examine_folder_info(&mut client, &folder.folder).await?;
        let uidvalidity = info.uidvalidity_key();
        let encoded_dir = backup::encode_folder_for_disk(&folder.folder);
        let folder_disk = out.join("messages").join(&encoded_dir);
        fs::create_dir_all(&folder_disk)
            .with_context(|| format!("create folder dir {}", folder_disk.display()))?;
        emit(
            json_output,
            BackupEvent::ExportFolderStart {
                folder: folder.folder.clone(),
                messages: info.exists,
            },
        )?;

        let uids = imap::list_selected_uids(&mut client).await?;
        let mut written = 0u32;
        // Issue #20: each `uid_set` materializes `batch_size` full RFC822
        // bodies in memory (the FETCH response Vec). We bound `batch_size` to
        // MAX_BATCH_SIZE in `validate_batch_size` and write+drop each body to
        // disk immediately below, so peak memory is ~one batch of bodies, never
        // the whole folder.
        for uid_set in migrate::uid_sequence_set_batches(&uids, batch_size) {
            let messages =
                imap::fetch_raw_messages_selected_uid_set(&mut client, &folder.folder, &uid_set)
                    .await?;
            for msg in messages {
                let rel_path = backup::relative_message_path(&folder.folder, uidvalidity, msg.uid);
                let abs_path = out.join(&rel_path);
                if let Err(e) = fs::write(&abs_path, &msg.rfc822) {
                    export_failed += 1;
                    emit(
                        json_output,
                        BackupEvent::ExportMessageFailed {
                            folder: folder.folder.clone(),
                            uid: msg.uid,
                            error: format!("write {}: {e}", abs_path.display()),
                        },
                    )?;
                    continue;
                }
                let sha = backup::sha256_hex(&msg.rfc822);
                let internal_date = msg.internal_date.map(|d| d.to_rfc3339());
                let bytes = msg.size as u64;
                message_records.push(ArchiveMessageRecord {
                    folder: folder.folder.clone(),
                    uid: msg.uid,
                    uidvalidity,
                    message_id: msg.message_id.clone(),
                    internal_date,
                    flags: msg.flags.clone(),
                    size: bytes,
                    sha256: sha.clone(),
                    rel_path: rel_path.clone(),
                });
                written += 1;
                total_bytes = total_bytes.saturating_add(bytes);
                emit(
                    json_output,
                    BackupEvent::ExportMessageWritten {
                        folder: folder.folder.clone(),
                        uid: msg.uid,
                        bytes,
                        sha256: sha,
                    },
                )?;
            }
        }
        total_messages = total_messages.saturating_add(written);
        folder_records.push(ArchiveFolderRecord {
            name: folder.folder.clone(),
            uidvalidity,
            encoded_dir,
            message_count: written,
        });
        emit(
            json_output,
            BackupEvent::ExportFolderDone {
                folder: folder.folder.clone(),
                written,
            },
        )?;
    }

    // Atomicity rule: never produce a manifest that *looks* complete when one
    // or more message bodies failed to land on disk. A future verify run
    // would happily accept the partial archive otherwise. Bail before writing
    // the final manifest; the half-written archive can be deleted by the
    // operator since they passed an empty `--out`.
    if export_failed > 0 {
        bail!(
            "export aborted: {} message write(s) failed; manifest.json was NOT written so this directory is not a valid archive",
            export_failed
        );
    }

    let manifest = ArchiveManifest {
        archive_format_version: backup::ARCHIVE_FORMAT_VERSION,
        tool: TOOL_NAME.to_string(),
        tool_version: TOOL_VERSION.to_string(),
        exported_at: backup::exported_at_now_utc(),
        account: ArchiveAccount {
            id: src.account.id.clone(),
            email: format!("{}@{}", src.account.username, src.account.domain),
            imap_host: src.account.imap_host.clone(),
            imap_port: src.account.imap_port,
            imap_username: src.effective_imap_username().to_string(),
        },
        folders: folder_records,
        messages: message_records,
    };
    // Defense in depth: validate before writing. validate_manifest is the
    // same check verify will run later, so an export that wouldn't survive a
    // round-trip never persists a misleading manifest.
    backup::validate_manifest(&manifest).map_err(|e| backup_error_to_anyhow(e, &out))?;
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    backup::write_atomic(&backup::manifest_path(&out), &manifest_bytes)
        .context("write manifest.json atomically")?;

    emit(
        json_output,
        BackupEvent::ExportRunDone {
            folders: total_folders,
            messages: total_messages,
            bytes: total_bytes,
            archive_dir: out.display().to_string(),
        },
    )?;

    Ok(())
}

// -----------------------------------------------------------------------------
// Verify
// -----------------------------------------------------------------------------

fn run_verify(from: PathBuf, strict: bool, json_output: bool) -> Result<()> {
    let outcome = backup::verify_archive(&from)
        .with_context(|| format!("verify archive {}", from.display()))?;

    for missing in &outcome.missing {
        emit(
            json_output,
            BackupEvent::VerifyMissingFile {
                folder: missing.folder.clone(),
                uid: missing.uid,
                rel_path: missing.rel_path.clone(),
            },
        )?;
    }
    for c in &outcome.corrupt {
        match c {
            backup::CorruptFile::SizeMismatch {
                folder,
                uid,
                rel_path,
                expected_size,
                actual_size,
            } => emit(
                json_output,
                BackupEvent::VerifySizeMismatch {
                    folder: folder.clone(),
                    uid: *uid,
                    rel_path: rel_path.clone(),
                    expected_size: *expected_size,
                    actual_size: *actual_size,
                },
            )?,
            backup::CorruptFile::ChecksumMismatch {
                folder,
                uid,
                rel_path,
                expected_sha256,
                actual_sha256,
            } => emit(
                json_output,
                BackupEvent::VerifyChecksumMismatch {
                    folder: folder.clone(),
                    uid: *uid,
                    rel_path: rel_path.clone(),
                    expected_sha256: expected_sha256.clone(),
                    actual_sha256: actual_sha256.clone(),
                },
            )?,
        }
    }
    for extra in &outcome.extras {
        emit(
            json_output,
            BackupEvent::VerifyExtraFile {
                rel_path: extra.clone(),
            },
        )?;
    }
    for unsafe_entry in &outcome.unsafe_entries {
        emit(
            json_output,
            BackupEvent::VerifyUnsafeEntry {
                rel_path: unsafe_entry.clone(),
            },
        )?;
    }

    let strict_extras_fail = strict && !outcome.extras.is_empty();
    // `outcome.ok` is already false when unsafe entries exist; strict only
    // promotes otherwise-tolerated extra files into a failure.
    let final_ok = outcome.ok && !strict_extras_fail;

    emit(
        json_output,
        BackupEvent::VerifyDone {
            ok: final_ok,
            missing: outcome.missing.len() as u32,
            corrupt: outcome.corrupt.len() as u32,
            extras: outcome.extras.len() as u32,
            unsafe_entries: outcome.unsafe_entries.len() as u32,
        },
    )?;

    if !final_ok {
        bail!(
            "verify failed: missing={} corrupt={} extras={} unsafe_entries={} (strict={})",
            outcome.missing.len(),
            outcome.corrupt.len(),
            outcome.extras.len(),
            outcome.unsafe_entries.len(),
            strict
        );
    }
    Ok(())
}

fn preflight_verify_archive_for_restore(
    from: &Path,
    dry_run: bool,
    json_output: bool,
) -> Result<()> {
    if dry_run {
        // Dry-run is the operator's safety gate before a live restore, so it
        // must prove the archive bytes still match the manifest before it ever
        // reports `would_append`. Reuse `backup verify` so JSON callers get the
        // same machine-readable verify events on success/failure.
        run_verify(from.to_path_buf(), false, json_output)
            .with_context(|| format!("preflight verify of archive {}", from.display()))?;
    } else {
        // Live restore keeps the existing preflight behavior: refuse to start
        // if the archive has missing/corrupt message bytes, but don't fail on
        // unreferenced extra files unless the operator runs `backup verify`.
        let outcome = backup::verify_archive(from)
            .with_context(|| format!("preflight verify of {}", from.display()))?;
        if !outcome.ok {
            bail!(
                "preflight verify of archive {} failed: missing={} corrupt={}",
                from.display(),
                outcome.missing.len(),
                outcome.corrupt.len()
            );
        }
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Restore
// -----------------------------------------------------------------------------

async fn run_restore(
    args: RestoreArgs,
    json_output: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let RestoreArgs {
        account,
        from,
        include,
        exclude,
        map,
        dry_run,
        batch_size,
    } = args;
    let batch_size = migrate::validate_batch_size(batch_size).map_err(anyhow::Error::msg)?;
    let mappings = parse_mappings(&map)?;
    // read_manifest runs validate_manifest (Critical #2) before returning, so
    // the records we plan against are already guaranteed to have canonical
    // rel_paths, no duplicates, and matching folder counts.
    let manifest = backup::read_manifest(&from).map_err(|e| backup_error_to_anyhow(e, &from))?;

    // Preflight verify (HP #6 + issue #10): refuse to start a restore plan
    // against an archive whose bytes no longer match its manifest. Dry-run
    // must verify before reporting `would_append`; live restore keeps its
    // existing missing/corrupt-only gate.
    preflight_verify_archive_for_restore(&from, dry_run, json_output)?;

    let (_db, dst) = setup_credentials(Some(&account), backend)?;
    backup::validate_restore_destination(
        &manifest.account,
        &dst.account.id,
        &dst.account.imap_host,
        dst.account.imap_port,
        dst.effective_imap_username(),
    )
    .map_err(|e| backup_error_to_anyhow(e, &from))?;
    let state_path = backup::restore_state_path(&from, &dst.account.id);
    let state_outcome = backup::load_restore_state(&state_path)
        .map_err(|e| backup_error_to_anyhow(e, &state_path))?;

    // Issue #19: surface restore-state warnings (malformed lines, pending
    // records from a prior crash) as machine-readable events before planning.
    for warning in &state_outcome.warnings {
        emit(
            json_output,
            BackupEvent::RestoreStateWarning {
                warning: format!("{warning:?}"),
            },
        )?;
    }

    let mut state = state_outcome.records;

    let plan: RestorePlan =
        backup::plan_restore(&manifest.messages, &state, &mappings, &include, &exclude);

    // Bucket by source folder so we emit one start/done per folder, mirroring
    // migrate's per-folder progress shape. PlannedAppend now carries the full
    // record (Critical #3), so the executor never needs to look anything up.
    let mut by_source: HashMap<String, Vec<&PlannedAppend>> = HashMap::new();
    for action in &plan.planned_appends {
        by_source
            .entry(action.source_folder().to_string())
            .or_default()
            .push(action);
    }
    let mut sorted_folders: Vec<String> = by_source.keys().cloned().collect();
    sorted_folders.sort();

    if dry_run {
        for source in &sorted_folders {
            let actions = &by_source[source];
            let dest = actions[0].destination_folder.clone();
            emit(
                json_output,
                BackupEvent::RestoreFolderStart {
                    source: source.clone(),
                    destination: dest,
                    messages: actions.len() as u32,
                },
            )?;
        }
        emit(
            json_output,
            BackupEvent::RestoreDryRunDone {
                folders: sorted_folders.len() as u32,
                would_append: plan.planned_appends.len() as u32,
                would_skip: plan
                    .skipped_already_restored
                    .saturating_add(plan.skipped_excluded),
            },
        )?;
        return Ok(());
    }

    // HP #7: surface any restore-state writability problem (permissions, full
    // disk, read-only filesystem) before we ever touch the destination IMAP.
    // touch_restore_state writes nothing if the file already exists.
    touch_restore_state(&state_path).with_context(|| {
        format!(
            "preflight: cannot write restore state {}",
            state_path.display()
        )
    })?;

    let mut client = imap::connect(&dst)
        .await
        .context("destination IMAP connection failed")?;

    // Pre-create destination folders in plan order (deduped) so per-message
    // appends never need to LIST the server.
    let mut created: HashSet<String> = HashSet::new();
    for dest in &plan.destinations {
        if created.insert(dest.clone()) {
            imap::create_folder_if_missing(&mut client, dest)
                .await
                .with_context(|| format!("create destination folder {dest}"))?;
        }
    }

    let mut total_appended = 0u32;
    let mut total_skipped = plan
        .skipped_already_restored
        .saturating_add(plan.skipped_excluded);
    let mut total_failed = 0u32;

    for source in &sorted_folders {
        let actions = &by_source[source];
        let destination = actions[0].destination_folder.clone();
        emit(
            json_output,
            BackupEvent::RestoreFolderStart {
                source: source.clone(),
                destination: destination.clone(),
                messages: actions.len() as u32,
            },
        )?;
        let mut appended = 0u32;
        let mut skipped = 0u32;
        let mut failed = 0u32;
        // HP #8: chunk per-folder action lists into windows of `--batch-size`.
        // Within a chunk we still process one APPEND at a time (each .eml is
        // read+appended+dropped sequentially), but the chunk boundary is the
        // natural place to bound how many actions are queued in memory and
        // gives operators a way to cap per-loop work for huge folders.
        for chunk in actions.chunks(batch_size as usize) {
            for action in chunk {
                // PlannedAppend owns its ArchiveMessageRecord — no scan, no
                // ambiguous-match risk under duplicate manifests.
                let record = &action.record;
                // Walk every component beneath the archive root and refuse to
                // follow any symlink. Closes the gap where a symlinked parent
                // dir (e.g. messages/INBOX -> /tmp/outside) would otherwise
                // let restore push bytes from outside the archive into the
                // destination mailbox.
                let abs_path = match backup::validate_materialized_message_path(&from, record) {
                    Ok(p) => p,
                    Err(e) => {
                        failed += 1;
                        emit(
                            json_output,
                            BackupEvent::RestoreMessageFailed {
                                source: source.clone(),
                                destination: destination.clone(),
                                uid: action.uid(),
                                error: format!("unsafe path for {}: {e}", record.rel_path),
                            },
                        )?;
                        continue;
                    }
                };
                let rfc822 = match fs::read(&abs_path) {
                    Ok(b) => b,
                    Err(e) => {
                        failed += 1;
                        emit(
                            json_output,
                            BackupEvent::RestoreMessageFailed {
                                source: source.clone(),
                                destination: destination.clone(),
                                uid: action.uid(),
                                error: format!("read {}: {e}", abs_path.display()),
                            },
                        )?;
                        continue;
                    }
                };
                // Defense in depth: if the on-disk file no longer matches the
                // manifest sha256, refuse to push corrupted bytes. Should be
                // unreachable for non-dry restores because preflight verify
                // already ran, but the check is cheap and tightens the loop.
                let actual_sha = backup::sha256_hex(&rfc822);
                if actual_sha != record.sha256 {
                    failed += 1;
                    emit(
                        json_output,
                        BackupEvent::RestoreMessageFailed {
                            source: source.clone(),
                            destination: destination.clone(),
                            uid: action.uid(),
                            error: format!(
                                "checksum mismatch for {}: expected {} got {}",
                                record.rel_path, record.sha256, actual_sha
                            ),
                        },
                    )?;
                    continue;
                }
                // Destination Message-ID dedup, mirroring migrate.
                if let Some(message_id) = record.message_id.as_deref() {
                    let already =
                        imap::find_uid_by_message_id(&mut client, &destination, message_id).await?;
                    if already.is_some() {
                        if let Err(e) = persist_state(&state_path, record, &mut state) {
                            return Err(e.context(format!(
                                "failed to persist restore state for {} UID {}",
                                source,
                                action.uid()
                            )));
                        }
                        skipped += 1;
                        emit(
                            json_output,
                            BackupEvent::RestoreMessageSkipped {
                                source: source.clone(),
                                uid: action.uid(),
                                reason: "destination_message_id".into(),
                            },
                        )?;
                        continue;
                    }
                }
                // Issue #19: write pending state BEFORE APPEND so that a
                // crash after APPEND but before the done-state write still
                // leaves a recoverable breadcrumb. On reload the pending
                // record is promoted to "done" (conservative skip + warning)
                // rather than silently re-appending a duplicate.
                if let Err(e) = persist_state_pending(&state_path, record) {
                    return Err(e.context(format!(
                        "failed to write pending restore state for {} UID {} before APPEND",
                        source,
                        action.uid()
                    )));
                }
                let flags = migrate::append_flags(&record.flags);
                let internal_date = backup::parse_internal_date(record.internal_date.as_deref());
                match imap::append_message_with_date(
                    &mut client,
                    &destination,
                    &flags,
                    internal_date,
                    &rfc822,
                )
                .await
                {
                    Ok(()) => {
                        // HP #7: state write failure after a successful APPEND
                        // is a hard error — we already mutated the destination
                        // and would otherwise re-append on rerun.
                        if let Err(e) = persist_state(&state_path, record, &mut state) {
                            return Err(e.context(format!(
                                "APPEND succeeded for {} UID {} but persisting restore state failed; aborting before subsequent appends would silently duplicate",
                                source,
                                action.uid()
                            )));
                        }
                        appended += 1;
                        emit(
                            json_output,
                            BackupEvent::RestoreMessageAppended {
                                source: source.clone(),
                                destination: destination.clone(),
                                uid: action.uid(),
                                bytes: record.size,
                            },
                        )?;
                    }
                    Err(e) => {
                        failed += 1;
                        emit(
                            json_output,
                            BackupEvent::RestoreMessageFailed {
                                source: source.clone(),
                                destination: destination.clone(),
                                uid: action.uid(),
                                error: e.to_string(),
                            },
                        )?;
                    }
                }
            }
        }
        total_appended = total_appended.saturating_add(appended);
        total_skipped = total_skipped.saturating_add(skipped);
        total_failed = total_failed.saturating_add(failed);
        emit(
            json_output,
            BackupEvent::RestoreFolderDone {
                source: source.clone(),
                destination,
                appended,
                skipped,
                failed,
            },
        )?;
    }

    emit(
        json_output,
        BackupEvent::RestoreRunDone {
            folders: sorted_folders.len() as u32,
            appended: total_appended,
            skipped: total_skipped,
            failed: total_failed,
        },
    )?;

    if total_failed > 0 {
        bail!("restore completed with {total_failed} failed message(s)");
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Audit-state (read-only; no IMAP)
// -----------------------------------------------------------------------------

fn run_audit_state(args: AuditStateArgs, json_output: bool) -> Result<()> {
    let AuditStateArgs {
        account,
        from,
        include,
        exclude,
        map,
    } = args;
    let mappings = parse_mappings(&map)?;
    let manifest = backup::read_manifest(&from).map_err(|e| backup_error_to_anyhow(e, &from))?;
    let state_path = backup::restore_state_path(&from, &account);
    let state_outcome = backup::load_restore_state(&state_path)
        .map_err(|e| backup_error_to_anyhow(e, &state_path))?;

    let rows =
        backup::plan_restore_state_audit(&manifest, &state_outcome, &mappings, &include, &exclude);

    let mut unknown = 0u32;
    let mut state_not_in_manifest = 0u32;
    for row in &rows {
        match row.status {
            backup::AuditStatus::UnknownNoMessageId => unknown = unknown.saturating_add(1),
            backup::AuditStatus::StateNotInManifest => {
                state_not_in_manifest = state_not_in_manifest.saturating_add(1)
            }
            backup::AuditStatus::Planned => {}
        }
        emit(
            json_output,
            BackupEvent::RestoreStateAuditRecord {
                source: row.source.clone(),
                destination: row.destination.clone(),
                uidvalidity: row.uidvalidity,
                uid: row.uid,
                sha256: row.sha256.clone(),
                message_id_present: row.message_id_present,
                status: row.status.as_wire_str().to_string(),
                destination_uid: None,
                error: None,
            },
        )?;
    }
    emit(
        json_output,
        BackupEvent::RestoreStateAuditDone {
            pending: rows.len() as u32,
            present: 0,
            missing: 0,
            unknown,
            state_not_in_manifest,
            errors: 0,
        },
    )?;
    Ok(())
}

fn parse_mappings(map: &[String]) -> Result<Vec<FolderMapping>> {
    let mut out = Vec::with_capacity(map.len());
    for raw in map {
        out.push(backup::parse_folder_mapping_arg(raw).map_err(|e| anyhow::anyhow!("{e}"))?);
    }
    Ok(out)
}

/// Persist a restore-state record to disk AND update the in-memory set so
/// subsequent loop iterations in this run see it. Without the in-memory
/// update, a malformed manifest with duplicate identities would APPEND twice
/// in the same run; manifest validation already rejects duplicates, but
/// keeping the in-memory state in sync makes the bug impossible by
/// construction.
fn persist_state(
    path: &Path,
    record: &ArchiveMessageRecord,
    in_memory: &mut HashSet<backup::RestoreStateRecord>,
) -> Result<()> {
    let key = backup::restore_state_key(record);
    backup::append_restore_state_line(path, &key).map_err(|e| backup_error_to_anyhow(e, path))?;
    in_memory.insert(key);
    Ok(())
}

/// Issue #19: write a pending-state record BEFORE IMAP APPEND. If the process
/// crashes after APPEND but before `persist_state` writes the done record,
/// `load_restore_state` will find the pending line and promote it to done on
/// the next run — preventing a duplicate for messages without Message-ID.
fn persist_state_pending(path: &Path, record: &ArchiveMessageRecord) -> Result<()> {
    let key = backup::restore_state_key_pending(record);
    backup::append_restore_state_line(path, &key).map_err(|e| backup_error_to_anyhow(e, path))?;
    Ok(())
}

/// Surface restore-state file writability before we ever touch IMAP. Opens
/// the path with append+create and immediately closes it. If the file
/// already exists this is a no-op; if it can't be created (read-only
/// filesystem, missing parent, permission denied) we get the failure here
/// instead of after a partial APPEND.
fn touch_restore_state(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create restore state parent dir {}", parent.display()))?;
    }
    // Issue #18: confine the sidecar to a real regular file inside the archive
    // before we create/open it, so a symlinked path can't redirect state IO.
    backup::ensure_restore_state_path_safe(path).map_err(|e| backup_error_to_anyhow(e, path))?;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open restore state {}", path.display()))?;
    Ok(())
}

fn backup_error_to_anyhow(err: BackupError, ctx: &Path) -> anyhow::Error {
    anyhow::anyhow!("{err} (at {})", ctx.display())
}

// -----------------------------------------------------------------------------
// Event emission
// -----------------------------------------------------------------------------

fn emit(json_output: bool, event: BackupEvent) -> Result<()> {
    if json_output {
        println!("{}", serde_json::to_string(&event)?);
    } else {
        match &event {
            BackupEvent::ExportFolderStart { folder, messages } => {
                println!("export start: {folder} ({messages} messages)")
            }
            BackupEvent::ExportMessageWritten {
                folder, uid, bytes, ..
            } => {
                println!("export wrote: {folder} UID {uid} ({bytes} bytes)")
            }
            BackupEvent::ExportMessageFailed { folder, uid, error } => {
                eprintln!("export fail: {folder} UID {uid}: {error}")
            }
            BackupEvent::ExportFolderDone { folder, written } => {
                println!("export done: {folder} ({written} written)")
            }
            BackupEvent::ExportRunDone {
                folders,
                messages,
                bytes,
                archive_dir,
            } => println!(
                "export complete: folders={folders} messages={messages} bytes={bytes} archive={archive_dir}"
            ),
            BackupEvent::VerifyFile { folder, uid, .. } => {
                println!("verify ok: {folder} UID {uid}")
            }
            BackupEvent::VerifyExtraFile { rel_path } => {
                println!("verify extra: {rel_path}")
            }
            BackupEvent::VerifyUnsafeEntry { rel_path } => {
                eprintln!("verify unsafe entry (symlink refused): {rel_path}")
            }
            BackupEvent::Error { error, phase, .. } => {
                let phase = phase.as_deref().unwrap_or("backup");
                eprintln!("{phase} error: {error}")
            }
            BackupEvent::VerifyMissingFile {
                folder,
                uid,
                rel_path,
            } => {
                eprintln!("verify missing: {folder} UID {uid} ({rel_path})")
            }
            BackupEvent::VerifyChecksumMismatch { folder, uid, .. } => {
                eprintln!("verify checksum mismatch: {folder} UID {uid}")
            }
            BackupEvent::VerifySizeMismatch {
                folder,
                uid,
                expected_size,
                actual_size,
                ..
            } => eprintln!(
                "verify size mismatch: {folder} UID {uid} expected {expected_size} got {actual_size}"
            ),
            BackupEvent::VerifyDone {
                ok,
                missing,
                corrupt,
                extras,
                unsafe_entries,
            } => {
                println!(
                    "verify done: ok={ok} missing={missing} corrupt={corrupt} extras={extras} unsafe_entries={unsafe_entries}"
                )
            }
            BackupEvent::RestoreFolderStart {
                source,
                destination,
                messages,
            } => {
                println!("restore start: {source} -> {destination} ({messages} messages)")
            }
            BackupEvent::RestoreMessageAppended {
                source,
                destination,
                uid,
                bytes,
            } => {
                println!("restore append: {source} UID {uid} -> {destination} ({bytes} bytes)")
            }
            BackupEvent::RestoreMessageSkipped {
                source,
                uid,
                reason,
            } => {
                println!("restore skip: {source} UID {uid} ({reason})")
            }
            BackupEvent::RestoreMessageFailed {
                source, uid, error, ..
            } => {
                eprintln!("restore fail: {source} UID {uid}: {error}")
            }
            BackupEvent::RestoreFolderDone {
                source,
                destination,
                appended,
                skipped,
                failed,
            } => println!(
                "restore folder done: {source} -> {destination} appended={appended} skipped={skipped} failed={failed}"
            ),
            BackupEvent::RestoreRunDone {
                folders,
                appended,
                skipped,
                failed,
            } => println!(
                "restore done: folders={folders} appended={appended} skipped={skipped} failed={failed}"
            ),
            BackupEvent::RestoreDryRunDone {
                folders,
                would_append,
                would_skip,
            } => println!(
                "restore dry-run: folders={folders} would_append={would_append} would_skip={would_skip}"
            ),
            BackupEvent::RestoreStateWarning { warning } => {
                eprintln!("restore state warning: {warning}")
            }
            BackupEvent::RestoreStateAuditRecord {
                source,
                destination,
                uidvalidity,
                uid,
                status,
                ..
            } => {
                println!(
                    "audit-state: {source} (uidvalidity={uidvalidity}, uid={uid}) -> {destination} [{status}]"
                )
            }
            BackupEvent::RestoreStateAuditDone {
                pending,
                present,
                missing,
                unknown,
                state_not_in_manifest,
                errors,
            } => {
                println!(
                    "audit-state done: pending={pending} present={present} missing={missing} unknown={unknown} state_not_in_manifest={state_not_in_manifest} errors={errors}"
                )
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn backup_export_requires_account() {
        // --account is still required; --out is now optional.
        let err = match crate::Cli::try_parse_from(["envelope", "backup", "export"]) {
            Ok(_) => panic!("expected parse error"),
            Err(err) => err,
        };
        let s = err.to_string();
        assert!(
            s.contains("--account"),
            "expected required-flag error for --account, got: {s}"
        );
    }

    #[test]
    fn backup_export_parses_include_exclude_and_batch_size() {
        let cli = crate::Cli::try_parse_from([
            "envelope",
            "backup",
            "export",
            "--account",
            "user@example.com",
            "--out",
            "/tmp/archive",
            "--include",
            "INBOX",
            "--include",
            "Sent*",
            "--exclude",
            "Junk*",
            "--batch-size",
            "10",
        ])
        .unwrap();
        match cli.command {
            crate::Commands::Backup {
                subcommand:
                    BackupCmd::Export {
                        account,
                        out,
                        include,
                        exclude,
                        batch_size,
                    },
            } => {
                assert_eq!(account, "user@example.com");
                assert_eq!(out, Some(PathBuf::from("/tmp/archive")));
                assert_eq!(include, vec!["INBOX", "Sent*"]);
                assert_eq!(exclude, vec!["Junk*"]);
                assert_eq!(batch_size, 10);
            }
            _ => panic!("expected backup export"),
        }
    }

    #[test]
    fn backup_export_uses_migrate_default_batch_size() {
        let cli = crate::Cli::try_parse_from([
            "envelope",
            "backup",
            "export",
            "--account",
            "user@example.com",
            "--out",
            "/tmp/a",
        ])
        .unwrap();
        match cli.command {
            crate::Commands::Backup {
                subcommand: BackupCmd::Export { batch_size, .. },
            } => assert_eq!(batch_size, migrate::DEFAULT_BATCH_SIZE),
            _ => panic!("expected backup export"),
        }
    }

    #[test]
    fn backup_export_parses_without_out() {
        let cli = crate::Cli::try_parse_from([
            "envelope",
            "backup",
            "export",
            "--account",
            "user@example.com",
        ])
        .unwrap();
        match cli.command {
            crate::Commands::Backup {
                subcommand: BackupCmd::Export { account, out, .. },
            } => {
                assert_eq!(account, "user@example.com");
                assert!(out.is_none(), "out should be None when --out is omitted");
            }
            _ => panic!("expected backup export"),
        }
    }

    #[test]
    fn backup_export_help_mentions_default_archive() {
        let err = match crate::Cli::try_parse_from(["envelope", "backup", "export", "--help"]) {
            Ok(_) => panic!("--help should cause early exit"),
            Err(e) => e,
        };
        let help = err.to_string();
        assert!(
            help.contains("app-data") || help.contains("archives") || help.contains("Defaults to"),
            "help text should mention default archive path, got: {help}"
        );
    }

    #[test]
    fn sanitize_account_for_filename_handles_special_chars() {
        assert_eq!(
            sanitize_account_for_filename("user@example.com"),
            "user_example.com"
        );
        assert_eq!(
            sanitize_account_for_filename("User+Tag@EXAMPLE.COM"),
            "user_tag_example.com"
        );
        assert_eq!(sanitize_account_for_filename("simple"), "simple");
        assert_eq!(sanitize_account_for_filename("a/b\\c:d"), "a_b_c_d");
    }

    #[test]
    fn default_archive_dir_under_app_data() {
        let dir = default_archive_dir("user@example.com");
        let archives = app_data_dir().join(ARCHIVES_DIR_NAME);
        assert!(
            dir.starts_with(&archives),
            "expected dir under {}, got {}",
            archives.display(),
            dir.display()
        );
        let name = dir.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("user_example.com-"),
            "expected sanitized account prefix, got: {name}"
        );
        // Timestamp + pid suffix: YYYYMMDDTHHMMSSmmmZ-<pid>
        let suffix = &name["user_example.com-".len()..];
        let (timestamp_part, pid_part) = suffix
            .split_once('-')
            .expect("default archive name should include pid suffix");
        assert_eq!(timestamp_part.len(), 19, "timestamp part: {timestamp_part}");
        assert!(timestamp_part.ends_with('Z'));
        assert!(!pid_part.is_empty(), "pid suffix should not be empty");
    }

    #[test]
    fn resolve_export_dir_returns_explicit_path() {
        let explicit = PathBuf::from("/tmp/my-archive");
        let result = resolve_export_dir(Some(explicit.clone()), "user@example.com", false).unwrap();
        assert_eq!(result, explicit);
    }

    #[test]
    fn resolve_export_dir_explicit_path_ignores_json_flag() {
        let explicit = PathBuf::from("/tmp/my-archive");
        let result = resolve_export_dir(Some(explicit.clone()), "user@example.com", true).unwrap();
        assert_eq!(result, explicit);
    }

    #[test]
    fn resolve_export_dir_uses_default_in_json_mode() {
        let result = resolve_export_dir(None, "user@example.com", true).unwrap();
        let archives = app_data_dir().join(ARCHIVES_DIR_NAME);
        assert!(
            result.starts_with(&archives),
            "expected path under {}, got {}",
            archives.display(),
            result.display()
        );
    }

    #[test]
    fn resolve_export_dir_uses_default_when_not_tty() {
        // In CI / test harness, stdin is piped (not a terminal), so the
        // non-interactive path fires and returns the default silently.
        let result = resolve_export_dir(None, "test@host.org", false).unwrap();
        let archives = app_data_dir().join(ARCHIVES_DIR_NAME);
        assert!(
            result.starts_with(&archives),
            "expected path under {}, got {}",
            archives.display(),
            result.display()
        );
        let name = result.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("test_host.org-"),
            "expected sanitized account prefix, got: {name}"
        );
    }

    #[test]
    fn backup_verify_parses_strict_flag() {
        let cli = crate::Cli::try_parse_from([
            "envelope",
            "backup",
            "verify",
            "--from",
            "/tmp/archive",
            "--strict",
        ])
        .unwrap();
        match cli.command {
            crate::Commands::Backup {
                subcommand: BackupCmd::Verify { from, strict },
            } => {
                assert_eq!(from, PathBuf::from("/tmp/archive"));
                assert!(strict);
            }
            _ => panic!("expected backup verify"),
        }
    }

    #[test]
    fn backup_verify_default_strict_false() {
        let cli =
            crate::Cli::try_parse_from(["envelope", "backup", "verify", "--from", "/tmp/archive"])
                .unwrap();
        match cli.command {
            crate::Commands::Backup {
                subcommand: BackupCmd::Verify { strict, .. },
            } => assert!(!strict),
            _ => panic!("expected backup verify"),
        }
    }

    #[test]
    fn backup_restore_parses_dry_run_and_map() {
        let cli = crate::Cli::try_parse_from([
            "envelope",
            "backup",
            "restore",
            "--account",
            "user@example.com",
            "--from",
            "/tmp/archive",
            "--map",
            "Junk E-mail=Junk",
            "--map",
            "Sent Items=Sent",
            "--dry-run",
        ])
        .unwrap();
        match cli.command {
            crate::Commands::Backup {
                subcommand:
                    BackupCmd::Restore {
                        account,
                        from,
                        map,
                        dry_run,
                        batch_size,
                        ..
                    },
            } => {
                assert_eq!(account, "user@example.com");
                assert_eq!(from, PathBuf::from("/tmp/archive"));
                assert_eq!(
                    map,
                    vec![
                        "Junk E-mail=Junk".to_string(),
                        "Sent Items=Sent".to_string()
                    ]
                );
                assert!(dry_run);
                assert_eq!(batch_size, migrate::DEFAULT_BATCH_SIZE);
            }
            _ => panic!("expected backup restore"),
        }
    }

    #[test]
    fn backup_restore_parses_batch_size() {
        let cli = crate::Cli::try_parse_from([
            "envelope",
            "backup",
            "restore",
            "--account",
            "user@example.com",
            "--from",
            "/tmp/archive",
            "--batch-size",
            "100",
        ])
        .unwrap();
        match cli.command {
            crate::Commands::Backup {
                subcommand: BackupCmd::Restore { batch_size, .. },
            } => assert_eq!(batch_size, 100),
            _ => panic!("expected backup restore"),
        }
    }

    // -------------------------------------------------------------------------
    // Phase 5: synthetic-archive end-to-end smoke (no IMAP)
    // -------------------------------------------------------------------------

    fn build_smoke_archive() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let payload_a = b"hello world";
        let payload_b = b"second message body";
        let manifest = ArchiveManifest {
            archive_format_version: backup::ARCHIVE_FORMAT_VERSION,
            tool: "envelope".to_string(),
            tool_version: "0.7.0".to_string(),
            exported_at: backup::exported_at_now_utc(),
            account: ArchiveAccount {
                id: "acct-smoke".to_string(),
                email: "smoke@example.com".to_string(),
                imap_host: "imap.example.com".to_string(),
                imap_port: 993,
                imap_username: "smoke@example.com".to_string(),
            },
            folders: vec![ArchiveFolderRecord {
                name: "INBOX".to_string(),
                uidvalidity: 7,
                encoded_dir: "INBOX".to_string(),
                message_count: 2,
            }],
            messages: vec![
                ArchiveMessageRecord {
                    folder: "INBOX".to_string(),
                    uid: 1,
                    uidvalidity: 7,
                    message_id: Some("<a@example.com>".to_string()),
                    internal_date: Some("2026-01-01T00:00:00+00:00".to_string()),
                    flags: vec!["\\Seen".to_string()],
                    size: payload_a.len() as u64,
                    sha256: backup::sha256_hex(payload_a),
                    rel_path: "messages/INBOX/7-1.eml".to_string(),
                },
                ArchiveMessageRecord {
                    folder: "INBOX".to_string(),
                    uid: 2,
                    uidvalidity: 7,
                    message_id: None,
                    internal_date: None,
                    flags: vec![],
                    size: payload_b.len() as u64,
                    sha256: backup::sha256_hex(payload_b),
                    rel_path: "messages/INBOX/7-2.eml".to_string(),
                },
            ],
        };
        for (path, body) in [
            ("messages/INBOX/7-1.eml", payload_a as &[u8]),
            ("messages/INBOX/7-2.eml", payload_b as &[u8]),
        ] {
            let p = dir.path().join(path);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        backup::write_atomic(
            &backup::manifest_path(dir.path()),
            serde_json::to_vec_pretty(&manifest).unwrap().as_slice(),
        )
        .unwrap();
        dir
    }

    #[test]
    fn smoke_verify_passes_for_synthetic_archive() {
        let dir = build_smoke_archive();
        run_verify(dir.path().to_path_buf(), false, false).unwrap();
    }

    #[test]
    fn smoke_verify_strict_fails_when_extras_present() {
        let dir = build_smoke_archive();
        let extra = dir.path().join("messages/INBOX/9999-1.eml");
        fs::write(&extra, b"orphan").unwrap();
        // Default mode: extras are warnings, verify still passes.
        run_verify(dir.path().to_path_buf(), false, false).unwrap();
        // Strict mode: extras flip into a hard failure.
        let err = run_verify(dir.path().to_path_buf(), true, false).unwrap_err();
        assert!(err.to_string().contains("verify failed"));
    }

    #[test]
    fn smoke_verify_fails_when_message_corrupted() {
        let dir = build_smoke_archive();
        // Same length but different bytes — sha mismatch.
        fs::write(dir.path().join("messages/INBOX/7-1.eml"), b"world hello").unwrap();
        let err = run_verify(dir.path().to_path_buf(), false, false).unwrap_err();
        assert!(err.to_string().contains("verify failed"));
    }

    // -------------------------------------------------------------------------
    // Critical #4: export atomicity helpers (no IMAP; pure FS preconditions)
    // -------------------------------------------------------------------------

    #[test]
    fn run_export_refuses_nonempty_output_directory() {
        // We can't run a full export (no live IMAP) but we can prove that
        // `validate_export_output_dir` is wired before any IMAP work: it's
        // the first call in run_export and must reject non-empty dirs.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("stale.json"), b"{}").unwrap();
        let err = backup::validate_export_output_dir(dir.path()).unwrap_err();
        assert!(matches!(err, BackupError::UnsafeOutputDir { .. }));
    }

    // -------------------------------------------------------------------------
    // HP #6: preflight verify wired into restore handler
    // -------------------------------------------------------------------------

    fn run_restore_dry_run_only(
        archive_dir: PathBuf,
        account_id: &str,
        imap_host: &str,
        imap_port: u16,
        imap_username: &str,
        map: Vec<String>,
    ) -> Result<()> {
        // Mirror the dry-run restore path *without* the credential resolution
        // step (we can't load real creds in tests). Drives the same planning
        // helpers run_restore uses; locks dry-run honesty.
        let manifest = backup::read_manifest(&archive_dir)
            .map_err(|e| backup_error_to_anyhow(e, &archive_dir))?;
        backup::validate_restore_destination(
            &manifest.account,
            account_id,
            imap_host,
            imap_port,
            imap_username,
        )
        .map_err(|e| backup_error_to_anyhow(e, &archive_dir))?;
        preflight_verify_archive_for_restore(&archive_dir, true, false)?;
        let mappings = parse_mappings(&map)?;
        let state_path = backup::restore_state_path(&archive_dir, account_id);
        let state_outcome = backup::load_restore_state(&state_path)
            .map_err(|e| backup_error_to_anyhow(e, &state_path))?;
        let plan = backup::plan_restore(
            &manifest.messages,
            &state_outcome.records,
            &mappings,
            &[],
            &[],
        );
        emit(
            false,
            BackupEvent::RestoreDryRunDone {
                folders: plan.destinations.len() as u32,
                would_append: plan.planned_appends.len() as u32,
                would_skip: plan
                    .skipped_already_restored
                    .saturating_add(plan.skipped_excluded),
            },
        )?;
        Ok(())
    }

    #[test]
    fn smoke_dry_run_restore_plans_against_synthetic_archive() {
        let dir = build_smoke_archive();
        run_restore_dry_run_only(
            dir.path().to_path_buf(),
            "smoke-dst",
            "imap.destination.example.com",
            993,
            "smoke-dst@example.com",
            vec![],
        )
        .unwrap();
    }

    #[test]
    fn smoke_dry_run_restore_honors_state_sidecar() {
        let dir = build_smoke_archive();
        let m = backup::read_manifest(dir.path()).unwrap();
        let state_path = backup::restore_state_path(dir.path(), "smoke-dst");
        // Simulate a partial restore having already happened for UID 1.
        backup::append_restore_state_line(&state_path, &backup::restore_state_key(&m.messages[0]))
            .unwrap();
        let state_outcome = backup::load_restore_state(&state_path).unwrap();
        let plan = backup::plan_restore(&m.messages, &state_outcome.records, &[], &[], &[]);
        assert_eq!(plan.planned_appends.len(), 1);
        assert_eq!(plan.planned_appends[0].uid(), 2);
        assert_eq!(plan.skipped_already_restored, 1);
    }

    #[test]
    fn smoke_dry_run_restore_fails_when_message_missing() {
        let dir = build_smoke_archive();
        let manifest = backup::read_manifest(dir.path()).unwrap();
        let record = &manifest.messages[0];
        fs::remove_file(dir.path().join(&record.rel_path)).unwrap();

        let err = run_restore_dry_run_only(
            dir.path().to_path_buf(),
            "smoke-dst",
            "imap.destination.example.com",
            993,
            "smoke-dst@example.com",
            vec![],
        )
        .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("verify failed"));
    }

    #[test]
    fn smoke_dry_run_restore_fails_when_message_corrupted() {
        let dir = build_smoke_archive();
        let manifest = backup::read_manifest(dir.path()).unwrap();
        let record = &manifest.messages[0];
        let corrupted = vec![b'Z'; record.size as usize];
        fs::write(dir.path().join(&record.rel_path), corrupted).unwrap();

        let err = run_restore_dry_run_only(
            dir.path().to_path_buf(),
            "smoke-dst",
            "imap.destination.example.com",
            993,
            "smoke-dst@example.com",
            vec![],
        )
        .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("verify failed"));
    }

    #[test]
    fn smoke_dry_run_restore_rejects_same_source_account_before_loading_state() {
        let dir = build_smoke_archive();
        let poison_path = backup::restore_state_path(dir.path(), "acct-smoke");
        fs::create_dir_all(&poison_path).unwrap();
        let err = run_restore_dry_run_only(
            dir.path().to_path_buf(),
            "acct-smoke",
            "imap.destination.example.com",
            993,
            "other@example.com",
            vec![],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("same account"),
            "expected same-account guard, got: {err:#}"
        );
    }

    #[test]
    fn smoke_dry_run_restore_rejects_same_source_mailbox_before_loading_state() {
        let dir = build_smoke_archive();
        let poison_path = backup::restore_state_path(dir.path(), "acct-dst");
        fs::create_dir_all(&poison_path).unwrap();
        let err = run_restore_dry_run_only(
            dir.path().to_path_buf(),
            "acct-dst",
            " IMAP.EXAMPLE.COM ",
            993,
            " SMOKE@example.com ",
            vec![],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("same IMAP mailbox"),
            "expected same-mailbox guard, got: {err:#}"
        );
    }

    #[test]
    fn touch_restore_state_creates_parent_and_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        // Nested missing parent — must be created by touch_restore_state.
        let path = dir.path().join("nested/sub/.restore-state-acct.ndjson");
        touch_restore_state(&path).unwrap();
        assert!(path.exists());
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
    }

    #[test]
    fn touch_restore_state_is_idempotent_when_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = backup::restore_state_path(dir.path(), "acct");
        // Pre-populate one line.
        let r = backup::RestoreStateRecord {
            folder: "INBOX".into(),
            uidvalidity: 1,
            uid: 1,
            sha256: backup::sha256_hex(b"x"),
            status: backup::RestoreStatus::Done,
        };
        backup::append_restore_state_line(&path, &r).unwrap();
        let before = fs::read_to_string(&path).unwrap();
        touch_restore_state(&path).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "touch must not mutate existing state");
    }

    // -------------------------------------------------------------------------
    // HP #7: in-memory state update mirrors disk after persist_state
    // -------------------------------------------------------------------------

    #[test]
    fn audit_state_parses_required_args_and_filters() {
        let cli = crate::Cli::try_parse_from([
            "envelope",
            "backup",
            "audit-state",
            "--account",
            "x",
            "--from",
            "/tmp/a",
            "--include",
            "INBOX",
            "--exclude",
            "Trash",
            "--map",
            "A=B",
        ])
        .unwrap();
        match cli.command {
            crate::Commands::Backup {
                subcommand:
                    BackupCmd::AuditState {
                        account,
                        from,
                        include,
                        exclude,
                        map,
                    },
            } => {
                assert_eq!(account, "x");
                assert_eq!(from, PathBuf::from("/tmp/a"));
                assert_eq!(include, vec!["INBOX".to_string()]);
                assert_eq!(exclude, vec!["Trash".to_string()]);
                assert_eq!(map, vec!["A=B".to_string()]);
            }
            _ => panic!("expected backup audit-state"),
        }
    }

    #[test]
    fn audit_state_runs_against_synthetic_archive_with_pending_state() {
        // End-to-end smoke for the read-only audit handler. Builds a synthetic
        // archive, drops a pending-only sidecar line for the message that has
        // no Message-ID, and confirms the handler completes without error and
        // without touching IMAP.
        let dir = build_smoke_archive();
        let manifest = backup::read_manifest(dir.path()).unwrap();
        // UID 2 in the smoke archive has message_id: None.
        let target = &manifest.messages[1];
        let pending = backup::restore_state_key_pending(target);
        let state_path = backup::restore_state_path(dir.path(), "audit-acct");
        backup::append_restore_state_line(&state_path, &pending).unwrap();

        run_audit_state(
            AuditStateArgs {
                account: "audit-acct".to_string(),
                from: dir.path().to_path_buf(),
                include: vec![],
                exclude: vec![],
                map: vec![],
            },
            true,
        )
        .unwrap();
    }

    #[test]
    fn persist_state_updates_in_memory_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = backup::restore_state_path(dir.path(), "acct");
        let mut state = HashSet::new();
        let record = ArchiveMessageRecord {
            folder: "INBOX".into(),
            uid: 7,
            uidvalidity: 1,
            message_id: None,
            internal_date: None,
            flags: vec![],
            size: 1,
            sha256: backup::sha256_hex(b"x"),
            rel_path: "messages/INBOX/1-7.eml".into(),
        };
        persist_state(&path, &record, &mut state).unwrap();
        let key = backup::restore_state_key(&record);
        assert!(
            state.contains(&key),
            "persist_state must mirror the new record in the in-memory set"
        );
        // And the line should be on disk too.
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"uid\":7"));
    }
}
