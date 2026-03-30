// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use envelope_email_store::models::AccountWithCredentials;
use lettre::message::{MultiPart, SinglePart, header::ContentType};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use tracing::info;

use crate::errors::SmtpError;

/// SMTP sender — stateless, builds a transport per send.
pub struct SmtpSender;

impl SmtpSender {
    /// Send an email through the account's SMTP server.
    ///
    /// Returns the generated Message-ID on success.
    #[allow(clippy::too_many_arguments)]
    pub async fn send(
        account: &AccountWithCredentials,
        to: &str,
        subject: &str,
        text: Option<&str>,
        html: Option<&str>,
        cc: Option<&str>,
        bcc: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<String, SmtpError> {
        let from_addr = if let Some(ref display) = account.account.display_name {
            format!("{display} <{}>", account.account.username)
        } else {
            account.account.username.clone()
        };

        // Build the message
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

        let email = match (text, html) {
            (Some(t), Some(h)) => builder
                .multipart(
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
                )
                .map_err(|e| SmtpError::Send(format!("failed to build multipart message: {e}")))?,
            (Some(t), None) => builder
                .header(ContentType::TEXT_PLAIN)
                .body(t.to_string())
                .map_err(|e| SmtpError::Send(format!("failed to build text message: {e}")))?,
            (None, Some(h)) => builder
                .header(ContentType::TEXT_HTML)
                .body(h.to_string())
                .map_err(|e| SmtpError::Send(format!("failed to build html message: {e}")))?,
            (None, None) => builder
                .header(ContentType::TEXT_PLAIN)
                .body(String::new())
                .map_err(|e| SmtpError::Send(format!("failed to build empty message: {e}")))?,
        };

        // Extract Message-ID before sending
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

        info!("sending email via {smtp_host}:{smtp_port} to {to}");

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
