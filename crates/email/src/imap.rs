// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::pin::pin;
use std::sync::Arc;

use async_imap::Session;
use chrono::{DateTime, FixedOffset};
use envelope_email_store::models::{
    AccountWithCredentials, AttachmentMeta, FolderStats, Message, MessageSummary,
};
use futures_util::StreamExt;
use mail_parser::MimeHeaders;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tracing::{debug, info, warn};

use crate::errors::ImapError;

/// Reject strings containing characters that could be used for IMAP command injection.
pub fn validate_imap_input(s: &str) -> Result<(), ImapError> {
    if s.contains('\r')
        || s.contains('\n')
        || s.contains('\0')
        || s.contains('{')
        || s.contains('}')
    {
        return Err(ImapError::Protocol(
            "invalid characters in input".to_string(),
        ));
    }
    Ok(())
}

/// Format a mailbox name as a quoted IMAP string for commands that async-imap
/// does not quote internally, such as UID COPY.
///
/// async-imap quotes mailbox arguments for SELECT/STATUS, but `uid_copy` places
/// the target mailbox directly into the command. Passing a bare mailbox with a
/// space, e.g. WorkMail/Exchange `Junk E-mail`, makes the server parse only the
/// first atom and fail with "folder not found". Quoting is valid for ordinary
/// mailbox names too and preserves literal names while escaping quoted-string
/// metacharacters.
pub fn imap_mailbox_arg(mailbox: &str) -> String {
    format!("\"{}\"", mailbox.replace('\\', r"\\").replace('"', "\\\""))
}

/// IMAP `SEARCH` key tokens recognized by RFC 3501 (plus common extensions).
///
/// Used to decide whether a user-supplied search string is already a
/// field-qualified IMAP query (e.g. `FROM bob`, `SUBJECT foo`, `OR ...`) or a
/// bare free-text term (e.g. `Hillan`, `régimen matrimonial`) that should be
/// treated as a `TEXT` search instead of being passed through raw — a raw bare
/// term is not a valid IMAP search key and silently returns zero matches on
/// most servers. See issue #63.
const IMAP_SEARCH_KEYS: &[&str] = &[
    "ALL",
    "ANSWERED",
    "BCC",
    "BEFORE",
    "BODY",
    "CC",
    "DELETED",
    "DRAFT",
    "FLAGGED",
    "FROM",
    "HEADER",
    "KEYWORD",
    "LARGER",
    "NEW",
    "NOT",
    "OLD",
    "ON",
    "OR",
    "RECENT",
    "SEEN",
    "SENTBEFORE",
    "SENTON",
    "SENTSINCE",
    "SINCE",
    "SMALLER",
    "SUBJECT",
    "TEXT",
    "TO",
    "UID",
    "UNANSWERED",
    "UNDELETED",
    "UNDRAFT",
    "UNFLAGGED",
    "UNKEYWORD",
    "UNSEEN",
];

/// Normalize a user search query into a valid IMAP `SEARCH` criteria string.
///
/// If the query already begins with a recognized IMAP search key (or a `(`
/// grouping / `*` charset-style construct), it is treated as an already
/// field-qualified query and passed through unchanged. Otherwise the entire
/// query is treated as bare free text and wrapped as `TEXT "<query>"` so bare
/// terms search message text instead of silently matching nothing.
///
/// The wrapped form escapes IMAP quoted-string metacharacters. Callers must
/// still run [`validate_imap_input`] (CRLF/NUL/brace rejection) on the result.
pub fn normalize_search_query(query: &str) -> String {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return trimmed.to_string();
    }

    // Grouped or already-structured queries pass through untouched.
    if trimmed.starts_with('(') {
        return trimmed.to_string();
    }

    let first_token = trimmed
        .split_whitespace()
        .next()
        .unwrap_or(trimmed)
        .to_ascii_uppercase();

    if IMAP_SEARCH_KEYS.contains(&first_token.as_str()) {
        return trimmed.to_string();
    }

    // Bare free-text term: wrap as a TEXT search with a quoted, escaped argument.
    let escaped = trimmed.replace('\\', r"\\").replace('"', "\\\"");
    format!("TEXT \"{escaped}\"")
}

pub type ImapSession = Session<TlsStream<TcpStream>>;

#[derive(Debug, Clone)]
pub struct RawMessage {
    pub uid: u32,
    pub message_id: Option<String>,
    pub flags: Vec<String>,
    pub internal_date: Option<DateTime<FixedOffset>>,
    pub size: u32,
    pub rfc822: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct MessageHeader {
    pub uid: u32,
    pub message_id: Option<String>,
    pub size: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct PeekHeaderSummary {
    pub uid: u32,
    pub from_addr: Option<String>,
    pub subject: Option<String>,
    pub date: Option<String>,
}

pub const QUICKSTART_PEEK_FETCH_DESCRIPTOR: &str =
    "(UID BODY.PEEK[HEADER.FIELDS (FROM SUBJECT DATE)])";
/// Batch summary FETCH: envelope metadata plus the provider spam-scoring
/// headers, requested via `BODY.PEEK[HEADER.FIELDS (...)]`.
///
/// `BODY.PEEK` (not `BODY[]`) means the fetch never sets `\Seen`, and only the
/// two named header fields are transferred — no message bodies. This keeps
/// rule preview/run header-only and read-only. `message_summary_from_fetch`
/// derives `provider_spam` from these fields.
pub const FETCH_SUMMARY_DESCRIPTOR: &str =
    "(UID FLAGS ENVELOPE RFC822.SIZE BODY.PEEK[HEADER.FIELDS (X-MIGADU-SPAM-SCORE X-SPAM-SCORE)])";

#[derive(Debug, Clone, Copy)]
pub struct SelectedMailbox {
    pub exists: u32,
    pub uid_validity: Option<u32>,
    pub uid_next: Option<u32>,
}

impl SelectedMailbox {
    pub fn uidvalidity_key(self) -> u32 {
        self.uid_validity.unwrap_or(0)
    }

    pub fn last_uid(self) -> Option<u32> {
        self.uid_next.and_then(|uid_next| uid_next.checked_sub(1))
    }
}

/// IMAP client wrapping an authenticated async-imap session.
pub struct ImapClient {
    session: ImapSession,
    /// True once this session has already emitted the "bare EXPUNGE fallback"
    /// warning, so [`expunge_uids`] warns at most once per connection.
    warned_bare_expunge: bool,
}

impl ImapClient {
    pub fn session_mut(&mut self) -> &mut ImapSession {
        &mut self.session
    }
}

/// How [`expunge_uids`] will remove `\Deleted` messages, chosen from the
/// server's advertised capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExpungeStrategy {
    /// Server advertises `UIDPLUS`: scope the expunge to exactly `seq_set`.
    UidScoped(String),
    /// Server lacks `UIDPLUS`: bare `EXPUNGE` (removes ALL `\Deleted` messages).
    BareAll,
}

/// Pure decision + command-string formatter, split out so both the capability
/// choice and the exact `UID EXPUNGE <seq_set>` wire form are unit-testable
/// without a live session. `has_uidplus` comes from
/// `Capabilities::has_str("UIDPLUS")` at the call site.
fn choose_expunge_strategy(has_uidplus: bool, seq_set: &str) -> ExpungeStrategy {
    if has_uidplus {
        ExpungeStrategy::UidScoped(format!("UID EXPUNGE {seq_set}"))
    } else {
        ExpungeStrategy::BareAll
    }
}

/// Expunge exactly the messages named by `seq_set`, never other sessions'
/// `\Deleted` messages.
///
/// The currently selected mailbox must already have `\Deleted` set on the
/// target UIDs. If the server advertises `UIDPLUS` (RFC 4315) we issue
/// `UID EXPUNGE <seq_set>`, which removes only messages that are both
/// `\Deleted` AND in `seq_set` — so a `\Deleted` message flagged by a
/// concurrent client survives.
///
/// If the server lacks `UIDPLUS` we fall back to a bare `EXPUNGE`, which
/// removes EVERY `\Deleted` message in the mailbox. That is the RFC 3501
/// behavior and can collaterally delete messages another session flagged;
/// there is no safe alternative on such servers, so we accept the residual
/// risk and emit a one-time warning per connection.
///
/// `seq_set` is validated to contain no injection characters before use.
pub async fn expunge_uids(client: &mut ImapClient, seq_set: &str) -> Result<(), ImapError> {
    validate_imap_input(seq_set)?;

    let has_uidplus = client
        .session
        .capabilities()
        .await
        .map_err(|e| ImapError::Protocol(format!("CAPABILITY: {e}")))?
        .has_str("UIDPLUS");

    match choose_expunge_strategy(has_uidplus, seq_set) {
        ExpungeStrategy::UidScoped(_) => {
            let stream = client
                .session
                .uid_expunge(seq_set)
                .await
                .map_err(|e| ImapError::Protocol(format!("UID EXPUNGE {seq_set}: {e}")))?;
            let mut stream = pin!(stream);
            while let Some(_item) = stream.next().await {}
        }
        ExpungeStrategy::BareAll => {
            if !client.warned_bare_expunge {
                client.warned_bare_expunge = true;
                warn!(
                    "server lacks UIDPLUS; falling back to bare EXPUNGE, which removes \
                     ALL \\Deleted messages in the mailbox and may collaterally delete \
                     messages flagged by other sessions"
                );
            }
            let expunge_stream = client
                .session
                .expunge()
                .await
                .map_err(|e| ImapError::Protocol(format!("EXPUNGE: {e}")))?;
            let mut stream = pin!(expunge_stream);
            while let Some(_item) = stream.next().await {}
        }
    }
    Ok(())
}

/// Connect to an IMAP server over TLS and authenticate.
pub async fn connect(account: &AccountWithCredentials) -> Result<ImapClient, ImapError> {
    let host = &account.account.imap_host;
    let port = account.account.imap_port;
    let username = account.effective_imap_username();
    let password = account.effective_imap_password();

    info!("connecting to IMAP {host}:{port} as {username}");

    let tcp = TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| ImapError::Connection(format!("{host}:{port}: {e}")))?;

    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    let connector = TlsConnector::from(Arc::new(tls_config));
    let server_name = rustls::pki_types::ServerName::try_from(host.as_str())
        .map_err(|e| ImapError::Connection(format!("invalid server name {host}: {e}")))?
        .to_owned();

    let tls_stream = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| ImapError::Connection(format!("TLS handshake with {host}: {e}")))?;

    let mut client = async_imap::Client::new(tls_stream);

    // Drain the server greeting before issuing LOGIN. async-imap's `Client::new`
    // does not consume the untagged `* OK ...` greeting; if we pipeline LOGIN
    // before reading it, some Dovecot-compatible servers can reset the
    // connection. The canonical async-imap pattern is to read the
    // greeting first — see the crate's lib.rs docs.
    read_imap_greeting(&mut client, host).await?;

    let session = client
        .login(username, password)
        .await
        .map_err(|(e, _)| ImapError::Auth(format!("login failed for {username}@{host}: {e}")))?;

    debug!("IMAP session established for {username}@{host}");
    Ok(ImapClient {
        session,
        warned_bare_expunge: false,
    })
}

/// Read and discard the untagged `* OK ...` greeting from a freshly constructed
/// `async_imap::Client`. Returns an `ImapError::Connection` if the server closes
/// the stream without a greeting or returns an I/O error mid-greeting.
///
/// `host` is used only for error context and never logged with credentials.
pub(crate) async fn read_imap_greeting<T>(
    client: &mut async_imap::Client<T>,
    host: &str,
) -> Result<(), ImapError>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + std::fmt::Debug + Send,
{
    let _greeting = client
        .read_response()
        .await
        .ok_or_else(|| ImapError::Connection(format!("no IMAP greeting from {host}")))?
        .map_err(|e| ImapError::Connection(format!("greeting from {host}: {e}")))?;
    Ok(())
}

/// List all mailbox folders.
pub async fn list_folders(client: &mut ImapClient) -> Result<Vec<String>, ImapError> {
    let mailboxes = client
        .session
        .list(Some(""), Some("*"))
        .await
        .map_err(|e| ImapError::Protocol(format!("LIST command failed: {e}")))?;

    let mut folders = Vec::new();
    let mut stream = mailboxes;
    while let Some(item) = stream.next().await {
        match item {
            Ok(mailbox) => folders.push(mailbox.name().to_string()),
            Err(e) => return Err(ImapError::Protocol(format!("LIST parse error: {e}"))),
        }
    }

    debug!("listed {} folders", folders.len());
    Ok(folders)
}

/// Return the mailbox the server flags with the RFC 6154 SPECIAL-USE `\Drafts`
/// attribute, if any.
///
/// This is the most reliable, name- and language-agnostic way to locate the
/// Drafts folder: the server names the role directly, so a mailbox called
/// `Brouillons`, `Entwürfe`, `[Gmail]/Drafts`, or `INBOX.Drafts` all resolve
/// the same way without guessing. Returns `Ok(None)` on servers that don't
/// advertise SPECIAL-USE, so callers fall back to name-based detection.
pub async fn drafts_special_use_folder(
    client: &mut ImapClient,
) -> Result<Option<String>, ImapError> {
    use async_imap::types::NameAttribute;
    let mailboxes = client
        .session
        .list(Some(""), Some("*"))
        .await
        .map_err(|e| ImapError::Protocol(format!("LIST (special-use) failed: {e}")))?;

    let mut stream = mailboxes;
    while let Some(item) = stream.next().await {
        let mailbox = item.map_err(|e| ImapError::Protocol(format!("LIST parse error: {e}")))?;
        if mailbox
            .attributes()
            .iter()
            .any(|attr| matches!(attr, NameAttribute::Drafts))
        {
            let name = mailbox.name().to_string();
            debug!("SPECIAL-USE \\Drafts folder: {name}");
            return Ok(Some(name));
        }
    }
    Ok(None)
}

/// Fetch stats for a single folder via IMAP `STATUS (MESSAGES RECENT UNSEEN)`.
///
/// Unlike `fetch_inbox`, this does NOT `SELECT` the folder (which would cause
/// unsolicited responses on some servers); it uses the STATUS command which
/// is read-only and designed for this purpose. Suitable for sidebar rendering
/// where we want counts without switching the active mailbox.
pub async fn folder_stats(client: &mut ImapClient, folder: &str) -> Result<FolderStats, ImapError> {
    validate_imap_input(folder)?;

    let mailbox = client
        .session
        .status(folder, "(MESSAGES RECENT UNSEEN)")
        .await
        .map_err(|e| ImapError::Protocol(format!("STATUS {folder}: {e}")))?;

    Ok(FolderStats {
        folder: folder.to_string(),
        exists: mailbox.exists,
        recent: mailbox.recent,
        unseen: mailbox.unseen,
    })
}

/// Fetch stats for every folder in the account, returning one [`FolderStats`]
/// per folder (in the same order as `list_folders`). Folders that fail the
/// STATUS query are skipped with a warning rather than propagating the error.
pub async fn list_folder_stats(client: &mut ImapClient) -> Result<Vec<FolderStats>, ImapError> {
    let folders = list_folders(client).await?;
    let mut stats = Vec::with_capacity(folders.len());
    for folder in &folders {
        match folder_stats(client, folder).await {
            Ok(s) => stats.push(s),
            Err(e) => {
                warn!("folder_stats skipped {folder}: {e}");
                // Emit a zeroed entry so the sidebar still shows the folder name.
                stats.push(FolderStats {
                    folder: folder.clone(),
                    exists: 0,
                    recent: 0,
                    unseen: None,
                });
            }
        }
    }
    Ok(stats)
}

/// Fetch message summaries from a folder.
pub async fn fetch_inbox(
    client: &mut ImapClient,
    folder: &str,
    limit: u32,
) -> Result<Vec<MessageSummary>, ImapError> {
    validate_imap_input(folder)?;
    if limit == 0 {
        return Ok(Vec::new());
    }

    let mailbox = client
        .session
        .select(folder)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {folder}: {e}")))?;

    let exists = mailbox.exists;
    if exists == 0 {
        return Ok(Vec::new());
    }

    let start = if exists > limit {
        exists - limit + 1
    } else {
        1
    };
    let range = format!("{start}:{exists}");

    let messages = client
        .session
        .fetch(&range, FETCH_SUMMARY_DESCRIPTOR)
        .await
        .map_err(|e| ImapError::Protocol(format!("FETCH {range}: {e}")))?;

    let mut summaries = Vec::new();
    let mut stream = messages;
    while let Some(item) = stream.next().await {
        match item {
            Ok(fetch) => summaries.push(message_summary_from_fetch(&fetch)),
            Err(e) => return Err(ImapError::Protocol(format!("FETCH parse error: {e}"))),
        }
    }

    Ok(summaries)
}

/// Fetch message summaries after opening the folder read-only with IMAP
/// `EXAMINE`.
///
/// This returns the same envelope-only shape as [`fetch_inbox`] without
/// selecting the mailbox read-write. It is intended for dashboard and aggregate
/// views that only need list metadata and must not mutate mailbox state.
pub async fn fetch_folder_summaries_read_only(
    client: &mut ImapClient,
    folder: &str,
    limit: u32,
) -> Result<Vec<MessageSummary>, ImapError> {
    let selected = examine_folder_info(client, folder).await?;
    if selected.exists == 0 || limit == 0 {
        return Ok(Vec::new());
    }

    let start = if selected.exists > limit {
        selected.exists - limit + 1
    } else {
        1
    };
    let range = format!("{start}:{}", selected.exists);

    let messages = client
        .session
        .fetch(&range, FETCH_SUMMARY_DESCRIPTOR)
        .await
        .map_err(|e| ImapError::Protocol(format!("FETCH {range}: {e}")))?;

    let mut summaries = Vec::new();
    let mut stream = messages;
    while let Some(item) = stream.next().await {
        match item {
            Ok(fetch) => summaries.push(message_summary_from_fetch(&fetch)),
            Err(e) => return Err(ImapError::Protocol(format!("FETCH parse error: {e}"))),
        }
    }

    Ok(summaries)
}

fn message_summary_from_fetch(fetch: &async_imap::types::Fetch) -> MessageSummary {
    let uid = fetch.uid.unwrap_or(0);
    let flags: Vec<String> = fetch.flags().map(|f| format!("{f:?}")).collect();
    let size = fetch.size.unwrap_or(0);

    let (from_addr, to_addr, subject, date, message_id) = if let Some(env) = fetch.envelope() {
        let from = imap_envelope_addresses(&env.from);
        let to = imap_envelope_addresses(&env.to);
        let subj = env
            .subject
            .as_ref()
            .map(|s| decode_rfc2047(s))
            .unwrap_or_default();
        let dt = env
            .date
            .as_ref()
            .map(|d| String::from_utf8_lossy(d).to_string());
        let mid = env
            .message_id
            .as_ref()
            .map(|m| String::from_utf8_lossy(m).to_string());
        (from, to, subj, dt, mid)
    } else {
        (String::new(), String::new(), String::new(), None, None)
    };

    // Derive the provider spam score from the peeked header fields requested by
    // `FETCH_SUMMARY_DESCRIPTOR`. `Fetch::header()` carries the
    // `BODY[HEADER.FIELDS (...)]` section bytes.
    let provider_spam = fetch.header().and_then(provider_spam_from_header_bytes);

    MessageSummary {
        uid,
        message_id,
        from_addr,
        to_addr,
        subject,
        date,
        flags,
        size,
        provider_spam,
    }
}

/// Decode RFC 2047 encoded words in IMAP ENVELOPE fields.
///
/// IMAP ENVELOPE returns subjects and addresses as raw bytes, which may
/// contain RFC 2047 encoded words like `=?utf-8?q?Hello_World?=` or
/// `=?utf-8?b?SGVsbG8=?=`. This function decodes them to plain text.
///
/// Handles:
/// - Q-encoding (quoted-printable variant for headers)
/// - B-encoding (base64)
/// - UTF-8 and ASCII charsets (most common in practice)
/// - Multiple encoded words separated by whitespace
///
/// For non-UTF-8 charsets (iso-8859-1, windows-1252, etc.), returns the
/// raw decoded bytes as lossy UTF-8 — imperfect but better than showing
/// `=?iso-8859-1?q?...?=` to the user.
fn decode_rfc2047(raw: &[u8]) -> String {
    let input = String::from_utf8_lossy(raw);

    // Fast path: no encoded words
    if !input.contains("=?") {
        return input.to_string();
    }

    let mut result = String::new();
    let mut remaining = input.as_ref();

    while let Some(start) = remaining.find("=?") {
        // Text before the encoded word
        result.push_str(&remaining[..start]);
        remaining = &remaining[start..];

        // Parse the RFC 2047 encoded word: =?charset?encoding?text?=
        //
        // We must locate the three `?` delimiters precisely rather than
        // searching for the first `?=` substring.  The naive `find("?=")`
        // approach fires on the `?=` formed by the `?` separator between
        // the encoding type and the encoded text when the text begins with
        // `=` (e.g. Q-encoded `=?UTF-8?Q?=C2=A1...?=` has `?=` at the
        // boundary between `Q` and `=C2`, not just at the closing `?=`).
        //
        // Algorithm: starting just past `=?`, walk forward to find:
        //   1. first  `?`  → end of charset
        //   2. second `?`  → end of encoding (single char, B or Q)
        //   3. closing `?=`→ end of encoded text
        if let Some(word) = parse_encoded_word(remaining) {
            let (charset, encoding_char, text, word_len) = word;
            remaining = &remaining[word_len..];

            // Strip whitespace between consecutive encoded words (RFC 2047 §6.2)
            if remaining.starts_with(' ') || remaining.starts_with('\t') {
                if remaining.trim_start().starts_with("=?") {
                    remaining = &remaining[remaining.find("=?").unwrap_or(0)..];
                }
            }

            let _ = charset; // TODO: proper charset conversion for non-UTF-8
            let decoded_bytes = match encoding_char.to_ascii_uppercase() {
                b'Q' => decode_q_encoding(text),
                b'B' => {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD
                        .decode(text)
                        .unwrap_or_else(|_| text.as_bytes().to_vec())
                }
                _ => text.as_bytes().to_vec(),
            };

            result.push_str(&String::from_utf8_lossy(&decoded_bytes));
        } else {
            // No valid encoded word at this position — emit as-is and advance
            result.push_str(remaining);
            remaining = "";
        }
    }

    result.push_str(remaining);
    result
}

/// Parse one RFC 2047 encoded word at the start of `s`.
///
/// Returns `(charset, encoding_byte, encoded_text, total_word_len)` or
/// `None` if `s` does not begin with a well-formed encoded word.
///
/// The format is `=?<charset>?<encoding>?<text>?=` where `<encoding>` is a
/// single byte (`B` or `Q`, case-insensitive) and `<text>` does not contain
/// `?` characters.  We locate the three `?` delimiters explicitly so that
/// any `?=` substring inside `<text>` (which cannot occur for B/Q, but may
/// occur in malformed input) does not cause premature termination.
fn parse_encoded_word(s: &str) -> Option<(&str, u8, &str, usize)> {
    // Must start with =?
    let rest = s.strip_prefix("=?")?;

    // Find first ? → end of charset
    let charset_end = rest.find('?')?;
    let charset = &rest[..charset_end];

    // After charset?, find second ? → end of encoding (must be exactly 1 byte)
    let after_charset = &rest[charset_end + 1..];
    let encoding_end = after_charset.find('?')?;
    if encoding_end != 1 {
        // Encoding must be a single character (B or Q)
        return None;
    }
    let encoding_char = after_charset.as_bytes()[0];

    // After encoding?, find closing ?= → end of encoded text
    let after_encoding = &after_charset[2..]; // skip "X?"
    let text_end = after_encoding.find("?=")?;
    let text = &after_encoding[..text_end];

    // Total consumed = "=?" + charset + "?" + encoding + "?" + text + "?="
    let total = 2 + charset_end + 1 + 1 + 1 + text_end + 2;
    Some((charset, encoding_char, text, total))
}

/// Decode Q-encoding (RFC 2047 variant of quoted-printable for headers).
///
/// - `_` → space
/// - `=XX` → byte with hex value XX
/// - Everything else → literal
fn decode_q_encoding(input: &str) -> Vec<u8> {
    let mut result = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'_' => {
                result.push(b' ');
                i += 1;
            }
            b'=' if i + 2 < bytes.len() => {
                if let Ok(byte) =
                    u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16)
                {
                    result.push(byte);
                    i += 3;
                } else {
                    result.push(bytes[i]);
                    i += 1;
                }
            }
            other => {
                result.push(other);
                i += 1;
            }
        }
    }
    result
}

/// IMAP fetch descriptor used by `fetch_message`.
///
/// **Critical: must use `BODY.PEEK[]`, not `BODY[]`.** `BODY[]` auto-sets
/// the `\Seen` flag on the server as a side effect of fetching; `BODY.PEEK[]`
/// does not. The dashboard "read message" action uses this fetch, and
/// users expect messages to stay unread until they explicitly mark them.
///
/// If you change this constant, the `test_fetch_uses_body_peek` regression
/// test will fail. That's intentional — do not silently loosen this.
pub const FETCH_MESSAGE_DESCRIPTOR: &str = "(UID FLAGS BODY.PEEK[])";

/// Evidence collection must open source folders read-only.
pub const EVIDENCE_MAILBOX_OPEN_COMMAND: &str = "EXAMINE";

/// Full-message evidence capture descriptor.
///
/// This is intentionally identical to the backup raw fetch descriptor: UID and
/// metadata plus raw RFC822 bytes via BODY.PEEK[] so no \Seen mutation occurs.
pub const EVIDENCE_RAW_FETCH_DESCRIPTOR: &str = "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[])";

/// Fetch a full message by UID, parsing the body with mail-parser.
///
/// Uses `BODY.PEEK[]` so reading a message does NOT auto-mark it as seen.
/// Call [`mark_seen`] explicitly when the user indicates they want the
/// message flagged as read.
pub async fn fetch_message(
    client: &mut ImapClient,
    folder: &str,
    uid: u32,
) -> Result<Option<Message>, ImapError> {
    validate_imap_input(folder)?;

    client
        .session
        .select(folder)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {folder}: {e}")))?;

    let uid_range = format!("{uid}");
    let messages = client
        .session
        .uid_fetch(&uid_range, FETCH_MESSAGE_DESCRIPTOR)
        .await
        .map_err(|e| ImapError::Protocol(format!("UID FETCH {uid}: {e}")))?;

    // fetch_message expects exactly one message for the UID — take the first item.
    let mut stream = messages;
    let Some(item) = stream.next().await else {
        return Ok(None);
    };
    let fetch = item.map_err(|e| ImapError::Protocol(format!("UID FETCH parse error: {e}")))?;
    let body: &[u8] = fetch.body().unwrap_or_default();
    let Some(parsed) = mail_parser::MessageParser::default().parse(body) else {
        return Ok(None);
    };

    let flags: Vec<String> = fetch.flags().map(|f| format!("{f:?}")).collect();
    let from_addr = mp_first_address(parsed.from());
    let to_addrs = mp_all_addresses(parsed.to());
    let cc_addrs = mp_all_addresses(parsed.cc());
    // Keep the scalar fields as the first address for backward compatibility;
    // `to_addrs`/`cc_addrs` carry the complete recipient set.
    let to_addr = to_addrs.first().cloned().unwrap_or_default();
    let cc_addr = cc_addrs.first().cloned();

    let subject = parsed.subject().unwrap_or_default().to_string();
    let date = parsed.date().map(|d| d.to_rfc3339());
    let text_body = parsed.body_text(0).map(|t| t.to_string());
    let html_body = parsed.body_html(0).map(|h| h.to_string());
    let in_reply_to = parsed.in_reply_to().as_text().map(|s| s.to_string());
    let references = crate::threading::references_header(&parsed);
    let message_id = parsed.message_id().map(|s| s.to_string());
    // Read the provider spam-scoring headers straight from the raw RFC822 so a
    // single code path handles both this full fetch and the summary FETCH.
    let provider_spam = provider_spam_from_header_bytes(body);

    let attachments: Vec<AttachmentMeta> = parsed
        .attachments()
        .map(|a| {
            let ct: Option<&mail_parser::ContentType> = a.content_type();
            AttachmentMeta {
                filename: a.attachment_name().unwrap_or("unnamed").to_string(),
                content_type: ct
                    .map(|ct| {
                        let subtype = ct.subtype().unwrap_or("octet-stream");
                        format!("{}/{subtype}", ct.ctype())
                    })
                    .unwrap_or_else(|| "application/octet-stream".to_string()),
                size: a.len() as u64,
                content_id: a.content_id().map(|s: &str| s.to_string()),
            }
        })
        .collect();

    Ok(Some(Message {
        uid,
        message_id,
        from_addr,
        to_addr,
        cc_addr,
        to_addrs,
        cc_addrs,
        subject,
        date,
        text_body,
        html_body,
        in_reply_to,
        references,
        flags,
        attachments,
        provider_spam,
    }))
}

/// Append a message to a folder with the given flags.
///
/// `flags` should be in IMAP format, e.g. `"(\\Draft \\Seen)"`.
pub async fn append_message(
    client: &mut ImapClient,
    folder: &str,
    flags: &str,
    rfc822: &[u8],
) -> Result<(), ImapError> {
    append_message_with_date(client, folder, flags, None, rfc822).await
}

/// Append a raw RFC822 message to a folder with flags and optional INTERNALDATE.
pub async fn append_message_with_date(
    client: &mut ImapClient,
    folder: &str,
    flags: &str,
    internal_date: Option<DateTime<FixedOffset>>,
    rfc822: &[u8],
) -> Result<(), ImapError> {
    validate_imap_input(folder)?;
    let date = internal_date.map(|d| d.format("%d-%b-%Y %H:%M:%S %z").to_string());

    client
        .session
        .append(folder, Some(flags), date.as_deref(), rfc822)
        .await
        .map_err(|e| ImapError::Protocol(format!("APPEND to {folder}: {e}")))?;

    debug!("appended message to {folder} ({} bytes)", rfc822.len());
    Ok(())
}

/// Select a folder and return migration-relevant mailbox metadata.
pub async fn select_folder_info(
    client: &mut ImapClient,
    folder: &str,
) -> Result<SelectedMailbox, ImapError> {
    validate_imap_input(folder)?;
    let mailbox = client
        .session
        .select(folder)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {folder}: {e}")))?;

    Ok(SelectedMailbox {
        exists: mailbox.exists,
        uid_validity: mailbox.uid_validity,
        uid_next: mailbox.uid_next,
    })
}

/// Open a folder read-only via IMAP `EXAMINE` and return the same metadata
/// as `select_folder_info`.
///
/// `EXAMINE` is identical to `SELECT` except the mailbox is opened read-only
/// for the lifetime of the selected state — the server will not mutate
/// `\Recent` or set `\Seen` on subsequent fetches, and any `STORE`/`APPEND`
/// in this session is rejected. Backup export uses this so a source mailbox
/// can never be mutated by Envelope while we're reading it.
pub async fn examine_folder_info(
    client: &mut ImapClient,
    folder: &str,
) -> Result<SelectedMailbox, ImapError> {
    validate_imap_input(folder)?;
    let mailbox = client
        .session
        .examine(folder)
        .await
        .map_err(|e| ImapError::Protocol(format!("EXAMINE {folder}: {e}")))?;

    Ok(SelectedMailbox {
        exists: mailbox.exists,
        uid_validity: mailbox.uid_validity,
        uid_next: mailbox.uid_next,
    })
}

/// Evidence-specific wrapper around EXAMINE for readability at call sites.
pub async fn examine_folder_for_evidence(
    client: &mut ImapClient,
    folder: &str,
) -> Result<SelectedMailbox, ImapError> {
    examine_folder_info(client, folder).await
}

/// Create a folder if it does not already exist.
pub async fn create_folder_if_missing(
    client: &mut ImapClient,
    folder: &str,
) -> Result<(), ImapError> {
    validate_imap_input(folder)?;
    let folders = list_folders(client).await?;
    if folders.iter().any(|f| f == folder) {
        return Ok(());
    }
    client
        .session
        .create(folder)
        .await
        .map_err(|e| ImapError::Protocol(format!("CREATE {folder}: {e}")))?;
    Ok(())
}

/// Return whether a folder currently exists without creating it.
pub async fn folder_exists(client: &mut ImapClient, folder: &str) -> Result<bool, ImapError> {
    validate_imap_input(folder)?;
    let folders = list_folders(client).await?;
    Ok(folders.iter().any(|f| f == folder))
}

/// Fetch all raw messages from a folder without marking them seen.
pub async fn fetch_raw_messages(
    client: &mut ImapClient,
    folder: &str,
) -> Result<Vec<RawMessage>, ImapError> {
    let selected = select_folder_info(client, folder).await?;
    if selected.exists == 0 {
        return Ok(Vec::new());
    }

    let uid_sets = if let Some(last_uid) = selected.last_uid() {
        crate::migrate::uid_range_batches(1, last_uid, crate::migrate::DEFAULT_BATCH_SIZE)
    } else {
        let uids = list_selected_uids(client).await?;
        crate::migrate::uid_sequence_set_batches(&uids, crate::migrate::DEFAULT_BATCH_SIZE)
    };

    let mut out = Vec::new();
    for uid_set in uid_sets {
        out.extend(fetch_raw_messages_selected_uid_set(client, folder, &uid_set).await?);
    }
    Ok(out)
}

/// Return all UIDs in the currently selected mailbox.
pub async fn list_selected_uids(client: &mut ImapClient) -> Result<Vec<u32>, ImapError> {
    let uid_set = client
        .session
        .uid_search("ALL")
        .await
        .map_err(|e| ImapError::Protocol(format!("UID SEARCH ALL: {e}")))?;
    let mut uids: Vec<u32> = uid_set.into_iter().collect();
    uids.sort_unstable();
    Ok(uids)
}

/// Fetch a batch of raw messages from the currently selected folder.
pub async fn fetch_raw_messages_selected_uid_set(
    client: &mut ImapClient,
    folder: &str,
    uid_set: &str,
) -> Result<Vec<RawMessage>, ImapError> {
    validate_imap_input(folder)?;
    validate_uid_set(uid_set)?;

    let messages = client
        .session
        .uid_fetch(uid_set, EVIDENCE_RAW_FETCH_DESCRIPTOR)
        .await
        .map_err(|e| ImapError::Protocol(format!("UID FETCH {folder} {uid_set}: {e}")))?;
    let mut out = Vec::new();
    let mut stream = messages;
    while let Some(item) = stream.next().await {
        let fetch = item.map_err(|e| ImapError::Protocol(format!("UID FETCH parse error: {e}")))?;
        let uid = fetch
            .uid
            .ok_or_else(|| ImapError::Protocol("UID FETCH returned message without UID".into()))?;
        let body = fetch
            .body()
            .ok_or_else(|| missing_body_protocol_error(folder, uid_set, Some(uid)))?;
        let parsed = mail_parser::MessageParser::default().parse(body);
        out.push(RawMessage {
            uid,
            message_id: parsed.and_then(|m| m.message_id().map(|s| s.to_string())),
            flags: fetch.flags().map(|f| format!("{f:?}")).collect(),
            internal_date: fetch.internal_date(),
            size: fetch.size.unwrap_or(body.len() as u32),
            rfc822: body.to_vec(),
        });
    }
    Ok(out)
}

/// Build a protocol error for a UID FETCH response that has no `BODY.PEEK[]`
/// section. Migration must surface this rather than silently under-counting —
/// every fetched UID has to round-trip a body or fail loudly.
pub(crate) fn missing_body_protocol_error(
    folder: &str,
    uid_set: &str,
    uid: Option<u32>,
) -> ImapError {
    let location = match uid {
        Some(uid) => format!("UID {uid}"),
        None => "unknown UID".to_string(),
    };
    ImapError::Protocol(format!(
        "UID FETCH {folder} {uid_set} returned no BODY.PEEK[] for {location}"
    ))
}

/// Fetch only recent headers after opening a mailbox read-only with EXAMINE.
pub async fn peek_folder_headers_read_only(
    client: &mut ImapClient,
    folder: &str,
    limit: u32,
) -> Result<Vec<PeekHeaderSummary>, ImapError> {
    let selected = examine_folder_info(client, folder).await?;
    if selected.exists == 0 || limit == 0 {
        return Ok(Vec::new());
    }

    let limit = limit.min(25);
    let start = if selected.exists > limit {
        selected.exists - limit + 1
    } else {
        1
    };
    let range = format!("{start}:{}", selected.exists);
    let messages = client
        .session
        .fetch(&range, QUICKSTART_PEEK_FETCH_DESCRIPTOR)
        .await
        .map_err(|e| ImapError::Protocol(format!("FETCH {range} HEADER: {e}")))?;

    let mut out = Vec::new();
    let mut stream = messages;
    while let Some(item) = stream.next().await {
        let fetch = item.map_err(|e| ImapError::Protocol(format!("FETCH parse error: {e}")))?;
        let uid = fetch.uid.unwrap_or(0);
        let (from_addr, subject, date) = fetch
            .body()
            .and_then(|body| mail_parser::MessageParser::default().parse(body))
            .map(|parsed| {
                let from = parsed
                    .from()
                    .and_then(|a| a.first())
                    .and_then(|a| a.address())
                    .map(|s| s.to_string());
                let subject = parsed.subject().map(|s| s.to_string());
                let date = parsed.date().map(|d| d.to_rfc3339());
                (from, subject, date)
            })
            .unwrap_or((None, None, None));
        out.push(PeekHeaderSummary {
            uid,
            from_addr,
            subject,
            date,
        });
    }
    out.sort_by(|a, b| b.uid.cmp(&a.uid));
    Ok(out)
}

/// Fetch only migration-planning headers for a batch of source UIDs.
pub async fn fetch_message_headers_selected_uid_set(
    client: &mut ImapClient,
    folder: &str,
    uid_set: &str,
) -> Result<Vec<MessageHeader>, ImapError> {
    validate_imap_input(folder)?;
    validate_uid_set(uid_set)?;

    let messages = client
        .session
        .uid_fetch(
            uid_set,
            "(UID RFC822.SIZE BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])",
        )
        .await
        .map_err(|e| ImapError::Protocol(format!("UID FETCH {folder} {uid_set} HEADER: {e}")))?;
    let mut out = Vec::new();
    let mut stream = messages;
    while let Some(item) = stream.next().await {
        let fetch = item.map_err(|e| ImapError::Protocol(format!("UID FETCH parse error: {e}")))?;
        let uid = fetch
            .uid
            .ok_or_else(|| ImapError::Protocol("UID FETCH returned message without UID".into()))?;
        // `BODY.PEEK[HEADER.FIELDS (…)]` responses parse to
        // `SectionPath::Full(MessageSection::Header)`, which async-imap
        // surfaces via `Fetch::header()` — `Fetch::body()` only matches the
        // section-less `BODY[]`/`RFC822` and returns None for these fetches.
        let message_id = fetch
            .header()
            .and_then(parse_message_id_from_header_section);
        out.push(MessageHeader {
            uid,
            message_id,
            size: fetch.size,
        });
    }
    Ok(out)
}

/// Parse the Message-ID from the raw bytes of a
/// `BODY[HEADER.FIELDS (MESSAGE-ID)]` FETCH section (header lines terminated
/// by an empty line). Returns the id as mail_parser normalizes it (angle
/// brackets stripped), or `None` when the section carries no parseable
/// Message-ID.
pub fn parse_message_id_from_header_section(section: &[u8]) -> Option<String> {
    mail_parser::MessageParser::default()
        .parse(section)
        .and_then(|m| m.message_id().map(str::to_string))
}

/// Extract the first value of header `name` from a raw RFC822 header block,
/// unfolding folded continuation lines. Case-insensitive on the field name;
/// scanning stops at the blank line that ends the header block, so passing a
/// full message body only reads its top-level headers.
///
/// Central helper so the full-message and summary FETCH paths read the same
/// headers the same way.
pub fn header_value_from_bytes(section: &[u8], name: &str) -> Option<String> {
    let text = String::from_utf8_lossy(section);
    // `Some(value)` while accumulating the target header's (possibly folded)
    // value; stays `None` until the first matching header line is seen.
    let mut capturing: Option<String> = None;

    for raw_line in text.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            break; // end of the header block
        }
        if line.starts_with([' ', '\t']) {
            // Folded continuation — only meaningful while reading the target.
            if let Some(value) = capturing.as_mut() {
                if !value.is_empty() {
                    value.push(' ');
                }
                value.push_str(line.trim());
            }
            continue;
        }
        // A new header line begins. If we were already reading the target, its
        // folded value is now complete — first match wins.
        if capturing.is_some() {
            break;
        }
        match line.split_once(':') {
            Some((field, value)) if field.trim().eq_ignore_ascii_case(name) => {
                capturing = Some(value.trim().to_string());
            }
            _ => {}
        }
    }
    capturing.map(|value| value.trim().to_string())
}

/// Derive the `provider_spam` score from a raw header block (a
/// `BODY.PEEK[HEADER.FIELDS (X-MIGADU-SPAM-SCORE X-SPAM-SCORE)]` section or a
/// full RFC822 message). Prefers `X-Migadu-Spam-Score`, falling back to
/// `X-Spam-Score`; returns `None` when neither carries a finite number.
pub fn provider_spam_from_header_bytes(section: &[u8]) -> Option<f64> {
    let migadu = header_value_from_bytes(section, "X-Migadu-Spam-Score");
    let generic = header_value_from_bytes(section, "X-Spam-Score");
    crate::rules::provider_spam_from_headers(migadu.as_deref(), generic.as_deref())
}

fn validate_uid_set(uid_set: &str) -> Result<(), ImapError> {
    if uid_set.is_empty()
        || !uid_set
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b':' | b',' | b'*'))
    {
        return Err(ImapError::Protocol("invalid UID set".to_string()));
    }
    Ok(())
}

/// Find a message UID by its Message-ID header in a given folder.
///
/// Uses IMAP SEARCH HEADER to locate the message.
pub async fn find_uid_by_message_id(
    client: &mut ImapClient,
    folder: &str,
    message_id: &str,
) -> Result<Option<u32>, ImapError> {
    validate_imap_input(folder)?;

    client
        .session
        .select(folder)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {folder}: {e}")))?;

    let search_query = message_id_search_query(message_id)?;
    let uid_set = client
        .session
        .uid_search(&search_query)
        .await
        .map_err(|e| ImapError::Protocol(format!("UID SEARCH {search_query}: {e}")))?;

    let uid = uid_set.into_iter().next();
    Ok(uid)
}

/// Find the single message whose Message-ID header equals `message_id`
/// exactly, or `None` when identity cannot be established unambiguously.
///
/// [`find_uid_by_message_id`] is best-effort: IMAP `SEARCH HEADER` is a
/// substring match and it returns an arbitrary first hit, so it must never be
/// treated as identity verification for destructive actions. This primitive
/// instead treats the search hits as *candidates*, fetches every candidate's
/// actual Message-ID header (`BODY.PEEK`, no flag changes), normalizes both
/// sides (whitespace, angle brackets), and requires exact equality. It returns
/// `Ok(Some(uid))` only when **exactly one** candidate matches exactly; zero
/// or multiple exact matches (duplicate Message-IDs) return `Ok(None)` so
/// callers with destructive intent skip instead of acting on an ambiguous
/// identity.
pub async fn find_unique_uid_by_exact_message_id(
    client: &mut ImapClient,
    folder: &str,
    message_id: &str,
) -> Result<Option<u32>, ImapError> {
    Ok(
        match find_exact_message_id_match(client, folder, message_id).await? {
            ExactMessageIdMatch::Unique(uid) => Some(uid),
            ExactMessageIdMatch::None | ExactMessageIdMatch::Ambiguous => None,
        },
    )
}

/// Classification of an exact Message-ID lookup, distinguishing "no exact match"
/// from "more than one exact match" so callers can report an explicit, stable
/// ambiguous status instead of collapsing both to `None` and inventing a UID.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ExactMessageIdMatch {
    /// Exactly one candidate's Message-ID matched exactly (its UID).
    Unique(u32),
    /// No candidate matched exactly.
    None,
    /// More than one candidate matched exactly (duplicate Message-IDs) — no
    /// arbitrary UID is returned.
    Ambiguous,
}

/// Resolve a Message-ID to a single UID by exact header match, distinguishing
/// zero / unique / ambiguous outcomes.
///
/// IMAP `SEARCH HEADER` is a substring match returning an arbitrary hit order,
/// so its results are treated only as *candidates*: this fetches every
/// candidate's actual Message-ID header (`BODY.PEEK`, no flag changes), compares
/// exactly after normalization, and returns [`ExactMessageIdMatch::Unique`] only
/// when exactly one candidate matches. Multiple exact matches yield
/// [`ExactMessageIdMatch::Ambiguous`] (never an arbitrary UID). Bounded: it
/// fetches only Message-ID headers of the search hits and never marks anything
/// read.
pub async fn find_exact_message_id_match(
    client: &mut ImapClient,
    folder: &str,
    message_id: &str,
) -> Result<ExactMessageIdMatch, ImapError> {
    let exact_uids = find_uids_by_exact_message_id(client, folder, message_id).await?;
    Ok(match exact_uids.as_slice() {
        [] => ExactMessageIdMatch::None,
        [uid] => ExactMessageIdMatch::Unique(*uid),
        _ => ExactMessageIdMatch::Ambiguous,
    })
}

/// Return every UID whose Message-ID header exactly matches `message_id`.
///
/// This keeps IMAP's substring-based SEARCH result out of destructive code:
/// every candidate header is fetched with BODY.PEEK and compared after strict
/// Message-ID normalization. Replacement cleanup may then remove all verified
/// copies of one logical draft, including duplicates left by an interrupted
/// earlier edit, without touching a merely similar Message-ID.
pub async fn find_uids_by_exact_message_id(
    client: &mut ImapClient,
    folder: &str,
    message_id: &str,
) -> Result<Vec<u32>, ImapError> {
    validate_imap_input(folder)?;

    client
        .session
        .select(folder)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {folder}: {e}")))?;

    let search_query = message_id_search_query(message_id)?;
    let uid_set = client
        .session
        .uid_search(&search_query)
        .await
        .map_err(|e| ImapError::Protocol(format!("UID SEARCH {search_query}: {e}")))?;

    let mut candidate_uids: Vec<u32> = uid_set.into_iter().collect();
    if candidate_uids.is_empty() {
        return Ok(Vec::new());
    }
    candidate_uids.sort_unstable();
    let uid_set_arg = candidate_uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");

    let headers = fetch_message_headers_selected_uid_set(client, folder, &uid_set_arg).await?;
    let Some(wanted) = normalize_message_id(message_id) else {
        return Ok(Vec::new());
    };
    let mut exact_uids: Vec<u32> = headers
        .into_iter()
        .filter_map(|header| {
            let candidate = normalize_message_id(header.message_id.as_deref()?)?;
            (candidate == wanted).then_some(header.uid)
        })
        .collect();
    exact_uids.sort_unstable();
    exact_uids.dedup();
    Ok(exact_uids)
}

/// Pure candidate selection for [`find_unique_uid_by_exact_message_id`]:
/// among `(uid, Message-ID header)` candidates produced by a substring-based
/// search, return the UID whose Message-ID equals `wanted` exactly after
/// normalization — and only when exactly one such candidate exists. Zero or
/// multiple exact matches return `None` (fail closed on ambiguity).
pub fn select_unique_exact_message_id(
    candidates: &[(u32, Option<String>)],
    wanted: &str,
) -> Option<u32> {
    match classify_exact_message_id_match(candidates, wanted) {
        ExactMessageIdMatch::Unique(uid) => Some(uid),
        ExactMessageIdMatch::None | ExactMessageIdMatch::Ambiguous => None,
    }
}

/// Pure classification of exact Message-ID candidates: among `(uid, Message-ID
/// header)` candidates from a substring search, distinguish no exact match, a
/// single exact match, and multiple exact matches (duplicate Message-IDs) after
/// normalization. Backs [`find_exact_message_id_match`].
pub fn classify_exact_message_id_match(
    candidates: &[(u32, Option<String>)],
    wanted: &str,
) -> ExactMessageIdMatch {
    let Some(wanted) = normalize_message_id(wanted) else {
        return ExactMessageIdMatch::None;
    };
    let mut exact = candidates.iter().filter_map(|(uid, header)| {
        let candidate = normalize_message_id(header.as_deref()?)?;
        (candidate == wanted).then_some(*uid)
    });
    let Some(unique) = exact.next() else {
        return ExactMessageIdMatch::None;
    };
    if exact.next().is_some() {
        ExactMessageIdMatch::Ambiguous
    } else {
        ExactMessageIdMatch::Unique(unique)
    }
}

/// Normalize a Message-ID for exact comparison: trim surrounding whitespace
/// and strip the angle brackets. Returns `None` when nothing remains. No
/// case folding — a case mismatch stays a mismatch (fail closed).
pub fn normalize_message_id(raw: &str) -> Option<String> {
    let bare = raw.trim().trim_matches(['<', '>']).trim();
    if bare.is_empty() {
        None
    } else {
        Some(bare.to_string())
    }
}

/// Search the currently selected/examined mailbox for evidence collection.
///
/// Callers must open the mailbox with `examine_folder_for_evidence` first.
pub async fn evidence_search_selected_uids(
    client: &mut ImapClient,
    query: &str,
) -> Result<Vec<u32>, ImapError> {
    validate_imap_input(query)?;
    let uid_set = client
        .session
        .uid_search(query)
        .await
        .map_err(|e| ImapError::Protocol(format!("UID SEARCH {query}: {e}")))?;
    let mut uids: Vec<u32> = uid_set.into_iter().collect();
    uids.sort_unstable();
    Ok(uids)
}

/// Search the currently selected/examined mailbox by one of the RFC5322
/// threading headers used for evidence expansion.
pub async fn evidence_search_selected_header_uids(
    client: &mut ImapClient,
    header_name: &str,
    value: &str,
) -> Result<Vec<u32>, ImapError> {
    let query = evidence_header_search_query(header_name, value)?;
    evidence_search_selected_uids(client, &query).await
}

fn message_id_search_query(message_id: &str) -> Result<String, ImapError> {
    Ok(format!(
        "HEADER Message-ID {}",
        imap_quoted_string_arg(message_id)?
    ))
}

pub fn evidence_header_search_query(header_name: &str, value: &str) -> Result<String, ImapError> {
    match header_name {
        "Message-ID" | "In-Reply-To" | "References" => Ok(format!(
            "HEADER {header_name} {}",
            imap_quoted_string_arg(value)?
        )),
        _ => Err(ImapError::Protocol(format!(
            "unsupported evidence thread header {header_name:?}"
        ))),
    }
}

fn imap_quoted_string_arg(value: &str) -> Result<String, ImapError> {
    if value.contains('\r') || value.contains('\n') || value.contains('\0') {
        return Err(ImapError::Protocol(
            "invalid characters in quoted IMAP string".to_string(),
        ));
    }

    Ok(format!(
        "\"{}\"",
        value.replace('\\', r"\\").replace('"', "\\\"")
    ))
}

/// Fetch List-Unsubscribe and List-Unsubscribe-Post headers for a message.
///
/// Returns `(list_unsubscribe, list_unsubscribe_post)` — both are None if
/// the headers are absent.
pub async fn fetch_list_unsubscribe_headers(
    client: &mut ImapClient,
    folder: &str,
    uid: u32,
) -> Result<(Option<String>, Option<String>), ImapError> {
    validate_imap_input(folder)?;

    client
        .session
        .select(folder)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {folder}: {e}")))?;

    let uid_range = format!("{uid}");
    let messages = client
        .session
        .uid_fetch(&uid_range, "BODY.PEEK[HEADER]")
        .await
        .map_err(|e| ImapError::Protocol(format!("UID FETCH {uid} HEADER: {e}")))?;

    let mut stream = messages;
    let Some(item) = stream.next().await else {
        return Ok((None, None));
    };
    let fetch = item.map_err(|e| ImapError::Protocol(format!("UID FETCH parse error: {e}")))?;
    let header_bytes = fetch.body().unwrap_or_default();

    let Some(parsed) = mail_parser::MessageParser::default().parse(header_bytes) else {
        return Ok((None, None));
    };

    let list_unsub = parsed
        .header_values("List-Unsubscribe")
        .find_map(|v| match v {
            mail_parser::HeaderValue::Text(t) => Some(t.to_string()),
            _ => None,
        });

    let list_unsub_post = parsed
        .header_values("List-Unsubscribe-Post")
        .find_map(|v| match v {
            mail_parser::HeaderValue::Text(t) => Some(t.to_string()),
            _ => None,
        });

    Ok((list_unsub, list_unsub_post))
}

/// Map human-readable flag names to IMAP flag format.
pub fn map_flag_name(flag: &str) -> String {
    match flag.to_lowercase().as_str() {
        "seen" => "\\Seen".to_string(),
        "flagged" => "\\Flagged".to_string(),
        "answered" => "\\Answered".to_string(),
        "draft" => "\\Draft".to_string(),
        "deleted" => "\\Deleted".to_string(),
        _ if flag.starts_with('\\') => flag.to_string(),
        _ => flag.to_string(),
    }
}

/// Search messages in a folder using IMAP SEARCH.
pub async fn search(
    client: &mut ImapClient,
    folder: &str,
    query: &str,
    limit: u32,
) -> Result<Vec<MessageSummary>, ImapError> {
    validate_imap_input(folder)?;
    validate_imap_input(query)?;

    // Map bare free-text terms to a TEXT search so they behave like the
    // field-qualified queries agents expect (issue #63).
    let search_criteria = normalize_search_query(query);

    client
        .session
        .select(folder)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {folder}: {e}")))?;

    let uid_set = client
        .session
        .uid_search(&search_criteria)
        .await
        .map_err(|e| ImapError::Protocol(format!("UID SEARCH {search_criteria}: {e}")))?;

    let mut uids: Vec<u32> = uid_set.into_iter().collect();

    // Sort ascending then reverse for newest first
    uids.sort_unstable();
    uids.reverse();
    uids.truncate(limit as usize);

    if uids.is_empty() {
        return Ok(Vec::new());
    }

    let uid_range = uids
        .iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let messages = client
        .session
        .uid_fetch(&uid_range, "(UID FLAGS ENVELOPE RFC822.SIZE)")
        .await
        .map_err(|e| ImapError::Protocol(format!("UID FETCH {uid_range}: {e}")))?;

    let mut summaries = Vec::new();
    let mut msg_stream = messages;
    while let Some(item) = msg_stream.next().await {
        match item {
            Ok(fetch) => {
                let uid = fetch.uid.unwrap_or(0);
                let flags: Vec<String> = fetch.flags().map(|f| format!("{f:?}")).collect();
                let size = fetch.size.unwrap_or(0);

                let (from_addr, to_addr, subject, date, message_id) =
                    if let Some(env) = fetch.envelope() {
                        let from = imap_envelope_addresses(&env.from);
                        let to = imap_envelope_addresses(&env.to);
                        let subj = env
                            .subject
                            .as_ref()
                            .map(|s| decode_rfc2047(s))
                            .unwrap_or_default();
                        let dt = env
                            .date
                            .as_ref()
                            .map(|d| String::from_utf8_lossy(d).to_string());
                        let mid = env
                            .message_id
                            .as_ref()
                            .map(|m| String::from_utf8_lossy(m).to_string());
                        (from, to, subj, dt, mid)
                    } else {
                        (String::new(), String::new(), String::new(), None, None)
                    };

                summaries.push(MessageSummary {
                    uid,
                    message_id,
                    from_addr,
                    to_addr,
                    subject,
                    date,
                    flags,
                    size,
                    // Search does not fetch spam-score headers; rules never run
                    // against search results, so leave the signal absent.
                    provider_spam: None,
                });
            }
            Err(e) => return Err(ImapError::Protocol(format!("UID FETCH parse error: {e}"))),
        }
    }

    Ok(summaries)
}

/// Move a message from one folder to another by UID (copy + delete).
pub async fn move_message(
    client: &mut ImapClient,
    uid: u32,
    from: &str,
    to: &str,
) -> Result<(), ImapError> {
    validate_imap_input(from)?;
    validate_imap_input(to)?;

    client
        .session
        .select(from)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {from}: {e}")))?;

    let uid_str = uid.to_string();

    let quoted_to = imap_mailbox_arg(to);

    client
        .session
        .uid_copy(&uid_str, &quoted_to)
        .await
        .map_err(|e| ImapError::Protocol(format!("UID COPY {uid} to {to}: {e}")))?;

    {
        let mut store_stream = client
            .session
            .uid_store(&uid_str, "+FLAGS (\\Deleted)")
            .await
            .map_err(|e| ImapError::Protocol(format!("UID STORE +FLAGS \\Deleted {uid}: {e}")))?;

        // Consume the store response stream
        while let Some(_item) = store_stream.next().await {}
    }

    // Scope the expunge to exactly this UID so we never remove another
    // session's \Deleted messages (see expunge_uids).
    expunge_uids(client, &uid_str).await?;

    debug!("moved UID {uid} from {from} to {to}");
    Ok(())
}

/// Copy a message from one folder to another by UID.
pub async fn copy_message(
    client: &mut ImapClient,
    uid: u32,
    from: &str,
    to: &str,
) -> Result<(), ImapError> {
    validate_imap_input(from)?;
    validate_imap_input(to)?;

    client
        .session
        .select(from)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {from}: {e}")))?;

    let uid_str = uid.to_string();
    let quoted_to = imap_mailbox_arg(to);

    client
        .session
        .uid_copy(&uid_str, &quoted_to)
        .await
        .map_err(|e| ImapError::Protocol(format!("UID COPY {uid} to {to}: {e}")))?;

    debug!("copied UID {uid} from {from} to {to}");
    Ok(())
}

/// Delete a message by UID (mark \Deleted + expunge).
pub async fn delete_message(
    client: &mut ImapClient,
    folder: &str,
    uid: u32,
) -> Result<(), ImapError> {
    validate_imap_input(folder)?;

    client
        .session
        .select(folder)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {folder}: {e}")))?;

    let uid_str = uid.to_string();

    {
        let mut store_stream = client
            .session
            .uid_store(&uid_str, "+FLAGS (\\Deleted)")
            .await
            .map_err(|e| ImapError::Protocol(format!("UID STORE +FLAGS \\Deleted {uid}: {e}")))?;

        while let Some(_item) = store_stream.next().await {}
    }

    // Scope the expunge to exactly this UID so we never remove another
    // session's \Deleted messages (see expunge_uids).
    expunge_uids(client, &uid_str).await?;

    debug!("deleted UID {uid} from {folder}");
    Ok(())
}

/// Set a flag on a message by UID.
pub async fn set_flag(
    client: &mut ImapClient,
    folder: &str,
    uid: u32,
    flag: &str,
) -> Result<(), ImapError> {
    validate_imap_input(folder)?;

    client
        .session
        .select(folder)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {folder}: {e}")))?;

    let imap_flag = map_flag_name(flag);
    validate_imap_input(&imap_flag)?;
    let store_query = format!("+FLAGS ({imap_flag})");

    let store_stream = client
        .session
        .uid_store(&uid.to_string(), &store_query)
        .await
        .map_err(|e| ImapError::Protocol(format!("UID STORE {store_query} {uid}: {e}")))?;

    let mut stream = store_stream;
    while let Some(_item) = stream.next().await {}

    debug!("set flag {imap_flag} on UID {uid} in {folder}");
    Ok(())
}

/// Create a new mailbox (folder) on the IMAP server.
///
/// Idempotent: if the mailbox already exists, the server returns an error
/// which is logged and converted into success (the caller doesn't care
/// whether the folder was created just now or previously). Used by
/// `snooze` to ensure the `Snoozed` folder exists before moving messages.
pub async fn create_folder(client: &mut ImapClient, folder: &str) -> Result<(), ImapError> {
    validate_imap_input(folder)?;
    match client.session.create(folder).await {
        Ok(()) => {
            debug!("created folder: {folder}");
            Ok(())
        }
        Err(e) => {
            // Already exists is fine — log and continue.
            let msg = e.to_string();
            if msg.contains("already exists") || msg.contains("ALREADYEXISTS") {
                debug!("folder {folder} already exists");
                Ok(())
            } else {
                Err(ImapError::Protocol(format!("CREATE {folder}: {e}")))
            }
        }
    }
}

/// Mark a message as seen (read) by setting the `\Seen` flag.
///
/// Since [`fetch_message`] uses `BODY.PEEK[]` to avoid auto-marking messages
/// as read, callers must invoke this explicitly when the user indicates they
/// want the message flagged as seen (e.g., dashboard "Mark as read" button).
pub async fn mark_seen(client: &mut ImapClient, folder: &str, uid: u32) -> Result<(), ImapError> {
    set_flag(client, folder, uid, "seen").await
}

/// Remove a flag from a message by UID.
pub async fn remove_flag(
    client: &mut ImapClient,
    folder: &str,
    uid: u32,
    flag: &str,
) -> Result<(), ImapError> {
    validate_imap_input(folder)?;

    client
        .session
        .select(folder)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {folder}: {e}")))?;

    let imap_flag = map_flag_name(flag);
    validate_imap_input(&imap_flag)?;
    let store_query = format!("-FLAGS ({imap_flag})");

    let store_stream = client
        .session
        .uid_store(&uid.to_string(), &store_query)
        .await
        .map_err(|e| ImapError::Protocol(format!("UID STORE {store_query} {uid}: {e}")))?;

    let mut stream = store_stream;
    while let Some(_item) = stream.next().await {}

    debug!("removed flag {imap_flag} from UID {uid} in {folder}");
    Ok(())
}

/// Fetch a specific attachment by filename from a message, returning (filename, raw bytes).
pub async fn download_attachment(
    client: &mut ImapClient,
    uid: u32,
    filename: &str,
    folder: &str,
) -> Result<(String, Vec<u8>), ImapError> {
    validate_imap_input(folder)?;

    client
        .session
        .select(folder)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {folder}: {e}")))?;

    let uid_range = format!("{uid}");
    let messages = client
        .session
        .uid_fetch(&uid_range, "(UID BODY.PEEK[])")
        .await
        .map_err(|e| ImapError::Protocol(format!("UID FETCH {uid}: {e}")))?;

    let mut stream = messages;
    let Some(item) = stream.next().await else {
        return Err(ImapError::NotFound(uid));
    };
    let fetch = item.map_err(|e| ImapError::Protocol(format!("UID FETCH parse error: {e}")))?;
    let body: &[u8] = fetch.body().unwrap_or_default();
    let parsed = mail_parser::MessageParser::default()
        .parse(body)
        .ok_or_else(|| ImapError::Protocol(format!("failed to parse message UID {uid}")))?;

    for attachment in parsed.attachments() {
        let att_name = attachment
            .attachment_name()
            .unwrap_or("unnamed")
            .to_string();
        if att_name == filename {
            return Ok((att_name, attachment.contents().to_vec()));
        }
    }
    Err(ImapError::Protocol(format!(
        "attachment '{filename}' not found in UID {uid}"
    )))
}

/// Extract first email address from a mail-parser Address.
/// Extract every address from a mail-parser address header as a list.
///
/// Unlike [`mp_first_address`], this preserves the full recipient set so
/// agent-facing output can expose all `To`/`Cc` recipients rather than only
/// the first one.
fn mp_all_addresses(header: Option<&mail_parser::Address<'_>>) -> Vec<String> {
    let mut out = Vec::new();
    match header {
        Some(mail_parser::Address::List(list)) => {
            for a in list.iter() {
                if let Some(addr) = a.address.as_ref() {
                    out.push(addr.to_string());
                }
            }
        }
        Some(mail_parser::Address::Group(groups)) => {
            for g in groups.iter() {
                for a in g.addresses.iter() {
                    if let Some(addr) = a.address.as_ref() {
                        out.push(addr.to_string());
                    }
                }
            }
        }
        None => {}
    }
    out
}

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

/// Format IMAP envelope addresses into a comma-separated string.
fn imap_envelope_addresses(addrs: &Option<Vec<imap_proto::types::Address<'_>>>) -> String {
    match addrs {
        Some(list) => list
            .iter()
            .map(|a| {
                let mailbox = a
                    .mailbox
                    .as_ref()
                    .map(|m| String::from_utf8_lossy(m).to_string())
                    .unwrap_or_default();
                let host = a
                    .host
                    .as_ref()
                    .map(|h| String::from_utf8_lossy(h).to_string())
                    .unwrap_or_default();
                if host.is_empty() {
                    mailbox
                } else {
                    format!("{mailbox}@{host}")
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_imap_mailbox_arg_quotes_workmail_junk_folder() {
        assert_eq!(imap_mailbox_arg("Junk E-mail"), "\"Junk E-mail\"");
    }

    // ── select_unique_exact_message_id (identity verification) ──────────
    // IMAP `SEARCH HEADER` is substring-based, so its hits are only
    // candidates; destructive callers need exact, unique verification.

    #[test]
    fn exact_message_id_match_excludes_substring_collisions() {
        // Both candidates contain the wanted id as a substring (both would be
        // SEARCH hits); only UID 7 is an exact match.
        let candidates = vec![
            (7, Some("<queued-1@martin.fm>".to_string())),
            (9, Some("<zzz-queued-1@martin.fm.evil.example>".to_string())),
            (11, Some("<queued-1@martin.fm.suffix>".to_string())),
        ];
        assert_eq!(
            select_unique_exact_message_id(&candidates, "queued-1@martin.fm"),
            Some(7)
        );
    }

    #[test]
    fn ambiguous_exact_matches_return_none() {
        // Duplicate Message-IDs (e.g. an appended copy): identity is
        // ambiguous, so no UID may be returned for destructive use.
        let candidates = vec![
            (7, Some("<dup@martin.fm>".to_string())),
            (9, Some("<dup@martin.fm>".to_string())),
        ];
        assert_eq!(
            select_unique_exact_message_id(&candidates, "dup@martin.fm"),
            None
        );
    }

    #[test]
    fn classify_exact_message_id_match_distinguishes_none_unique_ambiguous() {
        // Unique: exactly one exact match among substring collisions.
        let unique = vec![
            (7, Some("<queued-1@martin.fm>".to_string())),
            (11, Some("<queued-1@martin.fm.suffix>".to_string())),
        ];
        assert_eq!(
            classify_exact_message_id_match(&unique, "queued-1@martin.fm"),
            ExactMessageIdMatch::Unique(7)
        );

        // None: only substring collisions, no exact match — distinct from ambiguity.
        let none = vec![
            (9, Some("<zzz-queued-1@martin.fm.evil>".to_string())),
            (13, None),
        ];
        assert_eq!(
            classify_exact_message_id_match(&none, "queued-1@martin.fm"),
            ExactMessageIdMatch::None
        );
        assert_eq!(
            classify_exact_message_id_match(&[], "queued-1@martin.fm"),
            ExactMessageIdMatch::None
        );

        // Ambiguous: duplicate exact Message-IDs — an explicit, stable status
        // that never collapses to None or fabricates a UID.
        let ambiguous = vec![
            (7, Some("<dup@martin.fm>".to_string())),
            (9, Some(" <DUP-not-equal@martin.fm> ".to_string())),
            (12, Some("<dup@martin.fm>".to_string())),
        ];
        assert_eq!(
            classify_exact_message_id_match(&ambiguous, "dup@martin.fm"),
            ExactMessageIdMatch::Ambiguous
        );

        // Empty / bracket-only wanted normalizes away → None.
        assert_eq!(
            classify_exact_message_id_match(&unique, "   "),
            ExactMessageIdMatch::None
        );
    }

    #[test]
    fn zero_exact_matches_return_none() {
        let candidates = vec![(7, Some("<other@martin.fm>".to_string())), (9, None)];
        assert_eq!(
            select_unique_exact_message_id(&candidates, "queued-1@martin.fm"),
            None
        );
        assert_eq!(
            select_unique_exact_message_id(&[], "queued-1@martin.fm"),
            None
        );
    }

    #[test]
    fn exact_match_normalizes_brackets_and_whitespace_on_both_sides() {
        let candidates = vec![(7, Some("  <a@b.example>  ".to_string()))];
        assert_eq!(
            select_unique_exact_message_id(&candidates, "a@b.example"),
            Some(7)
        );
        assert_eq!(
            select_unique_exact_message_id(&candidates, "<a@b.example>"),
            Some(7)
        );
        // Case differences are NOT folded away — fail closed.
        assert_eq!(
            select_unique_exact_message_id(&candidates, "A@B.EXAMPLE"),
            None
        );
    }

    #[test]
    fn empty_or_bracket_only_wanted_id_matches_nothing() {
        let candidates = vec![(7, Some("<a@b.example>".to_string()))];
        assert_eq!(select_unique_exact_message_id(&candidates, ""), None);
        assert_eq!(select_unique_exact_message_id(&candidates, "<>"), None);
        assert_eq!(select_unique_exact_message_id(&candidates, "   "), None);
    }

    // ── parse_message_id_from_header_section (FETCH section boundary) ───
    // Representative `BODY[HEADER.FIELDS (MESSAGE-ID)]` section payloads:
    // header lines, CRLF-terminated, ending with an empty line.

    #[test]
    fn header_section_parses_message_id_from_representative_fetch_data() {
        let section = b"Message-ID: <queued-1@martin.fm>\r\n\r\n";
        assert_eq!(
            parse_message_id_from_header_section(section).as_deref(),
            Some("queued-1@martin.fm")
        );
        // Case-insensitive header name, as servers commonly emit it.
        let lower = b"Message-Id: <queued-2@martin.fm>\r\n\r\n";
        assert_eq!(
            parse_message_id_from_header_section(lower).as_deref(),
            Some("queued-2@martin.fm")
        );
    }

    #[test]
    fn header_section_parses_folded_message_id_header() {
        // RFC 5322 folding: continuation line starts with whitespace.
        let folded = b"Message-ID:\r\n <folded-3@martin.fm>\r\n\r\n";
        assert_eq!(
            parse_message_id_from_header_section(folded).as_deref(),
            Some("folded-3@martin.fm")
        );
    }

    #[test]
    fn header_section_without_message_id_yields_none() {
        assert_eq!(
            parse_message_id_from_header_section(b"Subject: hi\r\n\r\n"),
            None
        );
        assert_eq!(parse_message_id_from_header_section(b"\r\n"), None);
        assert_eq!(parse_message_id_from_header_section(b""), None);
    }

    #[test]
    fn test_imap_mailbox_arg_escapes_quoted_string_metacharacters() {
        assert_eq!(imap_mailbox_arg(r#"Foo\"Bar"#), r#""Foo\\\"Bar""#);
    }

    #[test]
    fn expunge_strategy_uses_uid_expunge_when_uidplus_present() {
        assert_eq!(
            choose_expunge_strategy(true, "1:5,9"),
            ExpungeStrategy::UidScoped("UID EXPUNGE 1:5,9".to_string())
        );
    }

    #[test]
    fn expunge_strategy_falls_back_to_bare_when_uidplus_absent() {
        assert_eq!(
            choose_expunge_strategy(false, "1:5,9"),
            ExpungeStrategy::BareAll
        );
    }

    #[test]
    fn test_mp_all_addresses_returns_full_recipient_list() {
        let raw = b"From: sender@example.com\r\n\
To: alice@example.com, Bob <bob@example.com>, carol@example.com\r\n\
Cc: dave@example.com, eve@example.com\r\n\
Subject: hi\r\n\r\nbody\r\n";
        let parsed = mail_parser::MessageParser::default()
            .parse(&raw[..])
            .expect("parse");
        assert_eq!(
            mp_all_addresses(parsed.to()),
            vec![
                "alice@example.com".to_string(),
                "bob@example.com".to_string(),
                "carol@example.com".to_string(),
            ]
        );
        assert_eq!(
            mp_all_addresses(parsed.cc()),
            vec![
                "dave@example.com".to_string(),
                "eve@example.com".to_string()
            ]
        );
        // mp_first_address only returns the first — the bug the list fixes.
        assert_eq!(mp_first_address(parsed.to()), "alice@example.com");
    }

    #[test]
    fn test_normalize_search_wraps_bare_term_as_text() {
        assert_eq!(normalize_search_query("Hillan"), r#"TEXT "Hillan""#);
        assert_eq!(
            normalize_search_query("SL unipersonal"),
            r#"TEXT "SL unipersonal""#
        );
        assert_eq!(
            normalize_search_query("régimen matrimonial"),
            r#"TEXT "régimen matrimonial""#
        );
    }

    #[test]
    fn test_normalize_search_passes_through_field_qualified() {
        assert_eq!(
            normalize_search_query("FROM bob@example.com"),
            "FROM bob@example.com"
        );
        assert_eq!(normalize_search_query("TEXT Hillan"), "TEXT Hillan");
        assert_eq!(normalize_search_query("SUBJECT SL"), "SUBJECT SL");
        // Case-insensitive key detection.
        assert_eq!(
            normalize_search_query("from bob@example.com"),
            "from bob@example.com"
        );
        // Grouped queries pass through.
        assert_eq!(
            normalize_search_query("(OR FROM a SUBJECT b)"),
            "(OR FROM a SUBJECT b)"
        );
    }

    #[test]
    fn test_normalize_search_escapes_quotes_in_bare_term() {
        assert_eq!(
            normalize_search_query(r#"say "hi""#),
            r#"TEXT "say \"hi\"""#
        );
    }

    #[test]
    fn test_message_id_search_query_quotes_plain_message_id() {
        assert_eq!(
            message_id_search_query("<abc@example.com>").unwrap(),
            r#"HEADER Message-ID "<abc@example.com>""#
        );
    }

    #[test]
    fn test_message_id_search_query_escapes_untrusted_syntax() {
        assert_eq!(
            message_id_search_query(r#"<a" OR ALL \ "b@example.com>"#).unwrap(),
            r#"HEADER Message-ID "<a\" OR ALL \\ \"b@example.com>""#
        );
    }

    #[test]
    fn test_message_id_search_query_rejects_crlf() {
        assert!(message_id_search_query("<a@example.com>\r\nALL").is_err());
    }

    #[test]
    fn test_missing_body_protocol_error_includes_uid_and_folder() {
        let err = missing_body_protocol_error("Junk E-mail", "1:25", Some(42));
        let ImapError::Protocol(msg) = err else {
            panic!("expected Protocol variant");
        };
        assert!(
            msg.contains("Junk E-mail"),
            "expected folder in message: {msg}"
        );
        assert!(msg.contains("1:25"), "expected uid set in message: {msg}");
        assert!(msg.contains("UID 42"), "expected UID in message: {msg}");
        assert!(
            msg.contains("BODY.PEEK"),
            "expected reason in message: {msg}"
        );
    }

    #[test]
    fn test_missing_body_protocol_error_handles_unknown_uid() {
        let err = missing_body_protocol_error("INBOX", "1:25", None);
        let ImapError::Protocol(msg) = err else {
            panic!("expected Protocol variant");
        };
        assert!(
            msg.contains("unknown UID"),
            "expected unknown-uid placeholder: {msg}"
        );
    }

    #[test]
    fn test_validate_uid_set_accepts_generated_sequence_sets_only() {
        assert!(validate_uid_set("1:25,30,*").is_ok());
        assert!(validate_uid_set("1 UID SEARCH ALL").is_err());
        assert!(validate_uid_set("").is_err());
    }

    /// Regression guard: reading a message must NEVER auto-set the \Seen flag.
    ///
    /// The dashboard "read message" action calls `fetch_message` for every
    /// message the user opens. If this descriptor were silently changed from
    /// `BODY.PEEK[]` to `BODY[]`, every message the user clicked would be
    /// marked as read on the server — surprising and destructive behavior.
    ///
    /// If this test fails, you are either (a) fixing something legitimate
    /// (in which case update the test) or (b) about to ship a regression.
    #[test]
    fn test_fetch_uses_body_peek() {
        assert_eq!(
            FETCH_MESSAGE_DESCRIPTOR, "(UID FLAGS BODY.PEEK[])",
            "fetch_message must use BODY.PEEK[] to avoid auto-setting \\Seen"
        );
        assert!(
            FETCH_MESSAGE_DESCRIPTOR.contains("BODY.PEEK"),
            "fetch descriptor must contain BODY.PEEK"
        );
        assert!(
            !FETCH_MESSAGE_DESCRIPTOR.contains("BODY[")
                || FETCH_MESSAGE_DESCRIPTOR.contains("BODY.PEEK["),
            "fetch descriptor must not contain BODY[ without .PEEK"
        );
    }

    #[test]
    fn quickstart_peek_descriptor_uses_body_peek_headers_only() {
        assert_eq!(
            QUICKSTART_PEEK_FETCH_DESCRIPTOR,
            "(UID BODY.PEEK[HEADER.FIELDS (FROM SUBJECT DATE)])"
        );
        assert!(QUICKSTART_PEEK_FETCH_DESCRIPTOR.contains("BODY.PEEK["));
        assert!(!QUICKSTART_PEEK_FETCH_DESCRIPTOR.contains("BODY[]"));
        assert!(!QUICKSTART_PEEK_FETCH_DESCRIPTOR.contains("BODY["));
    }

    #[test]
    fn summary_fetch_descriptor_is_header_only_and_read_only() {
        assert_eq!(
            FETCH_SUMMARY_DESCRIPTOR,
            "(UID FLAGS ENVELOPE RFC822.SIZE BODY.PEEK[HEADER.FIELDS (X-MIGADU-SPAM-SCORE X-SPAM-SCORE)])"
        );
        // Header fields only, fetched via PEEK — never a full/partial body, so
        // the summary FETCH stays read-only (no `\Seen`) and body-free.
        assert!(FETCH_SUMMARY_DESCRIPTOR.contains("BODY.PEEK[HEADER.FIELDS ("));
        assert!(!FETCH_SUMMARY_DESCRIPTOR.contains("BODY[]"));
        assert!(!FETCH_SUMMARY_DESCRIPTOR.contains("BODY.PEEK[]"));
        assert!(FETCH_SUMMARY_DESCRIPTOR.contains("X-MIGADU-SPAM-SCORE"));
        assert!(FETCH_SUMMARY_DESCRIPTOR.contains("X-SPAM-SCORE"));
    }

    #[test]
    fn provider_spam_from_migadu_header_section() {
        let section = b"X-Migadu-Spam-Score: 3.5\r\nX-Spam-Score: 9.9\r\n\r\n";
        assert_eq!(provider_spam_from_header_bytes(section), Some(3.5));
    }

    #[test]
    fn provider_spam_falls_back_to_generic_header_section() {
        let section = b"Subject: hi\r\nX-Spam-Score: 5.1\r\n\r\n";
        assert_eq!(provider_spam_from_header_bytes(section), Some(5.1));
    }

    #[test]
    fn provider_spam_header_section_ignores_malformed() {
        let section = b"X-Migadu-Spam-Score: not-a-number\r\n\r\n";
        assert_eq!(provider_spam_from_header_bytes(section), None);
    }

    #[test]
    fn header_value_from_bytes_is_case_insensitive_and_unfolds() {
        let section = b"x-migadu-spam-score: 2.0\r\nSubject: a\r\n  folded\r\n\r\n";
        assert_eq!(
            header_value_from_bytes(section, "X-Migadu-Spam-Score"),
            Some("2.0".to_string())
        );
        assert_eq!(
            header_value_from_bytes(section, "Subject"),
            Some("a folded".to_string())
        );
    }

    #[test]
    fn evidence_mailbox_access_is_read_only_examine() {
        assert_eq!(EVIDENCE_MAILBOX_OPEN_COMMAND, "EXAMINE");
    }

    #[test]
    fn evidence_raw_fetch_descriptor_uses_body_peek() {
        assert_eq!(
            EVIDENCE_RAW_FETCH_DESCRIPTOR, "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[])",
            "evidence raw capture must use BODY.PEEK[] to preserve unread state"
        );
        assert!(EVIDENCE_RAW_FETCH_DESCRIPTOR.contains("BODY.PEEK[]"));
        assert!(!EVIDENCE_RAW_FETCH_DESCRIPTOR.contains("BODY[]"));
    }

    #[test]
    fn evidence_header_search_query_allows_only_thread_headers_and_escapes_values() {
        assert_eq!(
            evidence_header_search_query("References", r#"<a" OR ALL \ "b@example.com>"#).unwrap(),
            r#"HEADER References "<a\" OR ALL \\ \"b@example.com>""#
        );
        assert_eq!(
            evidence_header_search_query("In-Reply-To", "<parent@example.com>").unwrap(),
            r#"HEADER In-Reply-To "<parent@example.com>""#
        );
    }

    #[test]
    fn evidence_header_search_query_rejects_subject_fallback_and_crlf() {
        assert!(evidence_header_search_query("Subject", "Contract").is_err());
        assert!(evidence_header_search_query("Message-ID", "<a@example.com>\r\nALL").is_err());
    }

    #[test]
    fn test_map_flag_name_seen() {
        assert_eq!(map_flag_name("seen"), "\\Seen");
        assert_eq!(map_flag_name("SEEN"), "\\Seen");
        assert_eq!(map_flag_name("flagged"), "\\Flagged");
    }

    #[test]
    fn test_decode_rfc2047_plain_text() {
        assert_eq!(decode_rfc2047(b"Hello World"), "Hello World");
    }

    #[test]
    fn test_decode_rfc2047_q_encoding_utf8() {
        let input = b"=?utf-8?q?Ticket_Received_-_Palvelupyynt=C3=B6?=";
        let result = decode_rfc2047(input);
        assert_eq!(result, "Ticket Received - Palvelupyynt\u{00f6}");
    }

    #[test]
    fn test_decode_rfc2047_b_encoding_utf8() {
        let input = b"=?utf-8?b?SGVsbG8gV29ybGQ=?=";
        let result = decode_rfc2047(input);
        assert_eq!(result, "Hello World");
    }

    #[test]
    fn test_decode_rfc2047_mixed_plain_and_encoded() {
        let input = b"Re: =?utf-8?q?Ihre_Anfrage?= ist eingegangen!";
        let result = decode_rfc2047(input);
        assert_eq!(result, "Re: Ihre Anfrage ist eingegangen!");
    }

    #[test]
    fn test_decode_rfc2047_multiple_encoded_words() {
        let input = b"=?utf-8?q?Hello?= =?utf-8?q?_World?=";
        let result = decode_rfc2047(input);
        assert!(result.contains("Hello"));
        assert!(result.contains("World"));
    }

    /// Regression: Q-encoded text that starts with `=XX` (multibyte UTF-8
    /// sequences, accented chars, emoji bytes) used to be truncated because the
    /// naive `find("?=")` in `remaining[2..]` would fire on the `?=` formed by
    /// the `?` charset/encoding separator followed by the leading `=` of `=XX`.
    /// e.g. `=?UTF-8?Q?=C2=A1Ey!?=` → `¡Ey!`
    #[test]
    fn test_decode_rfc2047_q_starts_with_hex_escape() {
        // ¡ = U+00A1 = bytes C2 A1 in UTF-8; É = U+00C9 = bytes C3 89
        let input = b"=?UTF-8?Q?=C2=A1Ey!_=C3=89chal?=";
        let result = decode_rfc2047(input);
        assert_eq!(result, "\u{00A1}Ey! \u{00C9}chal");
    }

    /// Regression: base64-encoded subject should decode fully including when
    /// the base64 payload happens to pad with `=`.
    #[test]
    fn test_decode_rfc2047_b_encoding_with_padding() {
        use base64::Engine as _;
        // base64("📧 Inbox") — 📧 = F0 9F 93 A7
        let b64 = base64::engine::general_purpose::STANDARD.encode("📧 Inbox");
        let input = format!("=?UTF-8?B?{b64}?=");
        let result = decode_rfc2047(input.as_bytes());
        assert_eq!(result, "📧 Inbox");
    }

    /// Regression: uppercase charset/encoding identifiers must be accepted.
    #[test]
    fn test_decode_rfc2047_uppercase_encoding_label() {
        let input = b"=?UTF-8?Q?caf=C3=A9?=";
        let result = decode_rfc2047(input);
        assert_eq!(result, "caf\u{00e9}");
    }

    #[test]
    fn test_decode_q_encoding_underscore_to_space() {
        let decoded = decode_q_encoding("Hello_World");
        assert_eq!(decoded, b"Hello World");
    }

    #[test]
    fn test_decode_q_encoding_hex_escape() {
        let decoded = decode_q_encoding("caf=C3=A9");
        assert_eq!(String::from_utf8_lossy(&decoded), "caf\u{00e9}");
    }

    /// `read_imap_greeting` must consume the `* OK ...` line so that a
    /// subsequent `LOGIN` is not framed alongside greeting bytes still
    /// sitting in the buffer (the bug that took down mail.inbox.eu).
    #[tokio::test]
    async fn read_imap_greeting_drains_ok_line() {
        use tokio::io::AsyncWriteExt;

        let (client_io, mut server_io) = tokio::io::duplex(4096);

        let server = tokio::spawn(async move {
            server_io
                .write_all(b"* OK [CAPABILITY IMAP4rev1] greeting\r\n")
                .await
                .unwrap();
            // Hold the stream open so the client's read_response sees a full line.
            server_io
        });

        let mut client = async_imap::Client::new(client_io);
        let result = read_imap_greeting(&mut client, "test.example").await;
        assert!(result.is_ok(), "greeting drain failed: {result:?}");

        let _server_io = server.await.unwrap();
    }

    /// If the server closes immediately without sending a greeting, surface a
    /// clear `Connection` error rather than masquerading as auth failure.
    #[tokio::test]
    async fn read_imap_greeting_reports_connection_error_on_eof() {
        let (client_io, server_io) = tokio::io::duplex(4096);
        // Drop the server side without writing anything → EOF.
        drop(server_io);

        let mut client = async_imap::Client::new(client_io);
        let err = read_imap_greeting(&mut client, "test.example")
            .await
            .expect_err("expected connection error on EOF greeting");
        match err {
            ImapError::Connection(msg) => {
                assert!(
                    msg.contains("test.example"),
                    "error should include host context: {msg}"
                );
            }
            other => panic!("expected Connection error, got: {other:?}"),
        }
    }
}
