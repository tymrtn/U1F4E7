// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub username: String,
    pub domain: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_username: Option<String>,
    pub imap_username: Option<String>,
    pub display_name: Option<String>,
    pub signature_text: Option<String>,
    pub signature_html: Option<String>,
    pub created_at: String,
}

/// Account with decrypted credentials — never serialize to JSON output.
pub struct AccountWithCredentials {
    pub account: Account,
    pub password: String,
    pub smtp_password: Option<String>,
    pub imap_password: Option<String>,
}

impl AccountWithCredentials {
    pub fn effective_smtp_username(&self) -> &str {
        self.account
            .smtp_username
            .as_deref()
            .unwrap_or(&self.account.username)
    }

    pub fn effective_smtp_password(&self) -> &str {
        self.smtp_password.as_deref().unwrap_or(&self.password)
    }

    pub fn effective_imap_username(&self) -> &str {
        self.account
            .imap_username
            .as_deref()
            .unwrap_or(&self.account.username)
    }

    pub fn effective_imap_password(&self) -> &str {
        self.imap_password.as_deref().unwrap_or(&self.password)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Draft {
    pub id: String,
    pub account_id: String,
    pub status: DraftStatus,
    pub to_addr: String,
    pub cc_addr: Option<String>,
    pub bcc_addr: Option<String>,
    pub reply_to: Option<String>,
    pub subject: Option<String>,
    pub text_content: Option<String>,
    pub html_content: Option<String>,
    pub in_reply_to: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub attachments: Vec<serde_json::Value>,
    pub message_id: Option<String>,
    pub send_after: Option<String>,
    pub snoozed_until: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub sent_at: Option<String>,
    pub created_by: Option<String>,
    /// IMAP UID of the draft in the server's Drafts folder (if synced).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imap_uid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DraftStatus {
    Draft,
    PendingReview,
    Blocked,
    Sent,
    Discarded,
}

impl DraftStatus {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Draft => "draft",
            Self::PendingReview => "pending_review",
            Self::Blocked => "blocked",
            Self::Sent => "sent",
            Self::Discarded => "discarded",
        }
    }

    pub fn is_editable(&self) -> bool {
        matches!(self, Self::Draft | Self::PendingReview | Self::Blocked)
    }
}

impl std::str::FromStr for DraftStatus {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "draft" => Ok(Self::Draft),
            "pending_review" => Ok(Self::PendingReview),
            "blocked" => Ok(Self::Blocked),
            "sent" => Ok(Self::Sent),
            "discarded" => Ok(Self::Discarded),
            _ => Err(format!("unknown draft status: {s}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionLog {
    pub id: String,
    pub account_id: String,
    pub action_type: String,
    pub confidence: f64,
    pub justification: String,
    pub action_taken: String,
    pub message_id: Option<String>,
    pub draft_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredLicense {
    pub id: String,
    pub token: String,
    pub licensee: String,
    pub expires_at: String,
    pub features: Vec<String>,
    pub activated_at: String,
}

/// Summary of an IMAP message (envelope data, no body).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSummary {
    pub uid: u32,
    pub message_id: Option<String>,
    pub from_addr: String,
    pub to_addr: String,
    pub subject: String,
    pub date: Option<String>,
    pub flags: Vec<String>,
    pub size: u32,
}

/// Full IMAP message with body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub uid: u32,
    pub message_id: Option<String>,
    pub from_addr: String,
    pub to_addr: String,
    pub cc_addr: Option<String>,
    pub subject: String,
    pub date: Option<String>,
    pub text_body: Option<String>,
    pub html_body: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
    pub flags: Vec<String>,
    pub attachments: Vec<AttachmentMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentMeta {
    pub filename: String,
    pub content_type: String,
    pub size: u64,
    pub content_id: Option<String>,
}

/// IMAP folder stats returned by `STATUS (MESSAGES RECENT UNSEEN)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderStats {
    /// Name of the folder these stats apply to.
    pub folder: String,
    /// Total messages in the folder (MESSAGES).
    pub exists: u32,
    /// Messages with \Recent flag.
    pub recent: u32,
    /// Messages without \Seen flag (Option because not all servers return this).
    pub unseen: Option<u32>,
}

/// A conversation thread backing the `threads` SQLite table.
///
/// Threads are grouped from individual messages by normalized subject
/// (via `threading::normalize_subject`) plus RFC 2822 `References` /
/// `In-Reply-To` header chain walking. See
/// `crates/email/src/threading.rs::build_threads`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    /// UUID for this thread.
    pub thread_id: String,
    /// Subject normalized by stripping `Re:`, `Fwd:`, and similar prefixes.
    pub subject_normalized: String,
    /// ISO 8601 datetime of the earliest message in the thread.
    pub first_seen: String,
    /// ISO 8601 datetime of the most recent message in the thread.
    pub last_activity: String,
    /// Total messages in this thread.
    pub message_count: i64,
    /// Owning account identifier.
    pub account_id: String,
}

/// A single message belonging to a [`Thread`], backing the `thread_messages` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadMessage {
    /// Auto-increment primary key (SQLite ROWID).
    pub id: i64,
    /// FK to `threads.thread_id`.
    pub thread_id: String,
    /// IMAP UID of the message in `folder`.
    pub uid: u32,
    /// RFC 2822 `Message-ID` header value (for cross-folder threading).
    pub message_id: Option<String>,
    /// RFC 2822 `In-Reply-To` header value.
    pub in_reply_to: Option<String>,
    /// RFC 2822 `References` header — space-separated list of message-ids.
    pub references: Option<String>,
    /// Folder the message lives in (`INBOX`, `[Gmail]/Sent Mail`, etc.).
    pub folder: String,
    /// Sender address (or `None` if unparseable).
    pub from_address: Option<String>,
    /// Comma-separated list of recipient addresses.
    pub to_addresses: Option<String>,
    /// ISO 8601 datetime of the message's `Date:` header.
    pub date: Option<String>,
    /// Subject as it appeared on this specific message (before normalization).
    pub subject: Option<String>,
    /// True if the message was sent by the account owner (found in Sent folder).
    pub is_outbound: bool,
    /// Short plain-text preview of the body (for thread list rendering).
    pub snippet: Option<String>,
}

/// A snoozed message record backing the `snoozed` SQLite table.
///
/// Snoozing moves a message from its original folder to a dedicated
/// `Snoozed` IMAP folder and records a `return_at` timestamp. When the
/// return time elapses, a background sweep (`envelope unsnooze --once`
/// or the `envelope serve` ticker) moves it back to its original folder.
///
/// The DB stores UID as INTEGER (SQLite has no u32) and `reply_received`
/// as INTEGER (0/1). Rust-side fields reflect the logical types used by
/// callers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnoozedMessage {
    /// UUID for this snooze record (primary key).
    pub id: String,
    /// Owning account identifier (usually an email address).
    pub account: String,
    /// IMAP UID of the message in `snoozed_folder` at time of insertion.
    /// Stored as INTEGER in SQLite.
    pub uid: u32,
    /// Folder the message was in before snoozing (e.g., `INBOX`).
    pub original_folder: String,
    /// Folder the message was moved to (default: `Snoozed`).
    pub snoozed_folder: String,
    /// ISO 8601 datetime when the message should return to its original folder.
    pub return_at: String,
    /// Message-ID header (for idempotent re-find after UID changes).
    pub message_id: Option<String>,
    /// Subject at time of snoozing (for display only).
    pub subject: Option<String>,
    /// ISO 8601 datetime the record was created.
    pub created_at: String,
    /// Optional reason code: `follow-up`, `waiting-reply`, `defer`, `reminder`, `review`.
    pub reason: Option<String>,
    /// Optional user note / annotation.
    pub note: Option<String>,
    /// Optional recipient grouping (for "waiting for X's reply" follow-ups).
    pub recipient: Option<String>,
    /// How many times this snooze has been escalated (e.g., bumped forward).
    pub escalation_tier: i32,
    /// True if the original recipient has replied (relevant for waiting-reply snoozes).
    pub reply_received: bool,
}

/// Discovery result for IMAP/SMTP auto-configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryResult {
    pub domain: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_source: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_source: String,
}

// ── Message tagging + scoring (v0.4.0) ──────────────────────────────

/// A freeform tag attached to a message, keyed on Message-ID for
/// stability across folder moves and UIDVALIDITY resets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageTag {
    pub account_id: String,
    pub message_id: String,
    pub tag: String,
    pub uid: Option<i64>,
    pub folder: Option<String>,
    pub created_at: String,
}

/// A numeric score on a named dimension (e.g., "urgent", "interesting")
/// attached to a message. Keyed on Message-ID for stability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageScore {
    pub account_id: String,
    pub message_id: String,
    pub dimension: String,
    pub value: f64,
    pub uid: Option<i64>,
    pub folder: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A mail rule: match expression + action, evaluated by the rules engine.
///
/// Match expressions and actions are stored as JSON (not a DSL) so the
/// CLI can construct them from flags (`--match-from`, `--match-tag`,
/// `--match-score-above`) without a parser. A human-readable DSL may
/// come in v0.5 as syntactic sugar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub account_id: String,
    pub name: String,
    /// JSON-serialized MatchExpr.
    pub match_expr: String,
    /// JSON-serialized Action.
    pub action: String,
    pub enabled: bool,
    pub priority: i64,
    /// If true, stop evaluating further rules after this one fires.
    pub stop: bool,
    /// Computed: can this rule be expressed in Sieve?
    pub sieve_exportable: bool,
    pub hit_count: i64,
    pub last_hit_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}
