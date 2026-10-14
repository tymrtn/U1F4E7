// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! IMAP IDLE support — returns a raw `ImapSession` suitable for the
//! ownership-transfer pattern required by `Session::idle()`.

use envelope_email_store::models::AccountWithCredentials;

use crate::errors::ImapError;
use crate::imap::ImapSession;

/// Connect to an IMAP server over TLS, authenticate, and return the raw
/// `Session<TlsStream<TcpStream>>`.  Unlike `imap::connect()` this does NOT
/// wrap the session in `ImapClient`, so the caller can pass ownership to
/// `session.idle()`. TLS, STARTTLS and certificate pinning are the same as
/// `imap::connect()`.
pub async fn connect_session(account: &AccountWithCredentials) -> Result<ImapSession, ImapError> {
    crate::imap::open_session(account).await
}
