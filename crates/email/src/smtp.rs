// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use envelope_email_store::models::AccountWithCredentials;
use lettre::message::header::ContentType;
use lettre::message::{Attachment as LettreAttachment, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use tracing::info;

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
            account, to, subject, text, html, None, cc, bcc, reply_to, None, None, &[],
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
        let from_addr = if let Some(f) = from_override {
            f.to_string()
        } else if let Some(ref display) = account.account.display_name {
            format!("{display} <{}>", account.account.username)
        } else {
            account.account.username.clone()
        };

        // Build the message headers
        let mut builder = Message::builder()
            .from(
                from_addr
                    .parse()
                    .map_err(|e| SmtpError::Send(format!("invalid from address: {e}")))?,
            )
            .to(to
                .parse()
                .map_err(|e| SmtpError::Send(format!("invalid to address: {e}")))?)
            .subject(subject);

        if let Some(cc_addr) = cc {
            builder = builder.cc(cc_addr
                .parse()
                .map_err(|e| SmtpError::Send(format!("invalid cc address: {e}")))?);
        }

        if let Some(bcc_addr) = bcc {
            builder = builder.bcc(
                bcc_addr
                    .parse()
                    .map_err(|e| SmtpError::Send(format!("invalid bcc address: {e}")))?,
            );
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
                let attachment = LettreAttachment::new(att.filename.clone())
                    .body(att.data.clone(), ct);
                // LettreAttachment::new().body() returns a SinglePart ready to append
                mixed = mixed.singlepart(attachment);
            }
            builder
                .multipart(mixed)
                .map_err(|e| SmtpError::Send(format!("failed to build multipart message: {e}")))?
        };

        // Extract Message-ID before sending (avoids the body consuming `email`)
        let message_id = email
            .headers()
            .get_raw("Message-ID")
            .map(|v| v.to_string())
            .unwrap_or_default();

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
    fn unknown_content_type_falls_back_to_octet_stream() {
        let result: ContentType = "not/a valid mime type!!"
            .parse::<ContentType>()
            .unwrap_or(ContentType::parse("application/octet-stream").unwrap());
        let _ = result; // just ensure the fallback path compiles
    }
}
