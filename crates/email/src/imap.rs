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
fn validate_imap_input(s: &str) -> Result<(), ImapError> {
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
fn imap_mailbox_arg(mailbox: &str) -> String {
    format!("\"{}\"", mailbox.replace('\\', r"\\").replace('"', "\\\""))
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
}

impl ImapClient {
    pub fn session_mut(&mut self) -> &mut ImapSession {
        &mut self.session
    }
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
    Ok(ImapClient { session })
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
        .fetch(&range, "(UID FLAGS ENVELOPE RFC822.SIZE)")
        .await
        .map_err(|e| ImapError::Protocol(format!("FETCH {range}: {e}")))?;

    let mut summaries = Vec::new();
    let mut stream = messages;
    while let Some(item) = stream.next().await {
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
                });
            }
            Err(e) => return Err(ImapError::Protocol(format!("FETCH parse error: {e}"))),
        }
    }

    Ok(summaries)
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

        // Find the end of the encoded word: =?charset?encoding?text?=
        if let Some(end) = remaining[2..].find("?=") {
            let encoded_word = &remaining[2..end + 2]; // charset?encoding?text
            remaining = &remaining[end + 4..]; // skip past ?=

            // Strip whitespace between consecutive encoded words (RFC 2047 §6.2)
            if remaining.starts_with(' ') || remaining.starts_with('\t') {
                if remaining.trim_start().starts_with("=?") {
                    remaining = &remaining[remaining.find("=?").unwrap_or(0)..];
                }
            }

            // Parse: charset?encoding?text
            let parts: Vec<&str> = encoded_word.splitn(3, '?').collect();
            if parts.len() == 3 {
                let _charset = parts[0]; // TODO: proper charset conversion for non-UTF-8
                let encoding = parts[1].to_uppercase();
                let text = parts[2];

                let decoded_bytes = match encoding.as_str() {
                    "Q" => decode_q_encoding(text),
                    "B" => {
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD
                            .decode(text)
                            .unwrap_or_else(|_| text.as_bytes().to_vec())
                    }
                    _ => text.as_bytes().to_vec(),
                };

                result.push_str(&String::from_utf8_lossy(&decoded_bytes));
            } else {
                // Malformed — emit as-is
                result.push_str("=?");
                result.push_str(encoded_word);
                result.push_str("?=");
            }
        } else {
            // No closing ?= — emit remainder as-is
            result.push_str(remaining);
            remaining = "";
        }
    }

    result.push_str(remaining);
    result
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
    let to_addr = mp_first_address(parsed.to());
    let cc_addr = {
        let addr = mp_first_address(parsed.cc());
        if addr.is_empty() { None } else { Some(addr) }
    };

    let subject = parsed.subject().unwrap_or_default().to_string();
    let date = parsed.date().map(|d| d.to_rfc3339());
    let text_body = parsed.body_text(0).map(|t| t.to_string());
    let html_body = parsed.body_html(0).map(|h| h.to_string());
    let in_reply_to = parsed.in_reply_to().as_text().map(|s| s.to_string());
    let references = parsed.references().as_text().map(|s| s.to_string());
    let message_id = parsed.message_id().map(|s| s.to_string());

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
        subject,
        date,
        text_body,
        html_body,
        in_reply_to,
        references,
        flags,
        attachments,
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
        let message_id = fetch.body().and_then(|body| {
            mail_parser::MessageParser::default()
                .parse(body)
                .and_then(|m| m.message_id().map(|s| s.to_string()))
        });
        out.push(MessageHeader {
            uid,
            message_id,
            size: fetch.size,
        });
    }
    Ok(out)
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
fn map_flag_name(flag: &str) -> String {
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

    client
        .session
        .select(folder)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {folder}: {e}")))?;

    let uid_set = client
        .session
        .uid_search(query)
        .await
        .map_err(|e| ImapError::Protocol(format!("UID SEARCH {query}: {e}")))?;

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

    {
        let expunge_stream = client
            .session
            .expunge()
            .await
            .map_err(|e| ImapError::Protocol(format!("EXPUNGE: {e}")))?;

        // Consume the expunge stream (needs pinning)
        let mut stream = pin!(expunge_stream);
        while let Some(_item) = stream.next().await {}
    }

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

    {
        let expunge_stream = client
            .session
            .expunge()
            .await
            .map_err(|e| ImapError::Protocol(format!("EXPUNGE: {e}")))?;

        let mut stream = pin!(expunge_stream);
        while let Some(_item) = stream.next().await {}
    }

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

    #[test]
    fn test_imap_mailbox_arg_escapes_quoted_string_metacharacters() {
        assert_eq!(imap_mailbox_arg(r#"Foo\"Bar"#), r#""Foo\\\"Bar""#);
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
