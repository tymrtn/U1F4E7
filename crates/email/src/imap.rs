// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::pin::pin;
use std::sync::Arc;

use async_imap::Session;
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

type ImapSession = Session<TlsStream<TcpStream>>;

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

    let client = async_imap::Client::new(tls_stream);

    let session = client
        .login(username, password)
        .await
        .map_err(|(e, _)| ImapError::Auth(format!("login failed for {username}@{host}: {e}")))?;

    debug!("IMAP session established for {username}@{host}");
    Ok(ImapClient { session })
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
pub async fn folder_stats(
    client: &mut ImapClient,
    folder: &str,
) -> Result<FolderStats, ImapError> {
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
pub async fn list_folder_stats(
    client: &mut ImapClient,
) -> Result<Vec<FolderStats>, ImapError> {
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
                            .map(|s| String::from_utf8_lossy(s).to_string())
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
    validate_imap_input(folder)?;

    client
        .session
        .append(folder, Some(flags), None, rfc822)
        .await
        .map_err(|e| ImapError::Protocol(format!("APPEND to {folder}: {e}")))?;

    debug!("appended message to {folder} ({} bytes)", rfc822.len());
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
    validate_imap_input(message_id)?;

    client
        .session
        .select(folder)
        .await
        .map_err(|e| ImapError::Protocol(format!("SELECT {folder}: {e}")))?;

    let search_query = format!("HEADER Message-ID {message_id}");
    let uid_set = client
        .session
        .uid_search(&search_query)
        .await
        .map_err(|e| ImapError::Protocol(format!("UID SEARCH {search_query}: {e}")))?;

    let uid = uid_set.into_iter().next();
    Ok(uid)
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
                            .map(|s| String::from_utf8_lossy(s).to_string())
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

    client
        .session
        .uid_copy(&uid_str, to)
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

    client
        .session
        .uid_copy(&uid.to_string(), to)
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
pub async fn create_folder(
    client: &mut ImapClient,
    folder: &str,
) -> Result<(), ImapError> {
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
pub async fn mark_seen(
    client: &mut ImapClient,
    folder: &str,
    uid: u32,
) -> Result<(), ImapError> {
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
        let att_name = attachment.attachment_name().unwrap_or("unnamed").to_string();
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
            !FETCH_MESSAGE_DESCRIPTOR.contains("BODY[") || FETCH_MESSAGE_DESCRIPTOR.contains("BODY.PEEK["),
            "fetch descriptor must not contain BODY[ without .PEEK"
        );
    }

    #[test]
    fn test_map_flag_name_seen() {
        assert_eq!(map_flag_name("seen"), "\\Seen");
        assert_eq!(map_flag_name("SEEN"), "\\Seen");
        assert_eq!(map_flag_name("flagged"), "\\Flagged");
    }
}
