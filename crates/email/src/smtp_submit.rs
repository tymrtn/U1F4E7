// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! SMTP submission as an explicit, staged state machine.
//!
//! lettre's transport runs MAIL, RCPT, DATA, the body and QUIT as one call.
//! A caller of that call cannot tell a failure before the body from one after
//! it, and a QUIT that hangs after the server's 250 comes back as a failed
//! send. This module drives lettre's public connection API one step at a
//! time, so a caller can:
//!
//! - commit a durable "transmitting" record after the server answers DATA
//!   with 354 and before the first body byte is written
//!   ([`OpenSubmission::transmit`]). A crash before that point submitted
//!   nothing;
//! - learn the outcome from the server's final reply to the body, before and
//!   independent of QUIT;
//! - classify every failure by the protocol stage it reached.
//!
//! lettre 0.11 stops at the first rejected RCPT and never issues DATA, so a
//! partial recipient rejection submits nothing. The tests below pin that.

use std::future::Future;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use envelope_email_store::models::AccountWithCredentials;
use lettre::Message;
use lettre::address::Envelope;
use lettre::transport::smtp::Error as LettreError;
use lettre::transport::smtp::authentication::{Credentials, DEFAULT_MECHANISMS};
use lettre::transport::smtp::client::{AsyncSmtpConnection, TlsParameters};
use lettre::transport::smtp::commands::{Data, Mail, Rcpt};
use lettre::transport::smtp::extension::{ClientId, Extension, MailBodyParameter, MailParameter};
use lettre::transport::smtp::response::{Response, Severity};

/// lettre's own TCP connect timeout for the relay transport.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// Time limits for one submission. lettre's async client has no read
/// timeout, so without these a silent server would hold a send forever.
#[derive(Debug, Clone, Copy)]
pub struct Deadlines {
    /// Everything before the body: connect, TLS, EHLO, AUTH, MAIL, RCPT, DATA.
    pub open: Duration,
    /// Writing the body and reading the server's final reply to it.
    pub body: Duration,
    /// QUIT. The outcome is already known when it is sent.
    pub quit: Duration,
}

impl Default for Deadlines {
    fn default() -> Self {
        Self {
            open: Duration::from_secs(10 * 60),
            body: Duration::from_secs(10 * 60),
            quit: Duration::from_secs(10),
        }
    }
}

/// The protocol step a submission reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitStage {
    Connect,
    Auth,
    Mail,
    Rcpt,
    Data,
    Body,
}

impl SubmitStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Auth => "auth",
            Self::Mail => "mail",
            Self::Rcpt => "rcpt",
            Self::Data => "data",
            Self::Body => "body",
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Connect,
            1 => Self::Auth,
            2 => Self::Mail,
            3 => Self::Rcpt,
            4 => Self::Data,
            _ => Self::Body,
        }
    }
}

/// Why a submission did not end in an accepted message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitFailure {
    /// Nothing was submitted. The failure came before the first body byte
    /// was written, or the server answered the body with a 4xx/5xx reply.
    NotSubmitted {
        stage: SubmitStage,
        /// The server's reply code when it refused a command.
        reply_code: Option<u16>,
        /// A 5xx refusal: retrying the same message will fail the same way.
        permanent: bool,
        error: String,
    },
    /// The body was being written, or was written, and no final reply was
    /// read. The server may have accepted the message.
    Uncertain { stage: SubmitStage, error: String },
}

impl SubmitFailure {
    pub fn stage(&self) -> SubmitStage {
        match self {
            Self::NotSubmitted { stage, .. } | Self::Uncertain { stage, .. } => *stage,
        }
    }

    /// True when a later attempt of the same message may succeed and cannot
    /// duplicate it.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::NotSubmitted {
                permanent: false,
                ..
            }
        )
    }

    /// Stable machine-readable evidence for receipts and outcomes.
    pub fn evidence(&self) -> serde_json::Value {
        match self {
            Self::NotSubmitted {
                stage,
                reply_code,
                permanent,
                error,
            } => serde_json::json!({
                "kind": "smtp_not_submitted",
                "stage": stage.as_str(),
                "reply_code": reply_code,
                "permanent": permanent,
                "error": truncate(error, 300),
            }),
            Self::Uncertain { stage, error } => serde_json::json!({
                "kind": "smtp_outcome_unknown",
                "stage": stage.as_str(),
                "error": truncate(error, 300),
            }),
        }
    }
}

impl std::fmt::Display for SubmitFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSubmitted {
                stage,
                reply_code: Some(code),
                error,
                ..
            } => write!(
                f,
                "SMTP server refused the message at {} ({code}): {error}",
                stage.as_str()
            ),
            Self::NotSubmitted { stage, error, .. } => {
                write!(
                    f,
                    "SMTP failed at {} before the message was sent: {error}",
                    stage.as_str()
                )
            }
            Self::Uncertain { stage, error } => write!(
                f,
                "SMTP outcome unknown after {} began: {error}",
                stage.as_str()
            ),
        }
    }
}

impl std::error::Error for SubmitFailure {}

/// Why a connection could not be opened. Nothing was submitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectFailure {
    pub reply_code: Option<u16>,
    pub permanent: bool,
    pub error: String,
}

impl From<LettreError> for ConnectFailure {
    fn from(e: LettreError) -> Self {
        Self {
            reply_code: reply_code(&e),
            permanent: e.is_permanent(),
            error: e.to_string(),
        }
    }
}

/// Opens an SMTP connection that has completed the greeting, EHLO and TLS.
///
/// Production uses [`AccountConnector`]. Tests implement it over a plain TCP
/// stream to a scripted server.
pub trait SmtpConnect {
    fn connect(&self) -> impl Future<Output = Result<AsyncSmtpConnection, ConnectFailure>> + Send;

    /// Credentials for AUTH, when the server requires them.
    fn credentials(&self) -> Option<Credentials>;
}

/// Connects to an account's SMTP server: implicit TLS on port 465, STARTTLS
/// (required) on every other port, the same choice the relay transport made.
pub struct AccountConnector<'a> {
    account: &'a AccountWithCredentials,
}

impl<'a> AccountConnector<'a> {
    pub fn new(account: &'a AccountWithCredentials) -> Self {
        Self { account }
    }
}

impl SmtpConnect for AccountConnector<'_> {
    fn connect(&self) -> impl Future<Output = Result<AsyncSmtpConnection, ConnectFailure>> + Send {
        let host = self.account.account.smtp_host.clone();
        let port = self.account.account.smtp_port;
        async move {
            let hello = ClientId::default();
            let tls = TlsParameters::new(host.clone())?;
            if port == 465 {
                Ok(AsyncSmtpConnection::connect_tokio1(
                    (host.as_str(), port),
                    Some(CONNECT_TIMEOUT),
                    &hello,
                    Some(tls),
                    None,
                )
                .await?)
            } else {
                let mut conn = AsyncSmtpConnection::connect_tokio1(
                    (host.as_str(), port),
                    Some(CONNECT_TIMEOUT),
                    &hello,
                    None,
                    None,
                )
                .await?;
                conn.starttls(tls, &hello).await?;
                Ok(conn)
            }
        }
    }

    fn credentials(&self) -> Option<Credentials> {
        Some(Credentials::new(
            self.account.effective_smtp_username().to_string(),
            self.account.effective_smtp_password().to_string(),
        ))
    }
}

/// A connection whose server has answered DATA with 354 and is waiting for
/// the body. Nothing has been submitted yet.
pub struct OpenSubmission {
    conn: AsyncSmtpConnection,
    deadlines: Deadlines,
}

/// The server answered the body with a 2xx: it accepted the message.
pub struct AcceptedSubmission {
    conn: AsyncSmtpConnection,
    deadlines: Deadlines,
    /// The server's final reply, e.g. `250 2.0.0 Ok: queued as 4F1`.
    pub reply: String,
}

/// Connect, authenticate, and run MAIL, RCPT and DATA. Every failure here is
/// [`SubmitFailure::NotSubmitted`]: no body byte has been written.
pub async fn open_submission<C: SmtpConnect>(
    connector: &C,
    envelope: &Envelope,
    body_is_ascii: bool,
    deadlines: Deadlines,
) -> Result<OpenSubmission, SubmitFailure> {
    let stage = AtomicU8::new(SubmitStage::Connect as u8);
    let opened = tokio::time::timeout(
        deadlines.open,
        open_steps(connector, envelope, body_is_ascii, deadlines, &stage),
    )
    .await;
    match opened {
        Ok(result) => result,
        // Dropping the unfinished future closes the socket. No body was
        // written, so there is nothing to be unsure about.
        Err(_) => Err(SubmitFailure::NotSubmitted {
            stage: SubmitStage::from_u8(stage.load(Ordering::SeqCst)),
            reply_code: None,
            permanent: false,
            error: format!(
                "no progress within {}s before the message body was sent",
                deadlines.open.as_secs()
            ),
        }),
    }
}

async fn open_steps<C: SmtpConnect>(
    connector: &C,
    envelope: &Envelope,
    body_is_ascii: bool,
    deadlines: Deadlines,
    stage: &AtomicU8,
) -> Result<OpenSubmission, SubmitFailure> {
    let mut conn = connector
        .connect()
        .await
        .map_err(|e| SubmitFailure::NotSubmitted {
            stage: SubmitStage::Connect,
            reply_code: e.reply_code,
            permanent: e.permanent,
            error: e.error,
        })?;

    if let Some(credentials) = connector.credentials() {
        stage.store(SubmitStage::Auth as u8, Ordering::SeqCst);
        if let Err(e) = conn.auth(DEFAULT_MECHANISMS, &credentials).await {
            quit(&mut conn, deadlines).await;
            return Err(refused(SubmitStage::Auth, &e));
        }
    }

    stage.store(SubmitStage::Mail as u8, Ordering::SeqCst);
    let mut mail_options = Vec::new();
    let non_ascii_address = envelope
        .from()
        .is_some_and(|a| !AsRef::<str>::as_ref(a).is_ascii())
        || envelope
            .to()
            .iter()
            .any(|a| !AsRef::<str>::as_ref(a).is_ascii());
    if non_ascii_address {
        if !conn.server_info().supports_feature(Extension::SmtpUtfEight) {
            quit(&mut conn, deadlines).await;
            return Err(SubmitFailure::NotSubmitted {
                stage: SubmitStage::Mail,
                reply_code: None,
                permanent: true,
                error:
                    "an address has non-ASCII characters and the server does not support SMTPUTF8"
                        .to_string(),
            });
        }
        mail_options.push(MailParameter::SmtpUtfEight);
    }
    if !body_is_ascii {
        if !conn.server_info().supports_feature(Extension::EightBitMime) {
            quit(&mut conn, deadlines).await;
            return Err(SubmitFailure::NotSubmitted {
                stage: SubmitStage::Mail,
                reply_code: None,
                permanent: true,
                error: "the message has non-ASCII bytes and the server does not support 8BITMIME"
                    .to_string(),
            });
        }
        mail_options.push(MailParameter::Body(MailBodyParameter::EightBitMime));
    }
    if let Err(e) = conn
        .command(Mail::new(envelope.from().cloned(), mail_options))
        .await
    {
        quit(&mut conn, deadlines).await;
        return Err(refused(SubmitStage::Mail, &e));
    }

    stage.store(SubmitStage::Rcpt as u8, Ordering::SeqCst);
    for recipient in envelope.to() {
        if let Err(e) = conn.command(Rcpt::new(recipient.clone(), vec![])).await {
            quit(&mut conn, deadlines).await;
            return Err(refused(SubmitStage::Rcpt, &e));
        }
    }

    stage.store(SubmitStage::Data as u8, Ordering::SeqCst);
    if let Err(e) = conn.command(Data).await {
        quit(&mut conn, deadlines).await;
        return Err(refused(SubmitStage::Data, &e));
    }

    Ok(OpenSubmission { conn, deadlines })
}

impl OpenSubmission {
    /// Write the body and read the server's final reply.
    ///
    /// Only a 2xx reply is acceptance. A 4xx/5xx reply is an authoritative
    /// refusal ([`SubmitFailure::NotSubmitted`]). Anything else (a broken
    /// connection, a malformed or missing reply, the deadline) is
    /// [`SubmitFailure::Uncertain`]: the server may hold the message.
    pub async fn transmit(mut self, body: &[u8]) -> Result<AcceptedSubmission, SubmitFailure> {
        let deadlines = self.deadlines;
        match tokio::time::timeout(deadlines.body, self.conn.message(body)).await {
            Ok(Ok(response)) if response.code().severity == Severity::PositiveCompletion => {
                Ok(AcceptedSubmission {
                    reply: response_text(&response),
                    conn: self.conn,
                    deadlines,
                })
            }
            Ok(Ok(response)) => Err(SubmitFailure::Uncertain {
                stage: SubmitStage::Body,
                error: format!(
                    "unexpected reply to the message body: {}",
                    response_text(&response)
                ),
            }),
            Ok(Err(e)) => match reply_code(&e) {
                Some(code) => {
                    quit(&mut self.conn, deadlines).await;
                    Err(SubmitFailure::NotSubmitted {
                        stage: SubmitStage::Body,
                        reply_code: Some(code),
                        permanent: e.is_permanent(),
                        error: e.to_string(),
                    })
                }
                None => Err(SubmitFailure::Uncertain {
                    stage: SubmitStage::Body,
                    error: e.to_string(),
                }),
            },
            Err(_) => Err(SubmitFailure::Uncertain {
                stage: SubmitStage::Body,
                error: format!(
                    "no reply to the message body within {}s",
                    deadlines.body.as_secs()
                ),
            }),
        }
    }

    /// Abandon the submission before the body: QUIT, bounded. Nothing was
    /// submitted.
    pub async fn abort(mut self) {
        quit(&mut self.conn, self.deadlines).await;
    }
}

impl AcceptedSubmission {
    /// Send QUIT, bounded. The message was accepted whatever QUIT does.
    pub async fn close(mut self) {
        quit(&mut self.conn, self.deadlines).await;
    }
}

/// Open, transmit and close in one call, for sends that keep no durable
/// attempt record. Returns the server's final reply.
pub async fn submit_once<C: SmtpConnect>(
    connector: &C,
    message: &Message,
    deadlines: Deadlines,
) -> Result<String, SubmitFailure> {
    let body = message.formatted();
    let open = open_submission(connector, message.envelope(), body.is_ascii(), deadlines).await?;
    let accepted = open.transmit(&body).await?;
    let reply = accepted.reply.clone();
    accepted.close().await;
    Ok(reply)
}

/// QUIT with a deadline. lettre's `abort` sends QUIT unless the connection
/// is already broken, ignores the reply, and closes the stream. A server that
/// never answers QUIT costs at most `deadlines.quit`.
async fn quit(conn: &mut AsyncSmtpConnection, deadlines: Deadlines) {
    if tokio::time::timeout(deadlines.quit, conn.abort())
        .await
        .is_err()
    {
        tracing::debug!(
            "SMTP QUIT got no reply within {}s; connection dropped",
            deadlines.quit.as_secs()
        );
    }
}

fn refused(stage: SubmitStage, e: &LettreError) -> SubmitFailure {
    SubmitFailure::NotSubmitted {
        stage,
        reply_code: reply_code(e),
        permanent: e.is_permanent(),
        error: e.to_string(),
    }
}

fn reply_code(e: &LettreError) -> Option<u16> {
    e.status().and_then(|code| code.to_string().parse().ok())
}

fn response_text(response: &Response) -> String {
    let text: Vec<&str> = response.message().collect();
    truncate(&format!("{} {}", response.code(), text.join(" ")), 300)
}

fn truncate(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

#[cfg(any(test, feature = "test-support"))]
pub mod testing {
    //! A scripted SMTP server over plain TCP, for driving the state machine
    //! through every outcome without TLS or a real relay.

    use super::*;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    #[derive(Clone, Debug)]
    pub enum AfterBody {
        Reply(&'static str),
        Close,
        Hang,
    }

    #[derive(Clone, Debug)]
    pub struct Script {
        pub auth: &'static str,
        pub rcpt: Vec<&'static str>,
        pub data: &'static str,
        pub after_body: AfterBody,
        pub answer_quit: bool,
    }

    impl Default for Script {
        fn default() -> Self {
            Self {
                auth: "235 2.7.0 ok",
                rcpt: Vec::new(),
                data: "354 go ahead",
                after_body: AfterBody::Reply("250 2.0.0 queued as T1"),
                answer_quit: true,
            }
        }
    }

    #[derive(Default, Debug)]
    pub struct Transcript {
        pub commands: Vec<String>,
        pub bodies: Vec<String>,
    }

    pub struct ScriptedServer {
        pub addr: SocketAddr,
        pub transcript: Arc<Mutex<Transcript>>,
    }

    impl ScriptedServer {
        /// Serve `script` to every connection until the test ends.
        pub async fn start(script: Script) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let transcript = Arc::new(Mutex::new(Transcript::default()));
            let shared = transcript.clone();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(serve(stream, script.clone(), shared.clone()));
                }
            });
            Self { addr, transcript }
        }

        pub fn bodies(&self) -> usize {
            self.transcript.lock().unwrap().bodies.len()
        }

        pub fn saw(&self, verb: &str) -> bool {
            self.transcript
                .lock()
                .unwrap()
                .commands
                .iter()
                .any(|c| c.to_ascii_uppercase().starts_with(verb))
        }
    }

    async fn serve(stream: TcpStream, script: Script, transcript: Arc<Mutex<Transcript>>) {
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read);
        let mut rcpt = script.rcpt.iter();
        write.write_all(b"220 scripted ESMTP\r\n").await.ok();
        loop {
            let mut line = String::new();
            if lines.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            let command = line.trim_end().to_string();
            transcript.lock().unwrap().commands.push(command.clone());
            let verb = command
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            let reply: String = match verb.as_str() {
                "EHLO" => "250-scripted\r\n250-8BITMIME\r\n250-SMTPUTF8\r\n250 AUTH PLAIN LOGIN"
                    .to_string(),
                "AUTH" => script.auth.to_string(),
                "MAIL" => "250 2.1.0 ok".to_string(),
                "RCPT" => rcpt.next().copied().unwrap_or("250 2.1.5 ok").to_string(),
                "DATA" => {
                    write
                        .write_all(format!("{}\r\n", script.data).as_bytes())
                        .await
                        .ok();
                    if !script.data.starts_with("354") {
                        continue;
                    }
                    let mut body = String::new();
                    loop {
                        let mut part = String::new();
                        if lines.read_line(&mut part).await.unwrap_or(0) == 0 {
                            return;
                        }
                        if part == ".\r\n" {
                            break;
                        }
                        body.push_str(&part);
                    }
                    transcript.lock().unwrap().bodies.push(body);
                    match &script.after_body {
                        AfterBody::Reply(reply) => {
                            write
                                .write_all(format!("{reply}\r\n").as_bytes())
                                .await
                                .ok();
                            continue;
                        }
                        AfterBody::Close => return,
                        AfterBody::Hang => {
                            tokio::time::sleep(Duration::from_secs(3600)).await;
                            return;
                        }
                    }
                }
                "QUIT" => {
                    if script.answer_quit {
                        write.write_all(b"221 bye\r\n").await.ok();
                        return;
                    }
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    return;
                }
                _ => "500 unknown".to_string(),
            };
            write
                .write_all(format!("{reply}\r\n").as_bytes())
                .await
                .ok();
        }
    }

    /// Plain-TCP connector to a [`ScriptedServer`].
    pub struct PlainConnector {
        pub addr: SocketAddr,
    }

    impl SmtpConnect for PlainConnector {
        fn connect(
            &self,
        ) -> impl Future<Output = Result<AsyncSmtpConnection, ConnectFailure>> + Send {
            let addr = self.addr;
            async move {
                let stream = TcpStream::connect(addr).await.map_err(|e| ConnectFailure {
                    reply_code: None,
                    permanent: false,
                    error: e.to_string(),
                })?;
                Ok(AsyncSmtpConnection::connect_with_transport(
                    Box::new(stream),
                    &ClientId::default(),
                )
                .await?)
            }
        }

        fn credentials(&self) -> Option<Credentials> {
            Some(Credentials::new("user".into(), "pass".into()))
        }
    }

    pub fn fast() -> Deadlines {
        Deadlines {
            open: Duration::from_secs(5),
            body: Duration::from_millis(500),
            quit: Duration::from_millis(200),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    fn message(to: &[&str]) -> Message {
        let mut builder = Message::builder()
            .from("sender@example.test".parse().unwrap())
            .subject("scripted");
        for addr in to {
            builder = builder.to(addr.parse().unwrap());
        }
        builder.body("hello\r\n".to_string()).unwrap()
    }

    async fn run(script: Script, to: &[&str]) -> (ScriptedServer, Result<String, SubmitFailure>) {
        let server = ScriptedServer::start(script).await;
        let result = submit_once(&PlainConnector { addr: server.addr }, &message(to), fast()).await;
        (server, result)
    }

    #[tokio::test]
    async fn accepted_message_returns_the_final_reply() {
        let (server, result) = run(Script::default(), &["a@example.test"]).await;
        assert!(result.unwrap().starts_with("250"));
        assert_eq!(server.bodies(), 1);
        assert!(server.saw("QUIT"));
    }

    #[tokio::test]
    async fn a_rejected_rcpt_stops_before_data_even_after_an_accepted_one() {
        let script = Script {
            rcpt: vec!["250 2.1.5 ok", "550 5.1.1 no such user"],
            ..Script::default()
        };
        let (server, result) = run(script, &["a@example.test", "b@example.test"]).await;
        assert!(matches!(
            result.unwrap_err(),
            SubmitFailure::NotSubmitted {
                stage: SubmitStage::Rcpt,
                reply_code: Some(550),
                permanent: true,
                ..
            }
        ));
        assert!(
            !server.saw("DATA"),
            "DATA must never follow a rejected RCPT"
        );
        assert_eq!(server.bodies(), 0);
    }

    #[tokio::test]
    async fn a_refused_data_command_submits_nothing() {
        let script = Script {
            data: "554 5.5.1 no valid recipients",
            ..Script::default()
        };
        let (server, result) = run(script, &["a@example.test"]).await;
        let failure = result.unwrap_err();
        assert_eq!(failure.stage(), SubmitStage::Data);
        assert!(matches!(
            failure,
            SubmitFailure::NotSubmitted {
                reply_code: Some(554),
                permanent: true,
                ..
            }
        ));
        assert_eq!(server.bodies(), 0);
    }

    #[tokio::test]
    async fn a_4xx_reply_to_the_body_is_a_retryable_refusal() {
        let script = Script {
            after_body: AfterBody::Reply("451 4.3.0 try later"),
            ..Script::default()
        };
        let (_server, result) = run(script, &["a@example.test"]).await;
        let failure = result.unwrap_err();
        assert!(matches!(
            failure,
            SubmitFailure::NotSubmitted {
                stage: SubmitStage::Body,
                reply_code: Some(451),
                permanent: false,
                ..
            }
        ));
        assert!(failure.retryable());
    }

    #[tokio::test]
    async fn a_5xx_reply_to_the_body_is_a_permanent_refusal() {
        let script = Script {
            after_body: AfterBody::Reply("554 5.7.1 rejected"),
            ..Script::default()
        };
        let (_server, result) = run(script, &["a@example.test"]).await;
        let failure = result.unwrap_err();
        assert!(matches!(
            failure,
            SubmitFailure::NotSubmitted {
                stage: SubmitStage::Body,
                reply_code: Some(554),
                permanent: true,
                ..
            }
        ));
        assert!(!failure.retryable());
    }

    #[tokio::test]
    async fn a_connection_lost_after_the_body_is_uncertain() {
        let script = Script {
            after_body: AfterBody::Close,
            ..Script::default()
        };
        let (server, result) = run(script, &["a@example.test"]).await;
        let failure = result.unwrap_err();
        assert!(matches!(
            failure,
            SubmitFailure::Uncertain {
                stage: SubmitStage::Body,
                ..
            }
        ));
        assert!(!failure.retryable());
        assert_eq!(server.bodies(), 1, "the server holds the message");
    }

    #[tokio::test]
    async fn a_missing_final_reply_is_uncertain_at_the_deadline() {
        let script = Script {
            after_body: AfterBody::Hang,
            ..Script::default()
        };
        let (_server, result) = run(script, &["a@example.test"]).await;
        assert!(matches!(
            result.unwrap_err(),
            SubmitFailure::Uncertain {
                stage: SubmitStage::Body,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn acceptance_is_known_before_a_hanging_quit() {
        let script = Script {
            answer_quit: false,
            ..Script::default()
        };
        let server = ScriptedServer::start(script).await;
        let msg = message(&["a@example.test"]);
        let body = msg.formatted();
        let open = open_submission(
            &PlainConnector { addr: server.addr },
            msg.envelope(),
            true,
            fast(),
        )
        .await
        .unwrap();
        let accepted = open.transmit(&body).await.expect("250 is acceptance");
        assert!(accepted.reply.starts_with("250"));
        let started = std::time::Instant::now();
        accepted.close().await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a silent QUIT is bounded by the quit deadline"
        );
    }

    #[tokio::test]
    async fn a_refused_login_is_a_permanent_refusal_at_auth() {
        let script = Script {
            auth: "535 5.7.8 bad credentials",
            ..Script::default()
        };
        let (server, result) = run(script, &["a@example.test"]).await;
        assert!(matches!(
            result.unwrap_err(),
            SubmitFailure::NotSubmitted {
                stage: SubmitStage::Auth,
                reply_code: Some(535),
                permanent: true,
                ..
            }
        ));
        assert!(!server.saw("MAIL"));
    }

    #[tokio::test]
    async fn an_unreachable_server_submits_nothing() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let result = submit_once(
            &PlainConnector { addr },
            &message(&["a@example.test"]),
            fast(),
        )
        .await;
        let failure = result.unwrap_err();
        assert_eq!(failure.stage(), SubmitStage::Connect);
        assert!(failure.retryable());
    }
}
