// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use serde::Serialize;

pub const DEFAULT_BATCH_SIZE: u32 = 25;

#[derive(Debug, Clone, Serialize)]
pub struct FolderPlan {
    pub source: String,
    pub destination: String,
    pub messages: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ProgressEvent {
    MigrationStatePreflightFailed {
        error: String,
    },
    FolderStart {
        source: String,
        destination: String,
        messages: u32,
    },
    MessageCopied {
        source: String,
        destination: String,
        src_uid: u32,
        message_id: Option<String>,
        bytes: u32,
    },
    MessageSkipped {
        source: String,
        src_uid: u32,
        reason: String,
    },
    MessageFailed {
        source: String,
        destination: String,
        src_uid: u32,
        error: String,
    },
    MessageStateRecordFailed {
        source: String,
        destination: String,
        src_uid: u32,
        error: String,
    },
    FolderDryRun {
        source: String,
        destination: String,
        messages: u32,
        destination_exists: bool,
        already_migrated: u32,
        already_in_destination: u32,
        would_copy: u32,
    },
    FolderDone {
        source: String,
        destination: String,
        copied: u32,
        skipped: u32,
        failed: u32,
    },
    RunDone {
        folders: u32,
        copied: u32,
        skipped: u32,
        failed: u32,
    },
    /// Aggregate summary at the end of a dry-run. `RunDone` is emitted only
    /// when bodies were actually copied; for dry-run, this gives operators a
    /// single line to grep for total `would_copy` without reducing per-folder
    /// `FolderDryRun` events themselves.
    RunDryRunDone {
        folders: u32,
        already_migrated: u32,
        already_in_destination: u32,
        would_copy: u32,
    },
}

pub fn wildcard_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let pattern = pattern.to_lowercase();
    let value = value.to_lowercase();
    let mut parts = pattern.split('*').peekable();
    let mut pos = 0usize;
    let anchored_start = !pattern.starts_with('*');
    let anchored_end = !pattern.ends_with('*');

    if let Some(first) = parts.next() {
        if anchored_start {
            if !value.starts_with(first) {
                return false;
            }
            pos = first.len();
        } else if !first.is_empty() {
            match value.find(first) {
                Some(i) => pos = i + first.len(),
                None => return false,
            }
        }
    }

    let mut last = "";
    for part in parts {
        last = part;
        if part.is_empty() {
            continue;
        }
        match value[pos..].find(part) {
            Some(i) => pos += i + part.len(),
            None => return false,
        }
    }

    !anchored_end || last.is_empty() || value.ends_with(last)
}

pub fn folder_selected(folder: &str, includes: &[String], excludes: &[String]) -> bool {
    if excludes.iter().any(|p| wildcard_match(p, folder)) {
        return false;
    }
    if includes.is_empty() {
        return true;
    }
    includes.iter().any(|p| wildcard_match(p, folder))
}

pub fn validate_distinct_accounts(
    src_account_id: &str,
    dst_account_id: &str,
) -> Result<(), String> {
    if src_account_id == dst_account_id {
        return Err("source and destination accounts resolve to the same account".to_string());
    }
    Ok(())
}

/// Resolve an IMAP endpoint to a stable comparison tuple. Hostnames are
/// case-insensitive per RFC 3501; usernames are server-defined but in practice
/// most providers (Gmail, Workmail, Migadu, Fastmail) treat them
/// case-insensitively. We compare lowercased to avoid a class of operator
/// foot-guns where casing-only differences mask the same physical mailbox.
fn imap_endpoint_key(host: &str, port: u16, username: &str) -> (String, u16, String) {
    (
        host.trim().to_ascii_lowercase(),
        port,
        username.trim().to_ascii_lowercase(),
    )
}

/// Reject migrations whose source and destination resolve to the same physical
/// IMAP mailbox even when the configured account IDs differ. Defense in depth
/// against an operator who registers the same mailbox under two account IDs.
pub fn validate_distinct_imap_endpoints(
    src_host: &str,
    src_port: u16,
    src_username: &str,
    dst_host: &str,
    dst_port: u16,
    dst_username: &str,
) -> Result<(), String> {
    if imap_endpoint_key(src_host, src_port, src_username)
        == imap_endpoint_key(dst_host, dst_port, dst_username)
    {
        return Err(format!(
            "source and destination resolve to the same IMAP mailbox \
             ({}@{}:{}) — refusing to migrate a mailbox onto itself",
            src_username.trim(),
            src_host.trim(),
            src_port,
        ));
    }
    Ok(())
}

/// Conservative upper bound on `--batch-size`. Each batched UID can carry a
/// raw RFC822 message body up to the IMAP server's per-message limit
/// (commonly 25–50 MB). At 500 messages per batch, worst-case in-memory
/// buffering is hundreds of MB before any record is committed — already at
/// the edge of operator-safe. Larger batches do not buy meaningful throughput.
pub const MAX_BATCH_SIZE: u32 = 500;

pub fn validate_batch_size(batch_size: u32) -> Result<u32, String> {
    if batch_size == 0 {
        return Err("--batch-size must be greater than zero".to_string());
    }
    if batch_size > MAX_BATCH_SIZE {
        return Err(format!(
            "--batch-size {batch_size} exceeds the safe upper bound of {MAX_BATCH_SIZE}; \
             larger batches risk holding hundreds of MB of raw message bodies in memory \
             before checkpointing"
        ));
    }
    Ok(batch_size)
}

pub fn uid_range_batches(first_uid: u32, last_uid: u32, batch_size: u32) -> Vec<String> {
    if batch_size == 0 || first_uid == 0 || first_uid > last_uid {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut start = first_uid;
    while start <= last_uid {
        let end = start.saturating_add(batch_size - 1).min(last_uid);
        out.push(format!("{start}:{end}"));
        if end == u32::MAX {
            break;
        }
        start = end + 1;
    }
    out
}

pub fn uid_sequence_set_batches(uids: &[u32], batch_size: u32) -> Vec<String> {
    if batch_size == 0 {
        return Vec::new();
    }

    uids.chunks(batch_size as usize)
        .map(compact_uid_sequence_set)
        .filter(|set| !set.is_empty())
        .collect()
}

fn compact_uid_sequence_set(uids: &[u32]) -> String {
    let mut sorted: Vec<u32> = uids.iter().copied().filter(|uid| *uid > 0).collect();
    sorted.sort_unstable();
    sorted.dedup();

    let mut parts = Vec::new();
    let mut iter = sorted.into_iter().peekable();
    while let Some(start) = iter.next() {
        let mut end = start;
        while let Some(next) = iter.peek().copied() {
            if next == end.saturating_add(1) {
                end = next;
                iter.next();
            } else {
                break;
            }
        }
        if start == end {
            parts.push(start.to_string());
        } else {
            parts.push(format!("{start}:{end}"));
        }
    }
    parts.join(",")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DryRunCounts {
    pub already_migrated: u32,
    pub already_in_destination: u32,
    pub would_copy: u32,
}

pub fn dry_run_counts(
    total_messages: u32,
    already_migrated: u32,
    already_in_destination: u32,
) -> DryRunCounts {
    let skipped = already_migrated.saturating_add(already_in_destination);
    DryRunCounts {
        already_migrated,
        already_in_destination,
        would_copy: total_messages.saturating_sub(skipped),
    }
}

pub fn append_flags(source_flags: &[String]) -> String {
    let mut out = Vec::new();
    for flag in source_flags {
        let normalized = flag.trim_matches('"');
        if normalized.eq_ignore_ascii_case("\\Recent")
            || normalized.eq_ignore_ascii_case("\\Deleted")
        {
            continue;
        }
        if matches!(
            normalized,
            "\\Seen" | "\\Answered" | "\\Flagged" | "\\Draft"
        ) {
            out.push(normalized.to_string());
        }
    }
    if out.is_empty() {
        "()".to_string()
    } else {
        format!("({})", out.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_match_supports_simple_globs() {
        assert!(wildcard_match("*", "INBOX"));
        assert!(wildcard_match("Junk*", "Junk E-mail"));
        assert!(wildcard_match("*Items", "Sent Items"));
        assert!(!wildcard_match("Drafts", "Sent Items"));
    }

    #[test]
    fn excludes_win_over_includes() {
        let includes = vec!["*".to_string()];
        let excludes = vec!["Junk*".to_string()];
        assert!(!folder_selected("Junk E-mail", &includes, &excludes));
        assert!(folder_selected("INBOX", &includes, &excludes));
    }

    #[test]
    fn same_account_migration_is_rejected() {
        assert!(validate_distinct_accounts("a", "b").is_ok());
        assert!(validate_distinct_accounts("a", "a").is_err());
    }

    #[test]
    fn batch_size_must_be_nonzero() {
        assert_eq!(validate_batch_size(25).unwrap(), 25);
        assert!(validate_batch_size(0).is_err());
    }

    #[test]
    fn batch_size_accepts_max_and_rejects_above() {
        assert_eq!(validate_batch_size(MAX_BATCH_SIZE).unwrap(), MAX_BATCH_SIZE);
        let err = validate_batch_size(MAX_BATCH_SIZE + 1)
            .unwrap_err()
            .to_string();
        assert!(err.contains("safe upper bound"), "{err}");
        assert!(err.contains(&MAX_BATCH_SIZE.to_string()), "{err}");
    }

    #[test]
    fn batch_size_rejects_pathological_values() {
        // u32::MAX would produce a chunks() iter the OS can't honor.
        assert!(validate_batch_size(u32::MAX).is_err());
    }

    #[test]
    fn endpoint_guard_rejects_same_host_port_username() {
        assert!(
            validate_distinct_imap_endpoints(
                "imap.example.com",
                993,
                "user@example.com",
                "imap.example.com",
                993,
                "user@example.com",
            )
            .is_err()
        );
    }

    #[test]
    fn endpoint_guard_is_case_insensitive_on_host_and_username() {
        let err = validate_distinct_imap_endpoints(
            "IMAP.Example.COM",
            993,
            "User@Example.com",
            "imap.example.com",
            993,
            "user@example.com",
        )
        .unwrap_err();
        assert!(
            err.contains("same IMAP mailbox"),
            "expected same-mailbox error: {err}"
        );
    }

    #[test]
    fn endpoint_guard_trims_whitespace_around_host_and_username() {
        assert!(
            validate_distinct_imap_endpoints(
                "  imap.example.com  ",
                993,
                "  user@example.com  ",
                "imap.example.com",
                993,
                "user@example.com",
            )
            .is_err()
        );
    }

    #[test]
    fn endpoint_guard_allows_distinct_hosts() {
        assert!(
            validate_distinct_imap_endpoints(
                "imap.old.example",
                993,
                "user@example.com",
                "imap.new.example",
                993,
                "user@example.com",
            )
            .is_ok()
        );
    }

    #[test]
    fn endpoint_guard_allows_distinct_usernames_on_same_host() {
        assert!(
            validate_distinct_imap_endpoints(
                "imap.example.com",
                993,
                "old@example.com",
                "imap.example.com",
                993,
                "new@example.com",
            )
            .is_ok()
        );
    }

    #[test]
    fn endpoint_guard_treats_distinct_ports_as_distinct() {
        assert!(
            validate_distinct_imap_endpoints(
                "imap.example.com",
                143,
                "user@example.com",
                "imap.example.com",
                993,
                "user@example.com",
            )
            .is_ok()
        );
    }

    #[test]
    fn uid_range_batches_split_inclusive_windows() {
        assert_eq!(
            uid_range_batches(1, 250, 100),
            vec!["1:100", "101:200", "201:250"]
        );
        assert!(uid_range_batches(10, 9, 100).is_empty());
        assert!(uid_range_batches(1, 10, 0).is_empty());
    }

    #[test]
    fn uid_sequence_set_batches_compact_sparse_uids() {
        assert_eq!(
            uid_sequence_set_batches(&[3, 1, 2, 9, 10, 15], 4),
            vec!["1:3,9", "10,15"]
        );
    }

    #[test]
    fn dry_run_counts_subtracts_map_and_destination_duplicates() {
        assert_eq!(
            dry_run_counts(10, 3, 2),
            DryRunCounts {
                already_migrated: 3,
                already_in_destination: 2,
                would_copy: 5,
            }
        );
        assert_eq!(dry_run_counts(2, 10, 10).would_copy, 0);
    }

    #[test]
    fn append_flags_strip_unsettable_and_destructive_flags() {
        let flags = vec![
            "\\Seen".to_string(),
            "\\Recent".to_string(),
            "\\Deleted".to_string(),
            "\\Flagged".to_string(),
        ];
        assert_eq!(append_flags(&flags), "(\\Seen \\Flagged)");
    }

    #[test]
    fn message_failed_event_serializes_with_event_tag_and_error_payload() {
        let event = ProgressEvent::MessageFailed {
            source: "INBOX".to_string(),
            destination: "INBOX".to_string(),
            src_uid: 42,
            error: "APPEND failed: server said no".to_string(),
        };
        let json: serde_json::Value = serde_json::from_str(&serde_json::to_string(&event).unwrap())
            .expect("MessageFailed must serialize to JSON");
        assert_eq!(json["event"], "message_failed");
        assert_eq!(json["source"], "INBOX");
        assert_eq!(json["destination"], "INBOX");
        assert_eq!(json["src_uid"], 42);
        assert_eq!(json["error"], "APPEND failed: server said no");
    }

    #[test]
    fn migration_state_preflight_failed_event_serializes_with_event_tag() {
        let event = ProgressEvent::MigrationStatePreflightFailed {
            error: "database error: attempt to write a readonly database".to_string(),
        };
        let json: serde_json::Value = serde_json::from_str(&serde_json::to_string(&event).unwrap())
            .expect("MigrationStatePreflightFailed must serialize to JSON");
        assert_eq!(json["event"], "migration_state_preflight_failed");
        assert_eq!(
            json["error"],
            "database error: attempt to write a readonly database"
        );
    }

    #[test]
    fn message_state_record_failed_event_serializes_with_event_tag() {
        let event = ProgressEvent::MessageStateRecordFailed {
            source: "INBOX".to_string(),
            destination: "INBOX".to_string(),
            src_uid: 42,
            error: "database error: disk I/O error".to_string(),
        };
        let json: serde_json::Value = serde_json::from_str(&serde_json::to_string(&event).unwrap())
            .expect("MessageStateRecordFailed must serialize to JSON");
        assert_eq!(json["event"], "message_state_record_failed");
        assert_eq!(json["source"], "INBOX");
        assert_eq!(json["destination"], "INBOX");
        assert_eq!(json["src_uid"], 42);
        assert_eq!(json["error"], "database error: disk I/O error");
    }

    /// Lock the public ProgressEvent JSON taxonomy. If a tag name or required
    /// field changes, downstream automation breaks silently — make any rename
    /// a deliberate, breaking change by failing this test first.
    #[test]
    fn progress_event_tags_lock_public_taxonomy() {
        fn tag_of(event: &ProgressEvent) -> String {
            let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(event).unwrap())
                .expect("ProgressEvent must serialize");
            v["event"].as_str().unwrap().to_string()
        }

        assert_eq!(
            tag_of(&ProgressEvent::MigrationStatePreflightFailed {
                error: "boom".into(),
            }),
            "migration_state_preflight_failed"
        );
        assert_eq!(
            tag_of(&ProgressEvent::FolderStart {
                source: "INBOX".into(),
                destination: "INBOX".into(),
                messages: 10,
            }),
            "folder_start"
        );
        assert_eq!(
            tag_of(&ProgressEvent::MessageCopied {
                source: "INBOX".into(),
                destination: "INBOX".into(),
                src_uid: 1,
                message_id: Some("<m@x>".into()),
                bytes: 100,
            }),
            "message_copied"
        );
        assert_eq!(
            tag_of(&ProgressEvent::MessageSkipped {
                source: "INBOX".into(),
                src_uid: 1,
                reason: "migration_map".into(),
            }),
            "message_skipped"
        );
        assert_eq!(
            tag_of(&ProgressEvent::MessageFailed {
                source: "INBOX".into(),
                destination: "INBOX".into(),
                src_uid: 1,
                error: "boom".into(),
            }),
            "message_failed"
        );
        assert_eq!(
            tag_of(&ProgressEvent::MessageStateRecordFailed {
                source: "INBOX".into(),
                destination: "INBOX".into(),
                src_uid: 1,
                error: "boom".into(),
            }),
            "message_state_record_failed"
        );
        assert_eq!(
            tag_of(&ProgressEvent::FolderDryRun {
                source: "INBOX".into(),
                destination: "INBOX".into(),
                messages: 5,
                destination_exists: true,
                already_migrated: 1,
                already_in_destination: 1,
                would_copy: 3,
            }),
            "folder_dry_run"
        );
        assert_eq!(
            tag_of(&ProgressEvent::FolderDone {
                source: "INBOX".into(),
                destination: "INBOX".into(),
                copied: 3,
                skipped: 1,
                failed: 0,
            }),
            "folder_done"
        );
        assert_eq!(
            tag_of(&ProgressEvent::RunDone {
                folders: 2,
                copied: 5,
                skipped: 1,
                failed: 0,
            }),
            "run_done"
        );
        assert_eq!(
            tag_of(&ProgressEvent::RunDryRunDone {
                folders: 2,
                already_migrated: 1,
                already_in_destination: 1,
                would_copy: 3,
            }),
            "run_dry_run_done"
        );
    }

    #[test]
    fn run_dry_run_done_event_carries_aggregate_counts() {
        let event = ProgressEvent::RunDryRunDone {
            folders: 4,
            already_migrated: 10,
            already_in_destination: 7,
            would_copy: 83,
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        assert_eq!(parsed["event"], "run_dry_run_done");
        assert_eq!(parsed["folders"], 4);
        assert_eq!(parsed["already_migrated"], 10);
        assert_eq!(parsed["already_in_destination"], 7);
        assert_eq!(parsed["would_copy"], 83);
    }

    #[test]
    fn folder_selected_handles_inbox_nested_sent_junk_and_spaced_names() {
        let none: Vec<String> = Vec::new();
        // No filters — every common folder layout is selected.
        for folder in [
            "INBOX",
            "INBOX/Archive",
            "INBOX/Archive/2024",
            "Sent",
            "Sent Items",
            "Junk",
            "Junk E-mail",
            "Folder With   Multiple Spaces",
            "[Gmail]/All Mail",
        ] {
            assert!(
                folder_selected(folder, &none, &none),
                "expected default policy to select {folder:?}"
            );
        }

        // Nested-folder include with star.
        let includes = vec!["INBOX/*".to_string()];
        assert!(folder_selected("INBOX/Archive", &includes, &none));
        assert!(folder_selected("INBOX/Archive/2024", &includes, &none));
        assert!(!folder_selected("Drafts", &includes, &none));

        // Wildcard exclude on a name with a hyphen and space (Workmail style).
        let excludes = vec!["Junk*".to_string()];
        assert!(!folder_selected("Junk", &none, &excludes));
        assert!(!folder_selected("Junk E-mail", &none, &excludes));
        assert!(folder_selected("INBOX", &none, &excludes));

        // Spaced-name include matches both 'Sent' and 'Sent Items'.
        let includes = vec!["Sent*".to_string()];
        assert!(folder_selected("Sent", &includes, &none));
        assert!(folder_selected("Sent Items", &includes, &none));
        assert!(!folder_selected("INBOX", &includes, &none));

        // Bracketed Gmail special-use folder via wildcard.
        let includes = vec!["[Gmail]/*".to_string()];
        assert!(folder_selected("[Gmail]/All Mail", &includes, &none));
        assert!(folder_selected("[Gmail]/Sent Mail", &includes, &none));
        assert!(!folder_selected("INBOX", &includes, &none));
    }
}
