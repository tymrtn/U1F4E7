// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Email provider detection and canonical folder resolution.
//!
//! Different email providers use different IMAP folder naming conventions:
//! - Gmail: `[Gmail]/Drafts`, `[Gmail]/Sent Mail`, `[Gmail]/Trash`, etc.
//! - Standard (Migadu, Fastmail, etc.): `Drafts`, `Sent`, `Trash`, etc.
//! - Dovecot (dot-separated): `INBOX.Drafts`, `INBOX.Sent`, etc.
//!
//! This module detects the provider type from the IMAP folder list and provides
//! a canonical folder resolver so the rest of the codebase never has to
//! hard-code provider-specific folder names.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Known email provider types that affect IMAP folder naming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderType {
    /// Gmail — uses `[Gmail]/` prefix for system folders.
    Gmail,
    /// Standard IMAP (Migadu, Fastmail, etc.) — flat folder names.
    Standard,
    /// Dovecot with dot-separated hierarchy (e.g., `INBOX.Drafts`).
    Dovecot,
    /// Microsoft Exchange / Outlook.com — similar to Standard but with
    /// variations like "Deleted Items", "Junk E-mail".
    Exchange,
    /// Unknown provider — fall back to try-both detection.
    Unknown,
}

impl ProviderType {
    /// Parse from a stored string value.
    pub fn from_str_value(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "gmail" => Self::Gmail,
            "standard" => Self::Standard,
            "dovecot" => Self::Dovecot,
            "exchange" => Self::Exchange,
            _ => Self::Unknown,
        }
    }

    /// Convert to a storable string value.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Gmail => "gmail",
            Self::Standard => "standard",
            Self::Dovecot => "dovecot",
            Self::Exchange => "exchange",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for ProviderType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Canonical (logical) folder names used throughout the codebase.
/// Code refers to these logical names; the resolver maps them to actual IMAP names.
pub mod canonical {
    pub const INBOX: &str = "inbox";
    pub const DRAFTS: &str = "drafts";
    pub const SENT: &str = "sent";
    pub const TRASH: &str = "trash";
    pub const SPAM: &str = "spam";
    pub const ARCHIVE: &str = "archive";
    pub const STARRED: &str = "starred";
}

/// Detect the provider type from a list of IMAP folder names.
///
/// Detection priority:
/// 1. Any folder starting with `[Gmail]` → Gmail
/// 2. Any folder starting with `INBOX.` (Dovecot dot-separator) → Dovecot
/// 3. Presence of "Deleted Items" or "Junk E-mail" → Exchange
/// 4. Otherwise → Standard
pub fn detect_provider(folders: &[String]) -> ProviderType {
    let has_gmail_prefix = folders.iter().any(|f| f.starts_with("[Gmail]"));
    if has_gmail_prefix {
        return ProviderType::Gmail;
    }

    let has_dovecot_prefix = folders
        .iter()
        .any(|f| f.starts_with("INBOX.") && f != "INBOX");
    if has_dovecot_prefix {
        return ProviderType::Dovecot;
    }

    let has_exchange_markers = folders
        .iter()
        .any(|f| f == "Deleted Items" || f == "Junk E-mail");
    if has_exchange_markers {
        return ProviderType::Exchange;
    }

    ProviderType::Standard
}

/// Resolve a canonical (logical) folder name to the actual IMAP folder name
/// for the given provider type.
///
/// If the logical name is not a known canonical folder, it is returned as-is
/// (passthrough for custom/user folders).
///
/// # Examples
/// ```
/// use envelope_email_transport::provider::{ProviderType, resolve_folder};
///
/// assert_eq!(resolve_folder(ProviderType::Gmail, "drafts"), "[Gmail]/Drafts");
/// assert_eq!(resolve_folder(ProviderType::Standard, "drafts"), "Drafts");
/// assert_eq!(resolve_folder(ProviderType::Gmail, "My Custom"), "My Custom");
/// ```
pub fn resolve_folder(provider: ProviderType, logical: &str) -> &str {
    match (provider, logical.to_lowercase().as_str()) {
        // ── Gmail ────────────────────────────────────
        (ProviderType::Gmail, "inbox") => "INBOX",
        (ProviderType::Gmail, "drafts") => "[Gmail]/Drafts",
        (ProviderType::Gmail, "sent") => "[Gmail]/Sent Mail",
        (ProviderType::Gmail, "trash") => "[Gmail]/Trash",
        (ProviderType::Gmail, "spam") => "[Gmail]/Spam",
        (ProviderType::Gmail, "archive") => "[Gmail]/All Mail",
        (ProviderType::Gmail, "starred") => "[Gmail]/Starred",

        // ── Standard (Migadu, Fastmail, etc.) ────────
        (ProviderType::Standard, "inbox") => "INBOX",
        (ProviderType::Standard, "drafts") => "Drafts",
        (ProviderType::Standard, "sent") => "Sent",
        (ProviderType::Standard, "trash") => "Trash",
        (ProviderType::Standard, "spam") => "Junk",
        (ProviderType::Standard, "archive") => "Archive",

        // ── Dovecot (dot-separated hierarchy) ────────
        (ProviderType::Dovecot, "inbox") => "INBOX",
        (ProviderType::Dovecot, "drafts") => "INBOX.Drafts",
        (ProviderType::Dovecot, "sent") => "INBOX.Sent",
        (ProviderType::Dovecot, "trash") => "INBOX.Trash",
        (ProviderType::Dovecot, "spam") => "INBOX.Junk",
        (ProviderType::Dovecot, "archive") => "INBOX.Archive",

        // ── Exchange / Outlook ───────────────────────
        (ProviderType::Exchange, "inbox") => "INBOX",
        (ProviderType::Exchange, "drafts") => "Drafts",
        (ProviderType::Exchange, "sent") => "Sent Items",
        (ProviderType::Exchange, "trash") => "Deleted Items",
        (ProviderType::Exchange, "spam") => "Junk E-mail",
        (ProviderType::Exchange, "archive") => "Archive",

        // ── Unknown — return reasonable defaults ─────
        // (These match Standard; callers should try-both if needed)
        (ProviderType::Unknown, "inbox") => "INBOX",
        (ProviderType::Unknown, "drafts") => "Drafts",
        (ProviderType::Unknown, "sent") => "Sent",
        (ProviderType::Unknown, "trash") => "Trash",
        (ProviderType::Unknown, "spam") => "Junk",
        (ProviderType::Unknown, "archive") => "Archive",

        // ── Passthrough for non-canonical folder names ──
        _ => logical,
    }
}

/// Resolve a canonical folder name, returning an owned String.
///
/// Convenience wrapper around [`resolve_folder`] for contexts that need ownership.
pub fn resolve_folder_owned(provider: ProviderType, logical: &str) -> String {
    resolve_folder(provider, logical).to_string()
}

/// Classify an actual IMAP folder name into its canonical type.
///
/// Returns `None` for unrecognized custom folders.
pub fn classify_folder(name: &str) -> Option<&'static str> {
    let lower = name.to_lowercase();
    match lower.as_str() {
        "inbox" => Some(canonical::INBOX),

        // Drafts
        "drafts" | "[gmail]/drafts" | "draft" | "inbox.drafts" | "inbox/drafts" => {
            Some(canonical::DRAFTS)
        }

        // Sent
        "sent"
        | "sent mail"
        | "sent messages"
        | "sent items"
        | "[gmail]/sent mail"
        | "inbox.sent"
        | "inbox.sent messages" => Some(canonical::SENT),

        // Trash
        "trash" | "[gmail]/trash" | "deleted messages" | "deleted items" | "inbox.trash" => {
            Some(canonical::TRASH)
        }

        // Spam/Junk
        "junk" | "spam" | "[gmail]/spam" | "junk e-mail" | "inbox.junk" | "inbox.spam" => {
            Some(canonical::SPAM)
        }

        // Archive
        "archive" | "all mail" | "[gmail]/all mail" | "inbox.archive" => Some(canonical::ARCHIVE),

        // Starred (Gmail-specific)
        "[gmail]/starred" => Some(canonical::STARRED),

        _ => None,
    }
}

/// Get all candidate IMAP folder names for a canonical type across all providers.
///
/// Used as a fallback when provider type is Unknown — tries all known variants.
pub fn all_candidates_for(canonical_type: &str) -> &'static [&'static str] {
    match canonical_type {
        "drafts" => &[
            "Drafts",
            "[Gmail]/Drafts",
            "Draft",
            "INBOX.Drafts",
            "INBOX/Drafts",
        ],
        "sent" => &[
            "Sent",
            "Sent Mail",
            "Sent Messages",
            "Sent Items",
            "[Gmail]/Sent Mail",
            "INBOX.Sent",
            "INBOX.Sent Messages",
        ],
        "trash" => &[
            "Trash",
            "[Gmail]/Trash",
            "Deleted Messages",
            "Deleted Items",
            "INBOX.Trash",
        ],
        "spam" => &[
            "Junk",
            "Spam",
            "[Gmail]/Spam",
            "Junk E-mail",
            "INBOX.Junk",
            "INBOX.Spam",
        ],
        "archive" => &["Archive", "All Mail", "[Gmail]/All Mail", "INBOX.Archive"],
        "inbox" => &["INBOX"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Provider detection ───────────────────────────────────────

    #[test]
    fn detect_gmail() {
        let folders = vec![
            "INBOX".to_string(),
            "[Gmail]/Drafts".to_string(),
            "[Gmail]/Sent Mail".to_string(),
            "[Gmail]/Trash".to_string(),
            "[Gmail]/Spam".to_string(),
            "[Gmail]/All Mail".to_string(),
            "[Gmail]/Starred".to_string(),
        ];
        assert_eq!(detect_provider(&folders), ProviderType::Gmail);
    }

    #[test]
    fn detect_standard() {
        let folders = vec![
            "INBOX".to_string(),
            "Drafts".to_string(),
            "Sent".to_string(),
            "Trash".to_string(),
            "Junk".to_string(),
            "Archive".to_string(),
        ];
        assert_eq!(detect_provider(&folders), ProviderType::Standard);
    }

    #[test]
    fn detect_dovecot() {
        let folders = vec![
            "INBOX".to_string(),
            "INBOX.Drafts".to_string(),
            "INBOX.Sent".to_string(),
            "INBOX.Trash".to_string(),
            "INBOX.Junk".to_string(),
        ];
        assert_eq!(detect_provider(&folders), ProviderType::Dovecot);
    }

    #[test]
    fn detect_exchange() {
        let folders = vec![
            "INBOX".to_string(),
            "Drafts".to_string(),
            "Sent Items".to_string(),
            "Deleted Items".to_string(),
            "Junk E-mail".to_string(),
        ];
        assert_eq!(detect_provider(&folders), ProviderType::Exchange);
    }

    #[test]
    fn detect_empty_folders_is_standard() {
        assert_eq!(detect_provider(&[]), ProviderType::Standard);
    }

    // ── Folder resolution ────────────────────────────────────────

    #[test]
    fn resolve_gmail_folders() {
        assert_eq!(
            resolve_folder(ProviderType::Gmail, "drafts"),
            "[Gmail]/Drafts"
        );
        assert_eq!(
            resolve_folder(ProviderType::Gmail, "sent"),
            "[Gmail]/Sent Mail"
        );
        assert_eq!(
            resolve_folder(ProviderType::Gmail, "trash"),
            "[Gmail]/Trash"
        );
        assert_eq!(resolve_folder(ProviderType::Gmail, "spam"), "[Gmail]/Spam");
        assert_eq!(
            resolve_folder(ProviderType::Gmail, "archive"),
            "[Gmail]/All Mail"
        );
        assert_eq!(
            resolve_folder(ProviderType::Gmail, "starred"),
            "[Gmail]/Starred"
        );
        assert_eq!(resolve_folder(ProviderType::Gmail, "inbox"), "INBOX");
    }

    #[test]
    fn resolve_standard_folders() {
        assert_eq!(resolve_folder(ProviderType::Standard, "drafts"), "Drafts");
        assert_eq!(resolve_folder(ProviderType::Standard, "sent"), "Sent");
        assert_eq!(resolve_folder(ProviderType::Standard, "trash"), "Trash");
        assert_eq!(resolve_folder(ProviderType::Standard, "spam"), "Junk");
        assert_eq!(resolve_folder(ProviderType::Standard, "archive"), "Archive");
        assert_eq!(resolve_folder(ProviderType::Standard, "inbox"), "INBOX");
    }

    #[test]
    fn resolve_dovecot_folders() {
        assert_eq!(
            resolve_folder(ProviderType::Dovecot, "drafts"),
            "INBOX.Drafts"
        );
        assert_eq!(resolve_folder(ProviderType::Dovecot, "sent"), "INBOX.Sent");
        assert_eq!(
            resolve_folder(ProviderType::Dovecot, "trash"),
            "INBOX.Trash"
        );
        assert_eq!(resolve_folder(ProviderType::Dovecot, "spam"), "INBOX.Junk");
        assert_eq!(
            resolve_folder(ProviderType::Dovecot, "archive"),
            "INBOX.Archive"
        );
    }

    #[test]
    fn resolve_exchange_folders() {
        assert_eq!(resolve_folder(ProviderType::Exchange, "drafts"), "Drafts");
        assert_eq!(resolve_folder(ProviderType::Exchange, "sent"), "Sent Items");
        assert_eq!(
            resolve_folder(ProviderType::Exchange, "trash"),
            "Deleted Items"
        );
        assert_eq!(
            resolve_folder(ProviderType::Exchange, "spam"),
            "Junk E-mail"
        );
    }

    #[test]
    fn resolve_unknown_uses_standard_defaults() {
        assert_eq!(resolve_folder(ProviderType::Unknown, "drafts"), "Drafts");
        assert_eq!(resolve_folder(ProviderType::Unknown, "sent"), "Sent");
        assert_eq!(resolve_folder(ProviderType::Unknown, "trash"), "Trash");
    }

    #[test]
    fn resolve_custom_folder_passthrough() {
        assert_eq!(
            resolve_folder(ProviderType::Gmail, "My Custom Folder"),
            "My Custom Folder"
        );
        assert_eq!(
            resolve_folder(ProviderType::Standard, "Projects/Active"),
            "Projects/Active"
        );
        assert_eq!(
            resolve_folder(ProviderType::Dovecot, "Newsletters"),
            "Newsletters"
        );
    }

    #[test]
    fn resolve_case_insensitive() {
        assert_eq!(
            resolve_folder(ProviderType::Gmail, "DRAFTS"),
            "[Gmail]/Drafts"
        );
        assert_eq!(
            resolve_folder(ProviderType::Gmail, "Drafts"),
            "[Gmail]/Drafts"
        );
        assert_eq!(resolve_folder(ProviderType::Standard, "SENT"), "Sent");
    }

    // ── Folder classification ────────────────────────────────────

    #[test]
    fn classify_common_folders() {
        assert_eq!(classify_folder("INBOX"), Some("inbox"));
        assert_eq!(classify_folder("Drafts"), Some("drafts"));
        assert_eq!(classify_folder("[Gmail]/Drafts"), Some("drafts"));
        assert_eq!(classify_folder("INBOX.Drafts"), Some("drafts"));
        assert_eq!(classify_folder("Sent"), Some("sent"));
        assert_eq!(classify_folder("Sent Mail"), Some("sent"));
        assert_eq!(classify_folder("[Gmail]/Sent Mail"), Some("sent"));
        assert_eq!(classify_folder("Sent Items"), Some("sent"));
        assert_eq!(classify_folder("Trash"), Some("trash"));
        assert_eq!(classify_folder("[Gmail]/Trash"), Some("trash"));
        assert_eq!(classify_folder("Deleted Items"), Some("trash"));
        assert_eq!(classify_folder("Junk"), Some("spam"));
        assert_eq!(classify_folder("Spam"), Some("spam"));
        assert_eq!(classify_folder("[Gmail]/Spam"), Some("spam"));
        assert_eq!(classify_folder("Archive"), Some("archive"));
        assert_eq!(classify_folder("[Gmail]/All Mail"), Some("archive"));
    }

    #[test]
    fn classify_unknown_folders() {
        assert_eq!(classify_folder("My Custom Folder"), None);
        assert_eq!(classify_folder("Projects"), None);
        assert_eq!(classify_folder("Newsletters"), None);
    }

    // ── ProviderType serialization ───────────────────────────────

    #[test]
    fn provider_type_roundtrip() {
        for provider in [
            ProviderType::Gmail,
            ProviderType::Standard,
            ProviderType::Dovecot,
            ProviderType::Exchange,
            ProviderType::Unknown,
        ] {
            assert_eq!(ProviderType::from_str_value(provider.as_str()), provider);
        }
    }

    #[test]
    fn provider_type_display() {
        assert_eq!(format!("{}", ProviderType::Gmail), "gmail");
        assert_eq!(format!("{}", ProviderType::Standard), "standard");
    }

    // ── all_candidates_for ───────────────────────────────────────

    #[test]
    fn candidates_cover_all_providers() {
        let drafts = all_candidates_for("drafts");
        assert!(drafts.contains(&"Drafts"));
        assert!(drafts.contains(&"[Gmail]/Drafts"));
        assert!(drafts.contains(&"INBOX.Drafts"));

        let sent = all_candidates_for("sent");
        assert!(sent.contains(&"Sent"));
        assert!(sent.contains(&"[Gmail]/Sent Mail"));
        assert!(sent.contains(&"Sent Items"));
        assert!(sent.contains(&"INBOX.Sent"));
    }

    #[test]
    fn candidates_unknown_type_returns_empty() {
        assert!(all_candidates_for("nonexistent").is_empty());
    }
}
