// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ImapError {
    #[error("IMAP authentication failed: {0}")]
    Auth(String),

    #[error("IMAP connection failed: {0}")]
    Connection(String),

    #[error("IMAP protocol error: {0}")]
    Protocol(String),

    #[error("message not found: uid {0}")]
    NotFound(u32),
}

#[derive(Debug, Error)]
pub enum SmtpError {
    #[error("SMTP authentication failed: {0}")]
    Auth(String),

    #[error("SMTP connection failed: {0}")]
    Connection(String),

    #[error("recipient rejected: {0}")]
    RecipientRejected(String),

    #[error("SMTP send error: {0}")]
    Send(String),
}

impl From<crate::smtp_submit::SubmitFailure> for SmtpError {
    fn from(failure: crate::smtp_submit::SubmitFailure) -> Self {
        use crate::smtp_submit::SubmitStage;
        let message = failure.to_string();
        match failure.stage() {
            SubmitStage::Connect => Self::Connection(message),
            SubmitStage::Auth => Self::Auth(message),
            SubmitStage::Rcpt => Self::RecipientRejected(message),
            SubmitStage::Mail | SubmitStage::Data | SubmitStage::Body => Self::Send(message),
        }
    }
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("no candidates found for domain {0}")]
    NoCandidates(String),

    #[error("discovery timed out for domain {0}")]
    Timeout(String),

    #[error("DNS resolution failed: {0}")]
    Dns(String),
}
