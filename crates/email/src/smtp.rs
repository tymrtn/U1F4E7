// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use envelope_email_store::models::AccountWithCredentials;
use lettre::message::header::{self, ContentType};
use lettre::message::{Attachment as LettreAttachment, Mailboxes, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use tracing::info;
use uuid::Uuid;

use crate::errors::SmtpError;

/// A file attachment to include in a sent message.
///
/// Content is passed in-memory. The dashboard base64-encodes files over
/// the wire and decodes into this struct before calling `SmtpSender::send`.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub filename: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

/// SMTP sender — stateless, builds a transport per send.
pub struct SmtpSender;

impl SmtpSender {
    /// Send an email through the account's SMTP server — simple path.
    ///
    /// Calls into [`SmtpSender::send`] with no `in_reply_to`, `references`,
    /// or attachments. Preserved for all existing CLI callsites that don't
    /// need the extended options.
    ///
    /// Returns the generated Message-ID on success.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_simple(
        account: &AccountWithCredentials,
        to: &str,
        subject: &str,
        text: Option<&str>,
        html: Option<&str>,
        cc: Option<&str>,
        bcc: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<String, SmtpError> {
        Self::send(
            account,
            to,
            subject,
            text,
            html,
            None,
            cc,
            bcc,
            reply_to,
            None,
            None,
            &[],
        )
        .await
    }

    /// Send an email through the account's SMTP server — full options.
    ///
    /// Supports all the simple-path options plus:
    /// - `in_reply_to`: sets the `In-Reply-To` header (for threaded replies).
    /// - `references`: sets the `References` header (list of prior Message-IDs
    ///   in the thread chain).
    /// - `attachments`: in-memory attachments appended as a multipart/mixed
    ///   envelope wrapping the text/html body.
    ///
    /// Callers building replies should use
    /// [`crate::reply::build_reply_headers`] to derive `in_reply_to` and
    /// `references` from the parent message.
    ///
    /// Returns the generated Message-ID on success.
    #[allow(clippy::too_many_arguments)]
    pub async fn send(
        account: &AccountWithCredentials,
        to: &str,
        subject: &str,
        text: Option<&str>,
        html: Option<&str>,
        from_override: Option<&str>,
        cc: Option<&str>,
        bcc: Option<&str>,
        reply_to: Option<&str>,
        in_reply_to: Option<&str>,
        references: Option<&[String]>,
        attachments: &[Attachment],
    ) -> Result<String, SmtpError> {
        // Generate a stable Message-ID up front and set it explicitly on the
        // message. We do NOT rely on lettre auto-generating one: in practice
        // `get_raw("Message-ID")` could come back empty, which left agents with
        // no durable proof of the send and no way to look the message up in the
        // Sent folder. A self-generated ID is guaranteed non-empty and matches
        // exactly what we transmit.
        let message_id = generate_message_id(account);

        let (email, message_id) = build_message(
            account,
            &message_id,
            to,
            subject,
            text,
            html,
            from_override,
            cc,
            bcc,
            reply_to,
            in_reply_to,
            references,
            attachments,
        )?;

        // Build SMTP transport
        let smtp_host = &account.account.smtp_host;
        let smtp_port = account.account.smtp_port;
        let username = account.effective_smtp_username().to_string();
        let password = account.effective_smtp_password().to_string();

        let creds = Credentials::new(username, password);

        let transport = match smtp_port {
            465 => {
                // Implicit TLS (SMTPS)
                AsyncSmtpTransport::<Tokio1Executor>::relay(smtp_host)
                    .map_err(|e| SmtpError::Connection(format!("{smtp_host}:{smtp_port}: {e}")))?
                    .port(smtp_port)
                    .credentials(creds)
                    .build()
            }
            _ => {
                // STARTTLS (typically port 587)
                AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(smtp_host)
                    .map_err(|e| SmtpError::Connection(format!("{smtp_host}:{smtp_port}: {e}")))?
                    .port(smtp_port)
                    .credentials(creds)
                    .build()
            }
        };

        info!(
            "sending email via {smtp_host}:{smtp_port} to {to} ({} attachment{})",
            attachments.len(),
            if attachments.len() == 1 { "" } else { "s" }
        );

        transport.send(email).await.map_err(|e| {
            let msg = e.to_string();
            if msg.contains("authentication") || msg.contains("AUTH") {
                SmtpError::Auth(msg)
            } else if msg.contains("rejected") || msg.contains("Recipient") {
                SmtpError::RecipientRejected(msg)
            } else {
                SmtpError::Send(msg)
            }
        })?;

        info!("email sent, message-id: {message_id}");
        Ok(message_id)
    }
}

/// Generate a stable, RFC-shaped Message-ID for an outgoing message.
///
/// Returns the bare id (no angle brackets), e.g. `uuid@domain`. The domain is
/// derived from the account's own address so the ID looks legitimate to
/// receiving servers; a missing/garbage local part falls back to a safe
/// sentinel domain. Callers pass the bare id to [`build_message`], which wraps
/// it in angle brackets when serializing the header.
pub fn generate_message_id(account: &AccountWithCredentials) -> String {
    let domain = account
        .account
        .username
        .rsplit('@')
        .next()
        .map(str::trim)
        .filter(|d| !d.is_empty() && d.contains('.') && !d.contains(' '))
        .unwrap_or("envelope.local");
    format!("{}@{}", Uuid::new_v4(), domain)
}

/// Build the lettre [`Message`] for a send, setting an explicit Message-ID.
///
/// Factored out of [`SmtpSender::send`] so message construction (headers,
/// threading, body, attachments, and the guaranteed-present Message-ID) can be
/// unit-tested without opening a socket. `message_id_bare` is the angle-bracket
/// -free id; the returned `String` is the header value as written to the wire
/// (with angle brackets), which the caller returns to agents as durable proof.
#[allow(clippy::too_many_arguments)]
pub fn build_message(
    account: &AccountWithCredentials,
    message_id_bare: &str,
    to: &str,
    subject: &str,
    text: Option<&str>,
    html: Option<&str>,
    from_override: Option<&str>,
    cc: Option<&str>,
    bcc: Option<&str>,
    reply_to: Option<&str>,
    in_reply_to: Option<&str>,
    references: Option<&[String]>,
    attachments: &[Attachment],
) -> Result<(Message, String), SmtpError> {
    let from_addr = if let Some(f) = from_override {
        f.to_string()
    } else if let Some(ref display) = account.account.display_name {
        format!("{display} <{}>", account.account.username)
    } else {
        account.account.username.clone()
    };

    // Build the message headers.
    //
    // To/Cc/Bcc accept RFC5322 comma-separated mailbox lists (with optional
    // display names) by parsing into `Mailboxes`. Reply-To stays a single
    // mailbox to match the existing CLI/agent contract.
    let mut builder = Message::builder()
        .from(
            from_addr
                .parse()
                .map_err(|e| SmtpError::Send(format!("invalid from address: {e}")))?,
        )
        .mailbox(header::To::from(parse_mailboxes(to, "to")?))
        .subject(subject)
        // Set the Message-ID explicitly so it is always present and matches the
        // value we hand back to the caller. lettre wraps the bare id in angle
        // brackets when it serializes the header.
        .message_id(Some(strip_brackets(message_id_bare)));

    if let Some(cc_addr) = cc {
        if !cc_addr.trim().is_empty() {
            builder = builder.mailbox(header::Cc::from(parse_mailboxes(cc_addr, "cc")?));
        }
    }

    if let Some(bcc_addr) = bcc {
        if !bcc_addr.trim().is_empty() {
            builder = builder.mailbox(header::Bcc::from(parse_mailboxes(bcc_addr, "bcc")?));
        }
    }

    if let Some(reply) = reply_to {
        builder = builder.reply_to(
            reply
                .parse()
                .map_err(|e| SmtpError::Send(format!("invalid reply-to address: {e}")))?,
        );
    }

    if let Some(irt) = in_reply_to {
        builder = builder.in_reply_to(irt.to_string());
    }

    if let Some(refs) = references {
        // lettre's References header accepts a Vec<String> via serialization.
        // Concatenate with spaces and wrap each in angle brackets if not already.
        let joined = refs
            .iter()
            .map(|r| {
                if r.starts_with('<') {
                    r.clone()
                } else {
                    format!("<{r}>")
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        builder = builder.references(joined);
    }

    // Build the body (text/html alternative) — shared across attachment path
    let body_part = build_body_part(text, html)?;

    // If we have attachments, wrap everything in multipart/mixed.
    // Otherwise, use the body part directly.
    let email = if attachments.is_empty() {
        match body_part {
            BodyPart::Single(single) => builder
                .singlepart(single)
                .map_err(|e| SmtpError::Send(format!("failed to build message: {e}")))?,
            BodyPart::Multi(multi) => builder
                .multipart(multi)
                .map_err(|e| SmtpError::Send(format!("failed to build message: {e}")))?,
        }
    } else {
        let mut mixed = match body_part {
            BodyPart::Single(single) => MultiPart::mixed().singlepart(single),
            BodyPart::Multi(multi) => MultiPart::mixed().multipart(multi),
        };
        for att in attachments {
            let ct = att
                .content_type
                .parse::<ContentType>()
                .unwrap_or(ContentType::parse("application/octet-stream").unwrap());
            let attachment = LettreAttachment::new(att.filename.clone()).body(att.data.clone(), ct);
            // LettreAttachment::new().body() returns a SinglePart ready to append
            mixed = mixed.singlepart(attachment);
        }
        builder
            .multipart(mixed)
            .map_err(|e| SmtpError::Send(format!("failed to build multipart message: {e}")))?
    };

    // Read the Message-ID back from the built headers so the returned value is
    // exactly what goes on the wire. Fall back to the angle-bracketed generated
    // id if lettre ever fails to surface the header — it must never be empty.
    let header_id = email
        .headers()
        .get_raw("Message-ID")
        .map(|v| v.to_string())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| format!("<{}>", strip_brackets(message_id_bare)));

    Ok((email, header_id))
}

/// Strip surrounding angle brackets from a Message-ID, leaving the bare id.
fn strip_brackets(id: &str) -> String {
    id.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_string()
}

/// Parse a recipient header value into a list of mailboxes.
///
/// Accepts a single address or an RFC5322 comma-separated list, with or without
/// display names (e.g. `a@x.com, "Bob Q" <b@y.com>`). `field` names the header
/// for error messages.
fn parse_mailboxes(value: &str, field: &str) -> Result<Mailboxes, SmtpError> {
    value
        .parse::<Mailboxes>()
        .map_err(|e| SmtpError::Send(format!("invalid {field} address: {e}")))
}

enum BodyPart {
    Single(SinglePart),
    Multi(MultiPart),
}

/// Construct the message body (text/html/alternative/empty) as either a
/// [`SinglePart`] or a [`MultiPart::alternative`] depending on which body
/// formats are provided. The caller decides whether to wrap it in
/// `multipart/mixed` for attachments.
fn build_body_part(text: Option<&str>, html: Option<&str>) -> Result<BodyPart, SmtpError> {
    match (text, html) {
        (Some(t), Some(h)) => Ok(BodyPart::Multi(
            MultiPart::alternative()
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_PLAIN)
                        .body(t.to_string()),
                )
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_HTML)
                        .body(h.to_string()),
                ),
        )),
        (Some(t), None) => Ok(BodyPart::Single(
            SinglePart::builder()
                .header(ContentType::TEXT_PLAIN)
                .body(t.to_string()),
        )),
        (None, Some(h)) => Ok(BodyPart::Single(
            SinglePart::builder()
                .header(ContentType::TEXT_HTML)
                .body(h.to_string()),
        )),
        (None, None) => Ok(BodyPart::Single(
            SinglePart::builder()
                .header(ContentType::TEXT_PLAIN)
                .body(String::new()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use envelope_email_store::models::Account;

    /// Minimal in-memory account for message-construction tests. No network,
    /// no credentials store — `build_message` never touches either.
    fn test_account(username: &str, display: Option<&str>) -> AccountWithCredentials {
        let account = Account {
            id: "acct-test".to_string(),
            name: "Test".to_string(),
            username: username.to_string(),
            domain: String::new(),
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            imap_host: "imap.example.com".to_string(),
            imap_port: 993,
            smtp_username: None,
            imap_username: None,
            display_name: display.map(str::to_string),
            signature_text: None,
            signature_html: None,
            created_at: String::new(),
        };
        AccountWithCredentials {
            account,
            password: "unused".to_string(),
            smtp_password: None,
            imap_password: None,
        }
    }

    fn header_message_id(email: &Message) -> String {
        email
            .headers()
            .get_raw("Message-ID")
            .map(|v| v.to_string())
            .unwrap_or_default()
    }

    #[test]
    fn generate_message_id_uses_account_domain() {
        let acct = test_account("alice@martin.fm", None);
        let id = generate_message_id(&acct);
        assert!(
            id.ends_with("@martin.fm"),
            "id should use account domain: {id}"
        );
        assert!(!id.starts_with('@'), "id must have a local part: {id}");
    }

    #[test]
    fn generate_message_id_falls_back_for_garbage_username() {
        for bad in ["", "no-at-sign", "local@", "local@no dot host"] {
            let acct = test_account(bad, None);
            let id = generate_message_id(&acct);
            assert!(
                id.ends_with("@envelope.local"),
                "garbage username {bad:?} should fall back: {id}"
            );
        }
    }

    #[test]
    fn build_message_always_sets_non_empty_message_id() {
        // Regression: every SMTP send must carry a Message-ID. Previously the
        // builder relied on lettre auto-generation and `get_raw` could return
        // empty, leaving agents with no Sent-folder lookup key.
        let acct = test_account("alice@martin.fm", Some("Alice"));
        let bare = generate_message_id(&acct);
        let (email, returned) = build_message(
            &acct,
            &bare,
            "bob@example.com",
            "Hello",
            Some("body text"),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            &[],
        )
        .unwrap();

        let header = header_message_id(&email);
        assert!(
            !header.trim().is_empty(),
            "header Message-ID must not be empty"
        );
        assert!(
            !returned.trim().is_empty(),
            "returned Message-ID must not be empty"
        );
        // The returned value matches what is on the wire.
        assert_eq!(returned, header);
        // The bare id we generated is preserved (modulo angle brackets).
        assert!(
            header.contains(strip_brackets(&bare).as_str()),
            "wire Message-ID {header} should preserve generated id {bare}"
        );
        assert!(header.contains("@martin.fm"));
    }

    #[test]
    fn build_message_message_id_survives_attachments_and_threading() {
        let acct = test_account("alice@martin.fm", None);
        let bare = generate_message_id(&acct);
        let refs = vec!["<parent@martin.fm>".to_string()];
        let attachments = vec![Attachment {
            filename: "note.txt".to_string(),
            content_type: "text/plain".to_string(),
            data: b"hi".to_vec(),
        }];
        let (email, returned) = build_message(
            &acct,
            &bare,
            "bob@example.com",
            "Re: Hello",
            Some("text"),
            Some("<p>html</p>"),
            None,
            Some("carol@example.com"),
            None,
            None,
            Some("<parent@martin.fm>"),
            Some(refs.as_slice()),
            &attachments,
        )
        .unwrap();

        let header = header_message_id(&email);
        assert!(!header.trim().is_empty());
        assert_eq!(returned, header);
        assert!(header.contains(strip_brackets(&bare).as_str()));
    }

    #[test]
    fn strip_brackets_handles_wrapped_and_bare() {
        assert_eq!(strip_brackets("<a@b>"), "a@b");
        assert_eq!(strip_brackets("a@b"), "a@b");
        assert_eq!(strip_brackets("  <a@b>  "), "a@b");
    }

    #[test]
    fn attachment_struct_defaults() {
        let att = Attachment {
            filename: "test.txt".to_string(),
            content_type: "text/plain".to_string(),
            data: b"hello world".to_vec(),
        };
        assert_eq!(att.filename, "test.txt");
        assert_eq!(att.data.len(), 11);
    }

    #[test]
    fn parse_mailboxes_accepts_single_address() {
        let mboxes = parse_mailboxes("a@example.com", "to").unwrap();
        assert_eq!(mboxes.iter().count(), 1);
    }

    #[test]
    fn parse_mailboxes_accepts_comma_separated_list() {
        let mboxes = parse_mailboxes("a@example.com, b@example.com, c@example.com", "cc").unwrap();
        assert_eq!(mboxes.iter().count(), 3);
    }

    #[test]
    fn parse_mailboxes_accepts_display_names() {
        let mboxes =
            parse_mailboxes("Alice <a@example.com>, \"Bob Q\" <b@example.com>", "to").unwrap();
        let parsed: Vec<_> = mboxes.iter().collect();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].email.to_string(), "a@example.com");
        assert_eq!(parsed[1].email.to_string(), "b@example.com");
    }

    #[test]
    fn parse_mailboxes_rejects_garbage() {
        let err = parse_mailboxes("not an address", "bcc").unwrap_err();
        assert!(err.to_string().contains("invalid bcc address"));
    }

    #[test]
    fn unknown_content_type_falls_back_to_octet_stream() {
        let result: ContentType = "not/a valid mime type!!"
            .parse::<ContentType>()
            .unwrap_or(ContentType::parse("application/octet-stream").unwrap());
        let _ = result; // just ensure the fallback path compiles
    }
}
