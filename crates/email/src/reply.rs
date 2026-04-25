// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Reply header construction helpers.
//!
//! Given a parent message (as returned by [`crate::imap::fetch_message`]),
//! produce the headers a reply must carry to remain properly threaded in
//! every major mail client: `In-Reply-To`, `References`, `Subject` (with
//! idempotent `Re:` prefix), `To`, and `Cc`.
//!
//! Reply-all excludes the account's own address from the Cc list so the
//! sender doesn't email themselves.
//!
//! ```text
//! let parent: Message = fetch_message(&mut client, "INBOX", 42).await?.unwrap();
//! let headers = reply::build_reply_headers(&parent);
//! smtp.send(
//!     account,
//!     &headers.to,
//!     &headers.subject,
//!     Some(&body_text),
//!     None,        // html
//!     None,        // cc
//!     None,        // bcc
//!     None,        // reply_to
//!     headers.in_reply_to.as_deref(),
//!     Some(&headers.references),
//!     &[],         // attachments
//! ).await?;
//! ```

use envelope_email_store::models::Message;

use crate::threading::{parse_references, strip_reply_prefixes};

/// Headers required to send a reply that threads correctly in mail clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyHeaders {
    /// Value for the `In-Reply-To` header (the parent's Message-ID, if any).
    pub in_reply_to: Option<String>,
    /// Value for the `References` header (ordered list of prior Message-IDs).
    pub references: Vec<String>,
    /// Subject line with idempotent `Re:` prefix.
    pub subject: String,
    /// Primary recipient (the parent's From address).
    pub to: String,
    /// Cc list — empty for a regular reply, populated for reply-all.
    pub cc: Vec<String>,
}

/// Build reply headers for a single-recipient `Reply` action.
///
/// - `In-Reply-To`: parent's Message-ID (trimmed of any angle brackets
///   the caller may have included — lettre adds its own).
/// - `References`: parent's existing `References` header + parent's
///   Message-ID appended. If the parent had no `References`, this is
///   just `[parent_message_id]`.
/// - `Subject`: `Re: ` prefix added if not already present. Idempotent
///   (`Re: Re: Re: foo` stays as `Re: foo`).
/// - `To`: parent's `from_addr`.
/// - `Cc`: empty.
pub fn build_reply_headers(parent: &Message) -> ReplyHeaders {
    let in_reply_to = parent.message_id.as_ref().map(|m| strip_brackets(m));

    let mut references = match parent.references.as_deref() {
        Some(refs) if !refs.is_empty() => parse_references(refs)
            .into_iter()
            .map(|r| strip_brackets(&r))
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };

    // Append parent's Message-ID to References (forming the new chain).
    if let Some(ref mid) = in_reply_to
        && !references.iter().any(|r| r == mid)
    {
        references.push(mid.clone());
    }

    let subject = prefix_subject(&parent.subject);

    ReplyHeaders {
        in_reply_to,
        references,
        subject,
        to: parent.from_addr.clone(),
        cc: Vec::new(),
    }
}

/// Build reply headers for a `Reply All` action.
///
/// Same as [`build_reply_headers`] plus:
/// - `Cc`: parent's `to_addr` + `cc_addr`, minus any entry whose local
///   address matches `self_addr` (to avoid replying to yourself).
///
/// The address match is case-insensitive and tolerates display-name
/// wrappers like `"Tyler Martin <tyler@example.com>"`.
pub fn build_reply_all_headers(parent: &Message, self_addr: &str) -> ReplyHeaders {
    let mut headers = build_reply_headers(parent);
    let self_lower = self_addr.to_lowercase();

    // Helper to split a comma-separated "Name <addr>, addr2" list into
    // individual trimmed entries.
    let split = |s: &str| -> Vec<String> {
        s.split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect()
    };

    let mut cc_candidates: Vec<String> = Vec::new();
    cc_candidates.extend(split(&parent.to_addr));
    if let Some(ref cc_field) = parent.cc_addr {
        cc_candidates.extend(split(cc_field));
    }

    // Dedupe + exclude self
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut cc = Vec::new();
    for entry in cc_candidates {
        let bare = extract_bare_address(&entry).to_lowercase();
        if bare.is_empty() || bare == self_lower {
            continue;
        }
        if seen.insert(bare) {
            cc.push(entry);
        }
    }

    headers.cc = cc;
    headers
}

/// Add `Re: ` prefix to a subject if not already present (idempotent).
/// Preserves original case — uses `strip_reply_prefixes`, not
/// `normalize_subject` (which lowercases for thread grouping).
fn prefix_subject(subject: &str) -> String {
    let stripped = strip_reply_prefixes(subject);
    if stripped.is_empty() {
        "Re:".to_string()
    } else {
        format!("Re: {stripped}")
    }
}

/// Strip surrounding angle brackets from a Message-ID if present.
///
/// `<abc@host>` → `abc@host`.
/// Idempotent: passing an already-stripped ID returns it unchanged.
fn strip_brackets(s: &str) -> String {
    s.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_string()
}

/// Extract the bare address from a `Name <addr@host>` string.
///
/// If the input has no angle brackets, returns it trimmed unchanged.
/// Used for self-address comparison in reply-all.
fn extract_bare_address(s: &str) -> String {
    let trimmed = s.trim();
    if let (Some(start), Some(end)) = (trimmed.find('<'), trimmed.rfind('>'))
        && start < end
    {
        return trimmed[start + 1..end].to_string();
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use envelope_email_store::models::{AttachmentMeta, Message};

    fn make_parent(
        subject: &str,
        message_id: Option<&str>,
        from: &str,
        to: &str,
        cc: Option<&str>,
        references: Option<&str>,
    ) -> Message {
        Message {
            uid: 1,
            message_id: message_id.map(|s| s.to_string()),
            from_addr: from.to_string(),
            to_addr: to.to_string(),
            cc_addr: cc.map(|s| s.to_string()),
            subject: subject.to_string(),
            date: Some("2026-04-09T10:00:00".to_string()),
            text_body: Some("Hello world".to_string()),
            html_body: None,
            in_reply_to: None,
            references: references.map(|s| s.to_string()),
            flags: vec![],
            attachments: Vec::<AttachmentMeta>::new(),
        }
    }

    // ── build_reply_headers ─────────────────────────────────────────

    #[test]
    fn reply_single_parent_no_references() {
        let parent = make_parent(
            "Project update",
            Some("<msg1@example.com>"),
            "alice@example.com",
            "bob@example.com",
            None,
            None,
        );
        let headers = build_reply_headers(&parent);
        assert_eq!(headers.to, "alice@example.com");
        assert_eq!(headers.subject, "Re: Project update");
        assert_eq!(headers.in_reply_to.as_deref(), Some("msg1@example.com"));
        assert_eq!(headers.references, vec!["msg1@example.com".to_string()]);
        assert!(headers.cc.is_empty());
    }

    #[test]
    fn reply_appends_to_existing_references_chain() {
        let parent = make_parent(
            "Re: Project update",
            Some("<msg3@example.com>"),
            "alice@example.com",
            "bob@example.com",
            None,
            Some("<msg1@example.com> <msg2@example.com>"),
        );
        let headers = build_reply_headers(&parent);
        assert_eq!(
            headers.references,
            vec![
                "msg1@example.com".to_string(),
                "msg2@example.com".to_string(),
                "msg3@example.com".to_string(),
            ]
        );
    }

    #[test]
    fn reply_does_not_duplicate_parent_in_references() {
        // Some mail clients include their own Message-ID in References; don't add it twice.
        let parent = make_parent(
            "Re: Project update",
            Some("<msg2@example.com>"),
            "alice@example.com",
            "bob@example.com",
            None,
            Some("<msg1@example.com> <msg2@example.com>"),
        );
        let headers = build_reply_headers(&parent);
        assert_eq!(
            headers.references,
            vec![
                "msg1@example.com".to_string(),
                "msg2@example.com".to_string()
            ],
            "references must not contain msg2 twice"
        );
    }

    #[test]
    fn reply_subject_re_prefix_is_idempotent() {
        let parent1 = make_parent("Project update", Some("<m@x>"), "a@x", "b@x", None, None);
        assert_eq!(build_reply_headers(&parent1).subject, "Re: Project update");

        let parent2 = make_parent(
            "Re: Project update",
            Some("<m@x>"),
            "a@x",
            "b@x",
            None,
            None,
        );
        assert_eq!(build_reply_headers(&parent2).subject, "Re: Project update");

        let parent3 = make_parent(
            "Re: Re: Re: Project update",
            Some("<m@x>"),
            "a@x",
            "b@x",
            None,
            None,
        );
        assert_eq!(build_reply_headers(&parent3).subject, "Re: Project update");
    }

    #[test]
    fn reply_handles_parent_without_message_id() {
        let parent = make_parent("hello", None, "a@x", "b@x", None, None);
        let headers = build_reply_headers(&parent);
        assert_eq!(headers.in_reply_to, None);
        assert!(headers.references.is_empty());
        assert_eq!(headers.subject, "Re: hello");
    }

    #[test]
    fn reply_normalizes_international_subject_prefixes() {
        // AW: is the German reply prefix, handled by threading::normalize_subject
        let parent = make_parent("AW: Besprechung", Some("<m@x>"), "a@x", "b@x", None, None);
        assert_eq!(build_reply_headers(&parent).subject, "Re: Besprechung");
    }

    // ── build_reply_all_headers ─────────────────────────────────────

    #[test]
    fn reply_all_excludes_self_from_cc() {
        let parent = make_parent(
            "Project update",
            Some("<msg1@example.com>"),
            "alice@example.com",
            "bob@example.com, charlie@example.com",
            Some("dave@example.com"),
            None,
        );
        let headers = build_reply_all_headers(&parent, "bob@example.com");
        assert_eq!(headers.to, "alice@example.com");
        assert_eq!(headers.cc.len(), 2);
        assert!(
            headers
                .cc
                .iter()
                .all(|c| !c.to_lowercase().contains("bob@example.com"))
        );
        assert!(headers.cc.iter().any(|c| c.contains("charlie@example.com")));
        assert!(headers.cc.iter().any(|c| c.contains("dave@example.com")));
    }

    #[test]
    fn reply_all_handles_display_names() {
        let parent = make_parent(
            "Project update",
            Some("<msg1@example.com>"),
            "alice@example.com",
            "\"Bob Smith\" <bob@example.com>, Charlie <charlie@example.com>",
            None,
            None,
        );
        let headers = build_reply_all_headers(&parent, "bob@example.com");
        assert_eq!(headers.cc.len(), 1);
        assert!(headers.cc[0].contains("charlie@example.com"));
    }

    #[test]
    fn reply_all_dedupes_cc_entries() {
        let parent = make_parent(
            "x",
            Some("<m@x>"),
            "a@x",
            "bob@x, bob@x, bob@x, charlie@x",
            None,
            None,
        );
        let headers = build_reply_all_headers(&parent, "me@x");
        assert_eq!(headers.cc.len(), 2); // bob + charlie, not three bobs
    }

    #[test]
    fn reply_all_case_insensitive_self_match() {
        let parent = make_parent(
            "x",
            Some("<m@x>"),
            "a@x",
            "Bob@Example.com, charlie@example.com",
            None,
            None,
        );
        let headers = build_reply_all_headers(&parent, "bob@example.com");
        assert_eq!(headers.cc.len(), 1);
        assert!(headers.cc[0].contains("charlie@example.com"));
    }

    // ── helpers ─────────────────────────────────────────────────────

    #[test]
    fn strip_brackets_idempotent() {
        assert_eq!(strip_brackets("<abc@host>"), "abc@host");
        assert_eq!(strip_brackets("abc@host"), "abc@host");
        assert_eq!(strip_brackets("  <abc@host>  "), "abc@host");
    }

    #[test]
    fn extract_bare_address_variants() {
        assert_eq!(extract_bare_address("bob@example.com"), "bob@example.com");
        assert_eq!(
            extract_bare_address("\"Bob Smith\" <bob@example.com>"),
            "bob@example.com"
        );
        assert_eq!(
            extract_bare_address("Bob Smith <bob@example.com>"),
            "bob@example.com"
        );
    }
}
