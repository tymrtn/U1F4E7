// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Email threading algorithm and IMAP thread builder.
//!
//! Groups messages into conversation threads using RFC 2822 headers
//! (Message-ID, In-Reply-To, References) with a subject-normalization
//! fallback for messages missing threading headers.

use envelope_email_store::Database;
use envelope_email_store::models::AccountWithCredentials;
use tracing::{debug, info, warn};

use crate::errors::ImapError;
use crate::folders;
use crate::imap;
use crate::provider;

/// Normalize an email subject for thread grouping.
///
/// Strips Re:, Fwd:, Fw:, and similar prefixes (case-insensitive, recursive),
/// trims whitespace, and lowercases the result.
/// Static list of reply/forward prefixes to strip, ordered longest-first
/// to avoid partial matches. All matching is case-insensitive.
///
/// Includes international variants from major email clients:
/// - English: Re:, Fwd:, Fw:
/// - German: Aw: (Antwort), Wg: (Weitergeleitet)
/// - Swedish/Norwegian: Sv: (Svar)
/// - French: Réf:, Réf.:
/// - Spanish: RV: (Reenviar)
/// - Dutch: Antw: (Antwoord)
/// - Italian: Rif: (Riferimento)
/// - Portuguese: Enc: (Encaminhar)
const SUBJECT_PREFIXES: &[&str] = &[
    "réf.:", // French (with dot) — must be before "réf:" to match first
    "antw:", // Dutch
    "réf:",  // French
    "fwd:",  // English forward
    "fw:",   // English forward variant
    "re:",   // English reply
    "aw:",   // German reply
    "wg:",   // German forward
    "sv:",   // Swedish/Norwegian reply
    "rv:",   // Spanish forward
    "rif:",  // Italian
    "enc:",  // Portuguese
];

pub fn normalize_subject(subject: &str) -> String {
    let mut s = subject.trim().to_string();
    loop {
        let trimmed = s.trim_start();
        let lower = trimmed.to_lowercase();

        // Check Re[N]: pattern first (e.g., Re[2]:, Re[3]:)
        if lower.starts_with("re[") {
            if let Some(end) = s.find("]:") {
                s = s[end + 2..].to_string();
                s = s.trim_start().to_string();
                continue;
            } else {
                break;
            }
        }

        // Check all known prefixes (case-insensitive)
        let mut matched = false;
        for prefix in SUBJECT_PREFIXES {
            if lower.starts_with(prefix) {
                // Advance past the prefix using its byte length
                // (safe because we check against the lowercased version)
                s = trimmed[prefix.len()..].to_string();
                matched = true;
                break;
            }
        }
        if !matched {
            break;
        }
        s = s.trim_start().to_string();
    }
    s.trim().to_lowercase()
}

/// Parse a References header string into individual Message-IDs.
///
/// References is typically space-separated angle-bracketed IDs:
/// `<id1@host> <id2@host> <id3@host>`
pub fn parse_references(refs: &str) -> Vec<String> {
    refs.split_whitespace()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Extract a snippet (first ~200 chars) from message body text.
pub fn extract_snippet(text: &str, max_len: usize) -> String {
    // Remove quoted lines (starting with >)
    let cleaned: String = text
        .lines()
        .filter(|line| !line.trim_start().starts_with('>'))
        .collect::<Vec<_>>()
        .join(" ");

    // Collapse whitespace
    let collapsed: String = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");

    if collapsed.len() <= max_len {
        collapsed
    } else {
        // Find a valid UTF-8 char boundary at or before max_len to avoid panicking
        // when truncation falls inside a multi-byte character.
        let mut end = max_len;
        while end > 0 && !collapsed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &collapsed[..end])
    }
}

// Re-export folder types from the canonical implementations (no more duplication).
pub use crate::folders::{FolderInfo, classify_folders, detect_drafts_folder, detect_sent_folder};

/// Classify a single folder name into a semantic type.
/// Delegates to the provider-aware classifier in [`crate::provider`].
pub fn classify_folder_type(name: &str) -> Option<String> {
    provider::classify_folder(name).map(|s| s.to_string())
}

/// Build threads for an account by scanning INBOX and Sent folders.
///
/// This is incremental: it tracks the last-synced UID per folder and only
/// fetches new messages since the last sync.
pub async fn build_threads(
    account: &AccountWithCredentials,
    db: &Database,
    max_messages: u32,
) -> Result<ThreadBuildResult, ImapError> {
    let account_id = &account.account.id;
    let account_email = &account.account.username;

    let mut client = imap::connect(account).await?;

    // Detect sent folder
    let sent_folder = detect_sent_folder(&mut client, db, account_id).await?;

    let mut total_indexed = 0u32;
    let mut threads_created = 0u32;
    let mut threads_updated = 0u32;

    // Scan INBOX
    let inbox_indexed = scan_folder_for_threads(
        &mut client,
        db,
        account_id,
        account_email,
        "INBOX",
        max_messages,
    )
    .await?;
    total_indexed += inbox_indexed.messages_indexed;
    threads_created += inbox_indexed.threads_created;
    threads_updated += inbox_indexed.threads_updated;

    // Scan Sent folder
    if let Some(ref sent) = sent_folder {
        let sent_indexed = scan_folder_for_threads(
            &mut client,
            db,
            account_id,
            account_email,
            sent,
            max_messages,
        )
        .await?;
        total_indexed += sent_indexed.messages_indexed;
        threads_created += sent_indexed.threads_created;
        threads_updated += sent_indexed.threads_updated;
    }

    Ok(ThreadBuildResult {
        messages_indexed: total_indexed,
        threads_created,
        threads_updated,
        sent_folder,
    })
}

/// Scan a single folder for threading data.
async fn scan_folder_for_threads(
    client: &mut imap::ImapClient,
    db: &Database,
    account_id: &str,
    account_email: &str,
    folder: &str,
    max_messages: u32,
) -> Result<FolderScanResult, ImapError> {
    info!("scanning folder {folder} for threads (max {max_messages})");

    // Select the folder
    let mailbox = client
        .session_mut()
        .select(folder)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {folder}: {e}")))?;

    let exists = mailbox.exists;
    if exists == 0 {
        debug!("folder {folder} is empty");
        return Ok(FolderScanResult::default());
    }

    // Check UIDVALIDITY — if changed, all stored UIDs are invalid (RFC 3501 §2.3.1.1)
    let current_uidvalidity = mailbox.uid_validity.unwrap_or(0);
    if current_uidvalidity > 0 {
        let stored_uidvalidity = db.get_uidvalidity(account_id, folder).unwrap_or(None);
        match stored_uidvalidity {
            Some(stored) if stored != current_uidvalidity => {
                warn!(
                    "UIDVALIDITY changed for {folder} ({stored} → {current_uidvalidity}), \
                     resetting sync state"
                );
                if let Err(e) = db.reset_folder_sync(account_id, folder, current_uidvalidity) {
                    warn!("failed to reset folder sync for {folder}: {e}");
                }
            }
            None => {
                // First time seeing this folder — store UIDVALIDITY
                if let Err(e) = db.set_uidvalidity(account_id, folder, current_uidvalidity) {
                    warn!("failed to store uidvalidity for {folder}: {e}");
                }
            }
            _ => {} // UIDVALIDITY unchanged, proceed normally
        }
    }

    // Determine range — use incremental sync if available
    let last_uid = db.get_last_synced_uid(account_id, folder).unwrap_or(None);

    let fetch_query = if let Some(last) = last_uid {
        // Fetch UIDs after the last synced one
        let start = last + 1;
        format!("{start}:*")
    } else {
        // First sync: fetch recent N messages by sequence number
        let start = if exists > max_messages {
            exists - max_messages + 1
        } else {
            1
        };
        format!("{start}:{exists}")
    };

    // Fetch envelope + body for threading headers and snippets
    // Use BODY.PEEK[] to get full RFC822 for mail-parser, which gives us
    // Message-ID, In-Reply-To, References, plus body for snippet extraction
    let fetch_items = if last_uid.is_some() {
        // UID-based fetch for incremental
        let messages = client
            .session_mut()
            .uid_fetch(&fetch_query, "(UID FLAGS BODY.PEEK[])")
            .await
            .map_err(|e| ImapError::Protocol(format!("UID FETCH {fetch_query}: {e}")))?;
        process_fetched_messages(messages, db, account_id, account_email, folder).await?
    } else {
        // Sequence-number-based fetch for initial scan
        let messages = client
            .session_mut()
            .fetch(&fetch_query, "(UID FLAGS BODY.PEEK[])")
            .await
            .map_err(|e| ImapError::Protocol(format!("FETCH {fetch_query}: {e}")))?;
        process_fetched_messages(messages, db, account_id, account_email, folder).await?
    };

    // Update the last-synced UID to the highest UID we saw
    if fetch_items.max_uid > 0 {
        if let Err(e) = db.set_last_synced_uid(account_id, folder, fetch_items.max_uid) {
            warn!("failed to update sync state for {folder}: {e}");
        }
        // Also persist UIDVALIDITY alongside the sync state
        if current_uidvalidity > 0
            && let Err(e) = db.set_uidvalidity(account_id, folder, current_uidvalidity)
        {
            warn!("failed to update uidvalidity for {folder}: {e}");
        }
    }

    Ok(fetch_items)
}

/// Process fetched IMAP messages and thread them.
async fn process_fetched_messages<S>(
    messages: S,
    db: &Database,
    account_id: &str,
    account_email: &str,
    folder: &str,
) -> Result<FolderScanResult, ImapError>
where
    S: futures_util::Stream<Item = Result<async_imap::types::Fetch, async_imap::error::Error>>
        + Unpin,
{
    use futures_util::StreamExt;

    let mut result = FolderScanResult::default();
    let mut dirty_threads: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut stream = messages;

    while let Some(item) = stream.next().await {
        match item {
            Ok(fetch) => {
                let uid = match fetch.uid {
                    Some(u) => u,
                    None => continue,
                };

                if uid > result.max_uid {
                    result.max_uid = uid;
                }

                let body: &[u8] = fetch.body().unwrap_or_default();
                let parsed = match mail_parser::MessageParser::default().parse(body) {
                    Some(p) => p,
                    None => {
                        debug!("skipping unparseable message UID {uid} in {folder}");
                        continue;
                    }
                };

                // Extract threading headers
                let message_id = parsed.message_id().map(|s| s.to_string());
                let in_reply_to = parsed.in_reply_to().as_text().map(|s| s.to_string());
                let references_raw = parsed.references().as_text().map(|s| s.to_string());
                let subject = parsed.subject().unwrap_or_default().to_string();
                let date = parsed
                    .date()
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

                // From/To
                let from_addr = mp_first_address(parsed.from());
                let to_addr = mp_all_addresses(parsed.to());

                // Is this outbound? (sent from our account)
                let is_outbound = from_addr.to_lowercase() == account_email.to_lowercase();

                // Snippet from body
                let snippet = parsed.body_text(0).map(|t| extract_snippet(&t, 200));

                // ── Threading algorithm ──────────────────────────────
                // Priority 1: Message-ID / In-Reply-To / References
                // Priority 2: Subject normalization + address overlap

                let thread_id = None
                    // Try In-Reply-To
                    .or_else(|| {
                        let irt = in_reply_to.as_deref()?;
                        let first_irt = irt.split_whitespace().next().unwrap_or(irt);
                        db.find_thread_by_message_id(first_irt, account_id).ok()?
                    })
                    // Try References
                    .or_else(|| {
                        let refs = references_raw.as_deref()?;
                        let ref_ids = parse_references(refs);
                        let ref_strs: Vec<&str> = ref_ids.iter().map(|s| s.as_str()).collect();
                        db.find_thread_by_references(&ref_strs, account_id).ok()?
                    })
                    // Try own Message-ID (maybe we're the original and replies exist)
                    .or_else(|| {
                        let mid = message_id.as_deref()?;
                        db.find_thread_by_message_id(mid, account_id).ok()?
                    })
                    // Fallback: subject normalization + address overlap
                    .or_else(|| {
                        let normalized = normalize_subject(&subject);
                        if normalized.is_empty() {
                            return None;
                        }
                        let thread = db.find_thread_by_subject(&normalized, account_id).ok()??;
                        let thread_msgs = db
                            .get_thread_messages(&thread.thread_id)
                            .unwrap_or_default();
                        let has_overlap = thread_msgs.iter().any(|tm| {
                            addresses_overlap(
                                &from_addr,
                                &to_addr,
                                &tm.from_address,
                                &tm.to_addresses,
                            )
                        });
                        if has_overlap {
                            Some(thread.thread_id)
                        } else {
                            None
                        }
                    });

                // Create a new thread if no match found
                let tid = match thread_id {
                    Some(tid) => {
                        result.threads_updated += 1;
                        tid
                    }
                    None => {
                        let normalized = normalize_subject(&subject);
                        match db.create_thread(&normalized, &date, &date, account_id) {
                            Ok(t) => {
                                result.threads_created += 1;
                                t.thread_id
                            }
                            Err(e) => {
                                warn!("failed to create thread for UID {uid}: {e}");
                                continue;
                            }
                        }
                    }
                };

                // Upsert the thread message
                if let Err(e) = db.upsert_thread_message(
                    &tid,
                    uid,
                    message_id.as_deref(),
                    in_reply_to.as_deref(),
                    references_raw.as_deref(),
                    folder,
                    &from_addr,
                    &to_addr,
                    &date,
                    &subject,
                    is_outbound,
                    snippet.as_deref(),
                ) {
                    warn!("failed to upsert thread message UID {uid}: {e}");
                    continue;
                }

                // Mark thread as dirty — stats will be refreshed in batch after the loop
                dirty_threads.insert(tid);

                result.messages_indexed += 1;
            }
            Err(e) => {
                warn!("fetch error in {folder}: {e}");
                continue;
            }
        }
    }

    // Batch refresh thread stats for all modified threads
    for tid in &dirty_threads {
        if let Err(e) = db.refresh_thread_stats(tid) {
            warn!("failed to refresh thread stats for {tid}: {e}");
        }
    }

    info!(
        "scanned {folder}: {} messages indexed, {} threads created, {} updated, {} unique threads refreshed",
        result.messages_indexed,
        result.threads_created,
        result.threads_updated,
        dirty_threads.len()
    );

    Ok(result)
}

/// Check if two sets of email addresses overlap (any shared address).
fn addresses_overlap(from_a: &str, to_a: &str, from_b: &str, to_b: &str) -> bool {
    let mut set_a: std::collections::HashSet<String> = std::collections::HashSet::new();
    for addr in std::iter::once(from_a).chain(to_a.split(',')) {
        let trimmed = addr.trim().to_lowercase();
        if !trimmed.is_empty() {
            set_a.insert(trimmed);
        }
    }

    for addr in std::iter::once(from_b).chain(to_b.split(',')) {
        let trimmed = addr.trim().to_lowercase();
        if !trimmed.is_empty() && set_a.contains(&trimmed) {
            return true;
        }
    }

    false
}

/// Extract first email address from a mail-parser Address.
fn mp_first_address(header: Option<&mail_parser::Address<'_>>) -> String {
    match header {
        Some(addr) => match addr {
            mail_parser::Address::List(list) => list
                .first()
                .and_then(|a| a.address.as_ref())
                .map(|a| a.to_string())
                .unwrap_or_default(),
            mail_parser::Address::Group(groups) => groups
                .first()
                .and_then(|g| g.addresses.first())
                .and_then(|a| a.address.as_ref())
                .map(|a| a.to_string())
                .unwrap_or_default(),
        },
        None => String::new(),
    }
}

/// Extract all email addresses as comma-separated string.
fn mp_all_addresses(header: Option<&mail_parser::Address<'_>>) -> String {
    match header {
        Some(addr) => match addr {
            mail_parser::Address::List(list) => list
                .iter()
                .filter_map(|a| a.address.as_ref())
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            mail_parser::Address::Group(groups) => groups
                .iter()
                .flat_map(|g| &g.addresses)
                .filter_map(|a| a.address.as_ref())
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        },
        None => String::new(),
    }
}

/// Result of building threads for one folder.
#[derive(Debug, Default)]
struct FolderScanResult {
    messages_indexed: u32,
    threads_created: u32,
    threads_updated: u32,
    max_uid: u32,
}

/// Result of a full thread build operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ThreadBuildResult {
    pub messages_indexed: u32,
    pub threads_created: u32,
    pub threads_updated: u32,
    pub sent_folder: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_subject() {
        assert_eq!(normalize_subject("Re: Hello World"), "hello world");
        assert_eq!(normalize_subject("re: Hello World"), "hello world");
        assert_eq!(normalize_subject("RE: Hello World"), "hello world");
        assert_eq!(normalize_subject("Fwd: Hello World"), "hello world");
        assert_eq!(normalize_subject("FWD: Hello World"), "hello world");
        assert_eq!(normalize_subject("Fw: Hello World"), "hello world");
        assert_eq!(normalize_subject("Re: Re: Fwd: Hello"), "hello");
        assert_eq!(normalize_subject("Re[2]: Hello"), "hello");
        assert_eq!(normalize_subject("  Re:  Spaces  "), "spaces");
        assert_eq!(normalize_subject("Hello World"), "hello world");
        assert_eq!(normalize_subject(""), "");
    }

    #[test]
    fn test_parse_references() {
        let refs = "<id1@host.com> <id2@host.com> <id3@host.com>";
        let parsed = parse_references(refs);
        assert_eq!(
            parsed,
            vec!["<id1@host.com>", "<id2@host.com>", "<id3@host.com>",]
        );

        assert!(parse_references("").is_empty());
        assert_eq!(
            parse_references("<single@host.com>"),
            vec!["<single@host.com>"]
        );
    }

    #[test]
    fn test_extract_snippet() {
        let text =
            "Hello there!\n\nHow are you doing?\n> This is a quoted line\n> Another quote\n\nBye!";
        let snippet = extract_snippet(text, 200);
        assert!(!snippet.contains('>'));
        assert!(snippet.contains("Hello there!"));
        assert!(snippet.contains("Bye!"));
    }

    #[test]
    fn test_extract_snippet_truncation() {
        let text = "A".repeat(300);
        let snippet = extract_snippet(&text, 200);
        assert!(snippet.len() <= 204); // 200 + "..."
        assert!(snippet.ends_with("..."));
    }

    #[test]
    fn test_addresses_overlap() {
        assert!(addresses_overlap(
            "alice@example.com",
            "bob@example.com",
            "bob@example.com",
            "carol@example.com",
        ));
        assert!(addresses_overlap(
            "alice@example.com",
            "bob@example.com, carol@example.com",
            "dave@example.com",
            "alice@example.com",
        ));
        assert!(!addresses_overlap(
            "alice@example.com",
            "bob@example.com",
            "carol@example.com",
            "dave@example.com",
        ));
        // Case-insensitive
        assert!(addresses_overlap(
            "Alice@Example.com",
            "",
            "alice@example.com",
            "",
        ));
    }

    #[test]
    fn test_classify_folder_type() {
        assert_eq!(classify_folder_type("INBOX"), Some("inbox".to_string()));
        assert_eq!(classify_folder_type("Sent"), Some("sent".to_string()));
        assert_eq!(classify_folder_type("Sent Mail"), Some("sent".to_string()));
        assert_eq!(
            classify_folder_type("[Gmail]/Sent Mail"),
            Some("sent".to_string())
        );
        assert_eq!(classify_folder_type("Drafts"), Some("drafts".to_string()));
        assert_eq!(classify_folder_type("Trash"), Some("trash".to_string()));
        assert_eq!(classify_folder_type("Spam"), Some("spam".to_string()));
        assert_eq!(classify_folder_type("Junk"), Some("spam".to_string()));
        assert_eq!(classify_folder_type("My Custom Folder"), None);
    }

    // ── S4: UTF-8 safety tests ──────────────────────────────────

    #[test]
    fn test_extract_snippet_utf8_multibyte() {
        // Spanish ñ (2 bytes), German ü (2 bytes), Russian д (2 bytes), emoji 🎉 (4 bytes)
        // Build a string where multi-byte chars fall near the truncation boundary
        let text = "Señor García está aquí. Grüße aus München. Дорогой друг. 🎉🎊🎈";
        let snippet = extract_snippet(text, 30);
        // Must not panic, must be valid UTF-8, must end with "..."
        assert!(snippet.ends_with("..."));
        // Verify it's valid UTF-8 (String guarantees this, but let's be explicit)
        assert!(std::str::from_utf8(snippet.as_bytes()).is_ok());
    }

    #[test]
    fn test_extract_snippet_truncation_at_emoji_boundary() {
        // Emoji are 4 bytes each — place truncation boundary inside an emoji
        let text = "AAAAAAAAAA🎉BBBBBBBBBB"; // A*10 (10 bytes) + 🎉 (4 bytes) + B*10
        let snippet = extract_snippet(&text, 11); // byte 11 falls inside the emoji
        assert!(snippet.ends_with("..."));
        // Should truncate to "AAAAAAAAAA..." (backs up to byte 10, before the emoji)
        assert_eq!(snippet, "AAAAAAAAAA...");
    }

    #[test]
    fn test_extract_snippet_all_multibyte() {
        // String of only multi-byte chars (Cyrillic, 2 bytes each)
        let text = "ДДДДДДДДДД"; // 10 chars * 2 bytes = 20 bytes
        let snippet = extract_snippet(&text, 5);
        assert!(snippet.ends_with("..."));
        // byte 5 falls inside char 3 (bytes 4-5), should back up to byte 4 (2 full chars)
        assert_eq!(snippet, "ДД...");
    }

    #[test]
    fn test_extract_snippet_exact_boundary() {
        // Truncation exactly at a char boundary should work cleanly
        let text = "ABC"; // 3 bytes
        let snippet = extract_snippet(&text, 3);
        assert_eq!(snippet, "ABC"); // no truncation needed
    }

    #[test]
    fn test_extract_snippet_zero_max_len() {
        let text = "Hello world";
        let snippet = extract_snippet(&text, 0);
        assert_eq!(snippet, "...");
    }

    // ── D2: Localized subject prefix tests ──────────────────────

    #[test]
    fn test_normalize_subject_german() {
        // Aw: (Antwort = Reply), Wg: (Weitergeleitet = Forwarded)
        assert_eq!(normalize_subject("Aw: Hallo Welt"), "hallo welt");
        assert_eq!(normalize_subject("AW: Hallo Welt"), "hallo welt");
        assert_eq!(normalize_subject("Wg: Hallo Welt"), "hallo welt");
        assert_eq!(normalize_subject("WG: Hallo Welt"), "hallo welt");
        // Nested
        assert_eq!(normalize_subject("Aw: Wg: Hallo"), "hallo");
    }

    #[test]
    fn test_normalize_subject_swedish_norwegian() {
        // Sv: (Svar = Reply)
        assert_eq!(normalize_subject("Sv: Hej världen"), "hej världen");
        assert_eq!(normalize_subject("SV: Hej"), "hej");
    }

    #[test]
    fn test_normalize_subject_french() {
        // Réf: and Réf.:
        assert_eq!(normalize_subject("Réf: Bonjour"), "bonjour");
        assert_eq!(normalize_subject("Réf.: Bonjour"), "bonjour");
        assert_eq!(normalize_subject("RÉF.: BONJOUR"), "bonjour");
    }

    #[test]
    fn test_normalize_subject_spanish() {
        // RV: (Reenviar = Forward)
        assert_eq!(normalize_subject("RV: Hola mundo"), "hola mundo");
        assert_eq!(normalize_subject("Rv: Hola"), "hola");
    }

    #[test]
    fn test_normalize_subject_dutch() {
        // Antw: (Antwoord = Reply)
        assert_eq!(normalize_subject("Antw: Hallo wereld"), "hallo wereld");
        assert_eq!(normalize_subject("ANTW: Hallo"), "hallo");
    }

    #[test]
    fn test_normalize_subject_italian() {
        // Rif: (Riferimento)
        assert_eq!(normalize_subject("Rif: Ciao mondo"), "ciao mondo");
    }

    #[test]
    fn test_normalize_subject_portuguese() {
        // Enc: (Encaminhar = Forward)
        assert_eq!(normalize_subject("Enc: Olá mundo"), "olá mundo");
    }

    #[test]
    fn test_normalize_subject_mixed_international() {
        // Chain of different language prefixes
        assert_eq!(normalize_subject("Re: Aw: Sv: Fwd: Hello"), "hello");
        assert_eq!(
            normalize_subject("Wg: Re: Enc: Meeting notes"),
            "meeting notes"
        );
    }

    #[test]
    fn test_normalize_subject_fw_variant() {
        // Fw: is a common variant of Fwd:
        assert_eq!(normalize_subject("Fw: Hello"), "hello");
        assert_eq!(normalize_subject("FW: Hello"), "hello");
    }

    // ── Drafts folder detection tests ───────────────────────────

    #[test]
    fn test_drafts_folder_candidates_coverage() {
        // All drafts candidates should classify as "drafts" type
        for candidate in provider::all_candidates_for("drafts") {
            assert_eq!(
                classify_folder_type(candidate),
                Some("drafts".to_string()),
                "candidate {candidate} should classify as drafts"
            );
        }
    }

    #[test]
    fn test_classify_folder_type_gmail_drafts() {
        assert_eq!(
            classify_folder_type("[Gmail]/Drafts"),
            Some("drafts".to_string())
        );
    }

    #[test]
    fn test_classify_folder_type_inbox_drafts() {
        assert_eq!(
            classify_folder_type("INBOX.Drafts"),
            Some("drafts".to_string())
        );
    }
}
