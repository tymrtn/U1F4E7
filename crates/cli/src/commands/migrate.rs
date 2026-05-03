// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result};
use envelope_email_store::CredentialBackend;
use envelope_email_store::migration::{MigrationKey, MigrationRecord, MigrationScope};
use envelope_email_transport::{imap, migrate};
use std::collections::HashSet;

use super::common::setup_credentials;
use crate::MigrateCmd;

#[tokio::main]
pub async fn run(
    subcommand: MigrateCmd,
    json_output: bool,
    backend: CredentialBackend,
) -> Result<()> {
    match subcommand {
        MigrateCmd::Folders {
            from,
            to,
            include,
            exclude,
        } => {
            let (_src_db, src) = setup_credentials(Some(&from), backend)?;
            let (_dst_db, dst) = setup_credentials(Some(&to), backend)?;
            migrate::validate_distinct_accounts(&src.account.id, &dst.account.id)
                .map_err(anyhow::Error::msg)?;
            migrate::validate_distinct_imap_endpoints(
                &src.account.imap_host,
                src.account.imap_port,
                src.effective_imap_username(),
                &dst.account.imap_host,
                dst.account.imap_port,
                dst.effective_imap_username(),
            )
            .map_err(anyhow::Error::msg)?;
            let mut src_client = imap::connect(&src)
                .await
                .context("source IMAP connection failed")?;
            let folders = imap::list_folder_stats(&mut src_client)
                .await
                .context("source folder listing failed")?;
            let plans: Vec<_> = folders
                .into_iter()
                .filter(|f| migrate::folder_selected(&f.folder, &include, &exclude))
                .map(|f| migrate::FolderPlan {
                    source: f.folder.clone(),
                    destination: f.folder,
                    messages: f.exists,
                })
                .collect();
            if json_output {
                println!("{}", serde_json::to_string_pretty(&plans)?);
            } else {
                for p in &plans {
                    println!(
                        "{} -> {} ({} messages)",
                        p.source, p.destination, p.messages
                    );
                }
                println!("\n{} folder(s) selected", plans.len());
            }
        }
        MigrateCmd::Run {
            from,
            to,
            include,
            exclude,
            dry_run,
            batch_size,
        } => {
            let batch_size =
                migrate::validate_batch_size(batch_size).map_err(anyhow::Error::msg)?;
            let (db, src) = setup_credentials(Some(&from), backend)?;
            let (_dst_db, dst) = setup_credentials(Some(&to), backend)?;
            migrate::validate_distinct_accounts(&src.account.id, &dst.account.id)
                .map_err(anyhow::Error::msg)?;
            migrate::validate_distinct_imap_endpoints(
                &src.account.imap_host,
                src.account.imap_port,
                src.effective_imap_username(),
                &dst.account.imap_host,
                dst.account.imap_port,
                dst.effective_imap_username(),
            )
            .map_err(anyhow::Error::msg)?;
            let mut src_client = imap::connect(&src)
                .await
                .context("source IMAP connection failed")?;
            let mut dst_client = imap::connect(&dst)
                .await
                .context("destination IMAP connection failed")?;
            let folders = imap::list_folder_stats(&mut src_client)
                .await
                .context("source folder listing failed")?;
            let mut total_copied = 0u32;
            let mut total_skipped = 0u32;
            let mut total_failed = 0u32;
            let mut total_folders = 0u32;
            let mut total_already_migrated = 0u32;
            let mut total_already_in_destination = 0u32;
            let mut total_would_copy = 0u32;

            for folder in folders {
                if !migrate::folder_selected(&folder.folder, &include, &exclude) {
                    continue;
                }
                total_folders += 1;
                let src_info = imap::select_folder_info(&mut src_client, &folder.folder).await?;
                let src_uidvalidity = src_info.uidvalidity_key();
                emit(
                    json_output,
                    migrate::ProgressEvent::FolderStart {
                        source: folder.folder.clone(),
                        destination: folder.folder.clone(),
                        messages: src_info.exists,
                    },
                )?;
                if dry_run {
                    let scope = MigrationScope {
                        src_account_id: &src.account.id,
                        dst_account_id: &dst.account.id,
                        src_folder: Some(&folder.folder),
                        src_uidvalidity: Some(src_uidvalidity),
                    };
                    let source_uids = imap::list_selected_uids(&mut src_client).await?;
                    let migrated_uids: HashSet<u32> =
                        db.list_migrated_uids(scope)?.into_iter().collect();
                    let destination_exists = imap::folder_exists(&mut dst_client, &folder.folder)
                        .await
                        .with_context(|| {
                            format!("destination folder listing failed for {}", folder.folder)
                        })?;
                    if destination_exists {
                        imap::select_folder_info(&mut dst_client, &folder.folder)
                            .await
                            .with_context(|| {
                                format!("destination SELECT failed for {}", folder.folder)
                            })?;
                    }

                    let mut already_migrated = 0u32;
                    let mut already_in_destination = 0u32;
                    for uid_set in migrate::uid_sequence_set_batches(&source_uids, batch_size) {
                        let headers = imap::fetch_message_headers_selected_uid_set(
                            &mut src_client,
                            &folder.folder,
                            &uid_set,
                        )
                        .await?;
                        for header in headers {
                            if migrated_uids.contains(&header.uid) {
                                already_migrated += 1;
                                continue;
                            }
                            if destination_exists {
                                if let Some(message_id) = header.message_id.as_deref() {
                                    if imap::find_uid_by_message_id(
                                        &mut dst_client,
                                        &folder.folder,
                                        message_id,
                                    )
                                    .await?
                                    .is_some()
                                    {
                                        already_in_destination += 1;
                                    }
                                }
                            }
                        }
                    }
                    let counts = migrate::dry_run_counts(
                        source_uids.len() as u32,
                        already_migrated,
                        already_in_destination,
                    );
                    emit(
                        json_output,
                        migrate::ProgressEvent::FolderDryRun {
                            source: folder.folder.clone(),
                            destination: folder.folder.clone(),
                            messages: src_info.exists,
                            destination_exists,
                            already_migrated: counts.already_migrated,
                            already_in_destination: counts.already_in_destination,
                            would_copy: counts.would_copy,
                        },
                    )?;
                    total_skipped += counts.already_migrated + counts.already_in_destination;
                    total_already_migrated =
                        total_already_migrated.saturating_add(counts.already_migrated);
                    total_already_in_destination =
                        total_already_in_destination.saturating_add(counts.already_in_destination);
                    total_would_copy = total_would_copy.saturating_add(counts.would_copy);
                    continue;
                }

                let dst_client = &mut dst_client;
                imap::create_folder_if_missing(dst_client, &folder.folder).await?;
                let dst_info = imap::select_folder_info(dst_client, &folder.folder).await?;
                let mut copied = 0u32;
                let mut skipped = 0u32;
                let mut failed = 0u32;

                let source_uids = imap::list_selected_uids(&mut src_client).await?;
                let uid_sets = migrate::uid_sequence_set_batches(&source_uids, batch_size);

                for uid_set in uid_sets {
                    let messages = imap::fetch_raw_messages_selected_uid_set(
                        &mut src_client,
                        &folder.folder,
                        &uid_set,
                    )
                    .await?;
                    for msg in messages {
                        let key = MigrationKey {
                            src_account_id: &src.account.id,
                            dst_account_id: &dst.account.id,
                            src_folder: &folder.folder,
                            src_uidvalidity,
                            src_uid: msg.uid,
                        };
                        if db.is_migrated(key)? {
                            skipped += 1;
                            emit(
                                json_output,
                                migrate::ProgressEvent::MessageSkipped {
                                    source: folder.folder.clone(),
                                    src_uid: msg.uid,
                                    reason: "migration_map".to_string(),
                                },
                            )?;
                            continue;
                        }
                        if let Some(message_id) = msg.message_id.as_deref() {
                            let already_in_destination = imap::find_uid_by_message_id(
                                dst_client,
                                &folder.folder,
                                message_id,
                            )
                            .await?
                            .is_some();
                            if already_in_destination {
                                db.record_migration(MigrationRecord {
                                    key,
                                    dst_folder: &folder.folder,
                                    dst_uidvalidity: dst_info.uid_validity,
                                    dst_uid: None,
                                    message_id: Some(message_id),
                                    size: Some(msg.size as u64),
                                })?;
                                skipped += 1;
                                emit(
                                    json_output,
                                    migrate::ProgressEvent::MessageSkipped {
                                        source: folder.folder.clone(),
                                        src_uid: msg.uid,
                                        reason: "destination_message_id".to_string(),
                                    },
                                )?;
                                continue;
                            }
                        }
                        let flags = migrate::append_flags(&msg.flags);
                        match imap::append_message_with_date(
                            dst_client,
                            &folder.folder,
                            &flags,
                            msg.internal_date,
                            &msg.rfc822,
                        )
                        .await
                        {
                            Ok(()) => {
                                db.record_migration(MigrationRecord {
                                    key,
                                    dst_folder: &folder.folder,
                                    dst_uidvalidity: dst_info.uid_validity,
                                    dst_uid: None,
                                    message_id: msg.message_id.as_deref(),
                                    size: Some(msg.size as u64),
                                })?;
                                copied += 1;
                                emit(
                                    json_output,
                                    migrate::ProgressEvent::MessageCopied {
                                        source: folder.folder.clone(),
                                        destination: folder.folder.clone(),
                                        src_uid: msg.uid,
                                        message_id: msg.message_id,
                                        bytes: msg.size,
                                    },
                                )?;
                            }
                            Err(e) => {
                                failed += 1;
                                emit(
                                    json_output,
                                    migrate::ProgressEvent::MessageFailed {
                                        source: folder.folder.clone(),
                                        destination: folder.folder.clone(),
                                        src_uid: msg.uid,
                                        error: e.to_string(),
                                    },
                                )?;
                            }
                        }
                    }
                }
                total_copied += copied;
                total_skipped += skipped;
                total_failed += failed;
                emit(
                    json_output,
                    migrate::ProgressEvent::FolderDone {
                        source: folder.folder.clone(),
                        destination: folder.folder,
                        copied,
                        skipped,
                        failed,
                    },
                )?;
            }
            if dry_run {
                emit(
                    json_output,
                    migrate::ProgressEvent::RunDryRunDone {
                        folders: total_folders,
                        already_migrated: total_already_migrated,
                        already_in_destination: total_already_in_destination,
                        would_copy: total_would_copy,
                    },
                )?;
            } else {
                emit(
                    json_output,
                    migrate::ProgressEvent::RunDone {
                        folders: total_folders,
                        copied: total_copied,
                        skipped: total_skipped,
                        failed: total_failed,
                    },
                )?;
                fail_if_any_message_failed(total_failed)?;
            }
        }
    }
    Ok(())
}

fn fail_if_any_message_failed(total_failed: u32) -> Result<()> {
    if total_failed > 0 {
        anyhow::bail!("migration completed with {total_failed} failed message(s)");
    }
    Ok(())
}

fn emit(json_output: bool, event: migrate::ProgressEvent) -> Result<()> {
    if json_output {
        println!("{}", serde_json::to_string(&event)?);
    } else {
        match event {
            migrate::ProgressEvent::FolderStart {
                source, messages, ..
            } => println!("Migrating {source} ({messages} messages)"),
            migrate::ProgressEvent::MessageSkipped {
                source,
                src_uid,
                reason,
            } => println!("skip {source} UID {src_uid}: {reason}"),
            migrate::ProgressEvent::MessageCopied {
                source,
                src_uid,
                bytes,
                ..
            } => println!("copy {source} UID {src_uid} ({bytes} bytes)"),
            migrate::ProgressEvent::MessageFailed {
                source,
                src_uid,
                error,
                ..
            } => eprintln!("fail {source} UID {src_uid}: {error}"),
            migrate::ProgressEvent::FolderDryRun {
                source,
                messages,
                destination_exists,
                already_migrated,
                already_in_destination,
                would_copy,
                ..
            } => println!(
                "plan {source}: messages={messages} destination_exists={destination_exists} already_migrated={already_migrated} already_in_destination={already_in_destination} would_copy={would_copy}"
            ),
            migrate::ProgressEvent::FolderDone {
                source,
                copied,
                skipped,
                failed,
                ..
            } => println!("done {source}: copied={copied} skipped={skipped} failed={failed}"),
            migrate::ProgressEvent::RunDone {
                folders,
                copied,
                skipped,
                failed,
            } => println!(
                "done: folders={folders} copied={copied} skipped={skipped} failed={failed}"
            ),
            migrate::ProgressEvent::RunDryRunDone {
                folders,
                already_migrated,
                already_in_destination,
                would_copy,
            } => println!(
                "dry-run done: folders={folders} already_migrated={already_migrated} \
                 already_in_destination={already_in_destination} would_copy={would_copy}"
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn migrate_run_requires_from_and_to() {
        let err = match crate::Cli::try_parse_from(["envelope", "migrate", "run"]) {
            Ok(_) => panic!("expected parse error"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("--from"));
    }

    #[test]
    fn migrate_run_parses_dry_run() {
        let cli = crate::Cli::try_parse_from([
            "envelope",
            "migrate",
            "run",
            "--from",
            "old@example.com",
            "--to",
            "new@example.com",
            "--dry-run",
        ])
        .unwrap();
        match cli.command {
            crate::Commands::Migrate {
                subcommand:
                    MigrateCmd::Run {
                        dry_run,
                        batch_size,
                        ..
                    },
            } => {
                assert!(dry_run);
                assert_eq!(batch_size, migrate::DEFAULT_BATCH_SIZE);
            }
            _ => panic!("expected migrate run"),
        }
    }

    #[test]
    fn migrate_run_parses_batch_size() {
        let cli = crate::Cli::try_parse_from([
            "envelope",
            "migrate",
            "run",
            "--from",
            "old@example.com",
            "--to",
            "new@example.com",
            "--batch-size",
            "10",
        ])
        .unwrap();
        match cli.command {
            crate::Commands::Migrate {
                subcommand: MigrateCmd::Run { batch_size, .. },
            } => assert_eq!(batch_size, 10),
            _ => panic!("expected migrate run"),
        }
    }

    #[test]
    fn migration_run_fails_when_any_append_failed() {
        assert!(fail_if_any_message_failed(0).is_ok());
        let err = fail_if_any_message_failed(2).unwrap_err().to_string();
        assert!(err.contains("2 failed"));
    }

    #[test]
    fn message_failed_event_emits_human_readable_line() {
        // emit() with json_output=false routes MessageFailed to stderr.
        // We don't capture stderr here; this just guards the match arm
        // exists so a future variant rename can't silently elide the case.
        emit(
            false,
            migrate::ProgressEvent::MessageFailed {
                source: "INBOX".to_string(),
                destination: "INBOX".to_string(),
                src_uid: 7,
                error: "boom".to_string(),
            },
        )
        .unwrap();
    }

    #[test]
    fn message_failed_event_serializes_with_event_tag() {
        let event = migrate::ProgressEvent::MessageFailed {
            source: "Junk E-mail".to_string(),
            destination: "Junk E-mail".to_string(),
            src_uid: 99,
            error: "APPEND rejected".to_string(),
        };
        let line = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["event"], "message_failed");
        assert_eq!(parsed["source"], "Junk E-mail");
        assert_eq!(parsed["src_uid"], 99);
        assert_eq!(parsed["error"], "APPEND rejected");
    }

    #[test]
    fn run_dry_run_done_event_emits_via_emit_helper() {
        // Guards the human-readable arm against silent removal during refactors.
        emit(
            false,
            migrate::ProgressEvent::RunDryRunDone {
                folders: 3,
                already_migrated: 5,
                already_in_destination: 2,
                would_copy: 18,
            },
        )
        .unwrap();
    }

    #[test]
    fn batch_size_above_max_is_rejected_at_validation() {
        // Defense-in-depth: even if clap parsing accepts a u32, validation
        // refuses operator-unsafe sizes before any IMAP work begins.
        let result = migrate::validate_batch_size(migrate::MAX_BATCH_SIZE + 1);
        assert!(result.is_err());
    }

    #[test]
    fn batch_size_zero_is_rejected_at_validation() {
        assert!(migrate::validate_batch_size(0).is_err());
    }
}
