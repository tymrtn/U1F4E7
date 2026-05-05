// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! `envelope evidence` CLI handler.
//!
//! Collection is intentionally read-only against the source mailbox: EXAMINE
//! opens the folder and BODY.PEEK[] fetches raw RFC822 bytes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use envelope_email_store::CredentialBackend;
use envelope_email_transport::backup;
use envelope_email_transport::evidence as evidence_core;
use envelope_email_transport::evidence::{
    CollectionSpec, EvidenceAccount, EvidenceEvent, EvidenceManifest, EvidenceMessageInput,
    EvidenceMessageRecord, EvidenceQueryFilters, EvidenceStats, EvidenceWarning,
    SourceStoreProvenance, ThreadExpansionMode,
};
use envelope_email_transport::{imap, migrate, provider};

use super::common::setup_credentials;
use super::paths;
use crate::EvidenceCmd;

const TOOL_NAME: &str = "envelope";
const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

struct CollectArgs {
    account: String,
    folder: String,
    query: Option<String>,
    include_thread: bool,
    max_thread_messages: usize,
    out: PathBuf,
    filters: EvidenceQueryFilters,
}

#[tokio::main]
pub async fn run(
    subcommand: EvidenceCmd,
    json_output: bool,
    backend: CredentialBackend,
) -> Result<()> {
    match subcommand {
        EvidenceCmd::Collect {
            account,
            folder,
            query,
            include_thread,
            max_thread_messages,
            out,
            from_address,
            to_address,
            subject,
            since,
            before,
            body,
            keyword,
        } => {
            run_collect(
                CollectArgs {
                    account,
                    folder,
                    query,
                    include_thread,
                    max_thread_messages,
                    out,
                    filters: EvidenceQueryFilters {
                        from_address,
                        to_address,
                        subject,
                        since,
                        before,
                        body,
                        keyword,
                    },
                },
                json_output,
                backend,
            )
            .await
        }
        EvidenceCmd::Verify { from, strict } => run_verify(from, strict, json_output),
    }
}

async fn run_collect(
    args: CollectArgs,
    json_output: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let query = evidence_core::compile_search_query(args.query.as_deref(), &args.filters)
        .map_err(anyhow::Error::msg)?;

    backup::validate_export_output_dir(&args.out)
        .map_err(|e| anyhow::anyhow!("{e} (at {})", args.out.display()))?;

    let path_report = paths::collect_report(backend);
    let source_store = source_store_from_paths(path_report);

    let (_db, src) = setup_credentials(Some(&args.account), backend)?;
    let mut client = imap::connect(&src)
        .await
        .context("source IMAP connection failed")?;

    let selected = imap::examine_folder_for_evidence(&mut client, &args.folder)
        .await
        .with_context(|| format!("EXAMINE {}", args.folder))?;
    let uidvalidity = selected.uidvalidity_key();

    emit(
        json_output,
        EvidenceEvent::CollectFolderStart {
            folder: args.folder.clone(),
            query: query.clone(),
        },
    )?;

    let matched_uids = imap::evidence_search_selected_uids(&mut client, &query)
        .await
        .with_context(|| format!("UID SEARCH {query}"))?;
    let matched_set: HashSet<u32> = matched_uids.iter().copied().collect();
    let mut raw_by_uid = fetch_raw_uids(&mut client, &args.folder, &matched_uids)
        .await
        .with_context(|| format!("fetch matched messages from {}", args.folder))?;
    let fetched_uids: HashSet<u32> = raw_by_uid.keys().copied().collect();
    let mut warnings = evidence_core::missing_uid_fetch_warnings(
        &matched_uids,
        &fetched_uids,
        "returned by UID SEARCH but absent from UID FETCH results",
    );

    if args.include_thread {
        warnings.extend(
            expand_thread_raw_messages(
                &mut client,
                &args.folder,
                &matched_set,
                &mut raw_by_uid,
                args.max_thread_messages,
            )
            .await
            .with_context(|| format!("expand header thread in {}", args.folder))?,
        );
    }

    let mut header_messages: Vec<_> = raw_by_uid
        .values()
        .map(|raw| evidence_core::header_thread_message_from_rfc822(raw.uid, &raw.rfc822))
        .collect();
    header_messages.sort_by_key(|message| message.uid);

    let expansion_mode = if args.include_thread {
        ThreadExpansionMode::FullThread {
            max_messages: args.max_thread_messages,
        }
    } else {
        ThreadExpansionMode::MatchedOnly
    };
    let expansion =
        evidence_core::expand_header_threads(&header_messages, &matched_set, expansion_mode);
    let expansion_by_uid: HashMap<u32, _> = expansion
        .included
        .iter()
        .map(|item| (item.uid, item.clone()))
        .collect();

    let mut message_records = Vec::new();
    let mut message_bytes = HashMap::new();
    let mut total_bytes = 0u64;
    let mut included_uids: Vec<u32> = expansion_by_uid.keys().copied().collect();
    included_uids.sort_unstable();

    for uid in included_uids {
        let Some(raw) = raw_by_uid.get(&uid) else {
            continue;
        };
        let expanded = &expansion_by_uid[&uid];
        let record = evidence_core::message_record_from_rfc822(EvidenceMessageInput {
            folder: &args.folder,
            uidvalidity,
            uid: raw.uid,
            internal_date: raw.internal_date.map(|date| date.to_rfc3339()),
            flags: raw.flags.clone(),
            rfc822: &raw.rfc822,
            query_matched: expanded.query_matched,
            inclusion_reason: expanded.inclusion_reason.clone(),
            thread_id: expanded.thread_id.clone(),
        });
        total_bytes = total_bytes.saturating_add(record.size);
        emit(
            json_output,
            EvidenceEvent::CollectMessageWritten {
                folder: args.folder.clone(),
                uid: record.uid,
                bytes: record.size,
                sha256: record.sha256.clone(),
                inclusion_reason: record.inclusion_reason.clone(),
            },
        )?;
        message_bytes.insert(record.rel_path.clone(), raw.rfc822.clone());
        message_records.push(record);
    }

    warnings.extend(expansion.warnings);
    let manifest = build_manifest(
        &src,
        &args.folder,
        uidvalidity,
        &query,
        args.query.clone(),
        args.filters.clone(),
        args.include_thread,
        args.max_thread_messages,
        source_store,
        message_records,
        warnings,
        total_bytes,
    );

    evidence_core::write_evidence_bundle(&args.out, &manifest, &message_bytes)
        .with_context(|| format!("write evidence bundle {}", args.out.display()))?;

    emit(
        json_output,
        EvidenceEvent::CollectDone {
            folder: args.folder.clone(),
            matched: matched_set.len() as u32,
            included: manifest.messages.len() as u32,
            bytes: total_bytes,
            bundle_dir: args.out.display().to_string(),
        },
    )?;

    Ok(())
}

fn run_verify(from: PathBuf, strict: bool, json_output: bool) -> Result<()> {
    let outcome = evidence_core::verify_bundle(&from, strict)
        .with_context(|| format!("verify evidence bundle {}", from.display()))?;

    for missing in &outcome.missing {
        emit(
            json_output,
            EvidenceEvent::VerifyMissingFile {
                folder: missing.folder.clone(),
                uid: missing.uid,
                rel_path: missing.rel_path.clone(),
            },
        )?;
    }
    for corrupt in &outcome.corrupt {
        match corrupt {
            evidence_core::EvidenceCorruptFile::SizeMismatch {
                folder,
                uid,
                rel_path,
                expected_size,
                actual_size,
            } => emit(
                json_output,
                EvidenceEvent::VerifySizeMismatch {
                    folder: folder.clone(),
                    uid: *uid,
                    rel_path: rel_path.clone(),
                    expected_size: *expected_size,
                    actual_size: *actual_size,
                },
            )?,
            evidence_core::EvidenceCorruptFile::ChecksumMismatch {
                folder,
                uid,
                rel_path,
                expected_sha256,
                actual_sha256,
            } => emit(
                json_output,
                EvidenceEvent::VerifyChecksumMismatch {
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
            EvidenceEvent::VerifyExtraFile {
                rel_path: extra.clone(),
            },
        )?;
    }
    if outcome.top_level_digest_mismatch {
        emit(json_output, EvidenceEvent::VerifyBundleDigestMismatch)?;
    }

    emit(
        json_output,
        EvidenceEvent::VerifyDone {
            ok: outcome.ok,
            missing: outcome.missing.len() as u32,
            corrupt: outcome.corrupt.len() as u32,
            extras: outcome.extras.len() as u32,
            top_level_digest_mismatch: outcome.top_level_digest_mismatch,
        },
    )?;

    if !outcome.ok {
        bail!(
            "verify failed: missing={} corrupt={} extras={} top_level_digest_mismatch={} (strict={})",
            outcome.missing.len(),
            outcome.corrupt.len(),
            outcome.extras.len(),
            outcome.top_level_digest_mismatch,
            strict
        );
    }
    Ok(())
}

async fn fetch_raw_uids(
    client: &mut imap::ImapClient,
    folder: &str,
    uids: &[u32],
) -> Result<HashMap<u32, imap::RawMessage>> {
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out = HashMap::new();
    if sorted.is_empty() {
        return Ok(out);
    }
    for uid_set in migrate::uid_sequence_set_batches(&sorted, migrate::DEFAULT_BATCH_SIZE) {
        for raw in imap::fetch_raw_messages_selected_uid_set(client, folder, &uid_set).await? {
            out.insert(raw.uid, raw);
        }
    }
    Ok(out)
}

async fn fetch_missing_raw_uids(
    client: &mut imap::ImapClient,
    folder: &str,
    raw_by_uid: &mut HashMap<u32, imap::RawMessage>,
    uids: &[u32],
) -> Result<Vec<u32>> {
    let missing: Vec<u32> = uids
        .iter()
        .copied()
        .filter(|uid| !raw_by_uid.contains_key(uid))
        .collect();
    let fetched = fetch_raw_uids(client, folder, &missing).await?;
    let mut fetched_uids = Vec::new();
    for (uid, raw) in fetched {
        fetched_uids.push(uid);
        raw_by_uid.insert(uid, raw);
    }
    Ok(fetched_uids)
}

async fn expand_thread_raw_messages(
    client: &mut imap::ImapClient,
    folder: &str,
    matched_set: &HashSet<u32>,
    raw_by_uid: &mut HashMap<u32, imap::RawMessage>,
    max_thread_messages: usize,
) -> Result<Vec<EvidenceWarning>> {
    let mut queued: VecDeque<u32> = matched_set.iter().copied().collect();
    let mut visited = HashSet::new();
    let mut warnings = Vec::new();
    let mut limit_warning_recorded = false;
    let mut missing_fetch_warning_uids = HashSet::new();

    while let Some(uid) = queued.pop_front() {
        if !visited.insert(uid) {
            continue;
        }
        let Some(raw) = raw_by_uid.get(&uid) else {
            continue;
        };
        let header = evidence_core::header_thread_message_from_rfc822(raw.uid, &raw.rfc822);
        let mut discovered = Vec::new();

        for parent_id in parent_ids_from_header(&header) {
            let parent_uids =
                imap::evidence_search_selected_header_uids(client, "Message-ID", &parent_id)
                    .await?;
            discovered.extend(parent_uids);
        }

        if let Some(message_id) = header.message_id.as_deref() {
            let direct_children =
                imap::evidence_search_selected_header_uids(client, "In-Reply-To", message_id)
                    .await?;
            discovered.extend(direct_children);
            let reference_children =
                imap::evidence_search_selected_header_uids(client, "References", message_id)
                    .await?;
            discovered.extend(reference_children);
        }

        discovered.sort_unstable();
        discovered.dedup();
        let loaded_uids: HashSet<u32> = raw_by_uid.keys().copied().collect();
        let plan = evidence_core::bounded_thread_fetch_candidates(
            &discovered,
            &loaded_uids,
            max_thread_messages,
        );
        if plan.limit_reached && !limit_warning_recorded {
            warnings.push(evidence_core::thread_expansion_limit_warning(
                max_thread_messages,
            ));
            limit_warning_recorded = true;
        }

        let fetched = fetch_missing_raw_uids(client, folder, raw_by_uid, &plan.uids).await?;
        let fetched_set: HashSet<u32> = fetched.iter().copied().collect();
        for warning in evidence_core::missing_uid_fetch_warnings(
            &plan.uids,
            &fetched_set,
            "returned by header UID SEARCH during thread expansion but absent from UID FETCH results",
        ) {
            if let Some(uid) = warning.uid
                && !missing_fetch_warning_uids.insert(uid)
            {
                continue;
            }
            warnings.push(warning);
        }
        for fetched_uid in fetched {
            queued.push_back(fetched_uid);
        }
    }

    Ok(warnings)
}

fn parent_ids_from_header(header: &evidence_core::HeaderThreadMessage) -> Vec<String> {
    let mut out = Vec::new();
    for value in &header.references {
        out.extend(evidence_core::extract_message_ids(value));
    }
    if let Some(in_reply_to) = header.in_reply_to.as_deref() {
        out.extend(evidence_core::extract_message_ids(in_reply_to));
    }
    let mut seen = HashSet::new();
    out.retain(|id| seen.insert(id.clone()));
    out
}

#[allow(clippy::too_many_arguments)]
fn build_manifest(
    src: &envelope_email_store::AccountWithCredentials,
    folder: &str,
    uidvalidity: u32,
    compiled_query: &str,
    raw_query: Option<String>,
    filters: EvidenceQueryFilters,
    include_thread: bool,
    max_thread_messages: usize,
    source_store: SourceStoreProvenance,
    messages: Vec<EvidenceMessageRecord>,
    warnings: Vec<EvidenceWarning>,
    total_bytes: u64,
) -> EvidenceManifest {
    let provider = provider::detect_provider(&[folder.to_string()]);
    let stats = EvidenceStats {
        matched_messages: messages
            .iter()
            .filter(|message| message.query_matched)
            .count() as u32,
        included_messages: messages.len() as u32,
        written_messages: messages.len() as u32,
        total_bytes,
        warnings: warnings.len() as u32,
    };
    EvidenceManifest {
        evidence_format_version: evidence_core::EVIDENCE_FORMAT_VERSION,
        tool: TOOL_NAME.to_string(),
        tool_version: TOOL_VERSION.to_string(),
        exported_at_utc: evidence_core::exported_at_now_utc(),
        account: EvidenceAccount {
            id: src.account.id.clone(),
            email: format!("{}@{}", src.account.username, src.account.domain),
            imap_host: Some(src.account.imap_host.clone()),
            imap_port: Some(src.account.imap_port),
            imap_username: Some(src.effective_imap_username().to_string()),
        },
        provider: Some(provider.to_string()),
        source_store,
        collection_spec: CollectionSpec {
            folder: folder.to_string(),
            compiled_query: compiled_query.to_string(),
            raw_query,
            filters,
            include_thread,
            max_thread_messages: include_thread.then_some(max_thread_messages),
        },
        folders: vec![evidence_core::EvidenceFolderRecord {
            name: folder.to_string(),
            uidvalidity,
            encoded_dir: evidence_core::encode_folder_for_disk(folder),
            message_count: messages.len() as u32,
        }],
        messages,
        warnings,
        stats,
    }
}

fn source_store_from_paths(report: paths::PathsReport) -> SourceStoreProvenance {
    SourceStoreProvenance {
        credential_backend: report.credential_backend,
        app_data_dir: report.app_data_dir,
        database_path: report.database_path,
        home: report.home,
        warnings: report.warnings,
    }
}

fn emit(json_output: bool, event: EvidenceEvent) -> Result<()> {
    if json_output {
        println!("{}", serde_json::to_string(&event)?);
        return Ok(());
    }

    match &event {
        EvidenceEvent::CollectFolderStart { folder, query } => {
            println!("evidence collect start: {folder} ({query})")
        }
        EvidenceEvent::CollectMessageWritten {
            folder, uid, bytes, ..
        } => {
            println!("evidence wrote: {folder} UID {uid} ({bytes} bytes)")
        }
        EvidenceEvent::CollectDone {
            folder,
            matched,
            included,
            bytes,
            bundle_dir,
        } => {
            println!(
                "evidence collect done: {folder} matched={matched} included={included} bytes={bytes} bundle={bundle_dir}"
            )
        }
        EvidenceEvent::VerifyMissingFile {
            folder,
            uid,
            rel_path,
        } => {
            eprintln!("evidence verify missing: {folder} UID {uid} {rel_path}")
        }
        EvidenceEvent::VerifyChecksumMismatch {
            folder,
            uid,
            rel_path,
            ..
        } => {
            eprintln!("evidence verify checksum mismatch: {folder} UID {uid} {rel_path}")
        }
        EvidenceEvent::VerifySizeMismatch {
            folder,
            uid,
            rel_path,
            ..
        } => {
            eprintln!("evidence verify size mismatch: {folder} UID {uid} {rel_path}")
        }
        EvidenceEvent::VerifyExtraFile { rel_path } => {
            eprintln!("evidence verify extra: {rel_path}")
        }
        EvidenceEvent::VerifyBundleDigestMismatch => {
            eprintln!("evidence verify bundle digest mismatch")
        }
        EvidenceEvent::VerifyDone {
            ok,
            missing,
            corrupt,
            extras,
            top_level_digest_mismatch,
        } => {
            println!(
                "evidence verify done: ok={ok} missing={missing} corrupt={corrupt} extras={extras} top_level_digest_mismatch={top_level_digest_mismatch}"
            )
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_store_provenance_omits_credential_file_path() {
        let report = paths::PathsReport {
            credential_backend: "file".to_string(),
            credential_file_in_use: true,
            database_path: "/Users/test/.config/envelope-email/envelope.db".to_string(),
            credential_file_path: "/Users/test/.config/envelope-email/credentials.json".to_string(),
            app_data_dir: "/Users/test/.config/envelope-email".to_string(),
            home: Some("/Users/test".to_string()),
            warnings: vec![],
        };

        let provenance = source_store_from_paths(report);
        let rendered = serde_json::to_string_pretty(&provenance).unwrap();
        assert!(rendered.contains("database_path"));
        assert!(rendered.contains("app_data_dir"));
        assert!(!rendered.contains("credential_file_path"));
        assert!(!rendered.contains("credentials.json"));
    }
}
