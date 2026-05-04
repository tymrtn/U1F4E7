// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! IMAP IDLE support — returns a raw `ImapSession` suitable for the
//! ownership-transfer pattern required by `Session::idle()`.

use std::sync::Arc;

use envelope_email_store::models::AccountWithCredentials;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tracing::{debug, info};

use crate::errors::ImapError;
use crate::imap::ImapSession;

/// Connect to an IMAP server over TLS, authenticate, and return the raw
/// `Session<TlsStream<TcpStream>>`.  Unlike `imap::connect()` this does NOT
/// wrap the session in `ImapClient`, so the caller can pass ownership to
/// `session.idle()`.
pub async fn connect_session(account: &AccountWithCredentials) -> Result<ImapSession, ImapError> {
    let host = &account.account.imap_host;
    let port = account.account.imap_port;
    let username = account.effective_imap_username();
    let password = account.effective_imap_password();

    info!("idle: connecting to IMAP {host}:{port} as {username}");

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

    debug!("idle: IMAP session established for {username}@{host}");
    Ok(session)
}
