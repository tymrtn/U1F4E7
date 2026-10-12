// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! `envelope code` — poll IMAP for a verification code and extract it.

use anyhow::{Context, Result, bail};
use envelope_email_store::Account;
use envelope_email_store::credential_store::CredentialBackend;
use envelope_email_transport::code_extractor::extract_code;
use envelope_email_transport::imap;
use envelope_email_transport::threat::ThreatInput;
use envelope_email_transport::threat::auth_results::{SenderAuth, sender_auth};
use envelope_email_transport::threat::config::ThreatConfig;
use envelope_email_transport::threat::domains::ascii_host;
use serde_json::{Value, json};

use super::common::setup_credentials;
use super::provenance;

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
/// JSON is the supported unattended/agent surface. Collect for one extra poll
/// interval so a forged first arrival cannot win before legitimate candidates.
const AUTOMATION_STABILIZATION_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
struct OtpCandidate {
    code: String,
    from: String,
    subject: String,
    auth: SenderAuth,
}

/// One new message, judged as a one-time-code source.
#[derive(Debug, PartialEq, Eq)]
enum Classified {
    /// Outside the --from/--subject filters, or carries no code.
    NoMatch,
    /// A code from a sender whose From domain is authenticated.
    Candidate(OtpCandidate),
    /// A code from a sender that is not authenticated. Its code is never
    /// reported.
    Rejected(OtpCandidate),
}

/// Judge one raw message. The From filter, subject, code and sender
/// authentication all come from the same bytes. A candidate needs a pass
/// for the very domain in its From header, from results written in the
/// account's receiving mail domain (see [`ThreatConfig::receiver_domain`]).
fn classify(
    raw: &[u8],
    account: &Account,
    config: &ThreatConfig,
    from_filter: Option<&str>,
    subject_filter: Option<&str>,
) -> Classified {
    let Ok(mut input) = ThreatInput::from_raw(raw, &account.username) else {
        return Classified::NoMatch;
    };
    input.receiver_domain = config.receiver_domain(&account.username, Some(&account.imap_host));
    if from_filter.is_some_and(|filter| !sender_matches(&input.from_addr, filter)) {
        return Classified::NoMatch;
    }
    let subject = mail_parser::MessageParser::default()
        .parse(raw)
        .and_then(|message| message.subject().map(str::to_string))
        .unwrap_or_default();
    if subject_filter.is_some_and(|filter| !subject.to_lowercase().contains(&filter.to_lowercase()))
    {
        return Classified::NoMatch;
    }
    let Some(code) = extract_code(input.text.as_deref().unwrap_or(""), input.html.as_deref())
    else {
        return Classified::NoMatch;
    };
    let auth = sender_auth(&input);
    let authenticated = matches!(&auth, SenderAuth::Pass { domain, .. }
        if input.from_domain().map(|d| ascii_host(&d)).as_deref() == Some(domain.as_str()));
    let candidate = OtpCandidate {
        code,
        from: input.from_addr.clone(),
        subject,
        auth,
    };
    if authenticated {
        Classified::Candidate(candidate)
    } else {
        Classified::Rejected(candidate)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CollectionOutcome {
    Continue,
    Ready(OtpCandidate),
    Ambiguous(usize),
}

#[derive(Default)]
struct CandidateCollection {
    candidates: Vec<OtpCandidate>,
    rejected: Vec<OtpCandidate>,
    first_seen_at: Option<std::time::Duration>,
}

impl CandidateCollection {
    /// Keep the messages whose code may count and remember the rest. With the
    /// operator opt-in, a sender that cannot be verified counts; a sender
    /// that failed authentication never does.
    fn admit(&mut self, classified: Vec<Classified>, allow_unverified: bool) -> Vec<OtpCandidate> {
        let mut admitted = Vec::new();
        for item in classified {
            match item {
                Classified::NoMatch => {}
                Classified::Candidate(candidate) => admitted.push(candidate),
                Classified::Rejected(candidate)
                    if allow_unverified
                        && matches!(candidate.auth, SenderAuth::Unverifiable(_)) =>
                {
                    admitted.push(candidate)
                }
                Classified::Rejected(candidate) => self.rejected.push(candidate),
            }
        }
        admitted
    }

    fn observe(
        &mut self,
        now: std::time::Duration,
        matches: impl IntoIterator<Item = OtpCandidate>,
    ) -> CollectionOutcome {
        self.candidates.extend(matches);
        if self.candidates.len() > 1 {
            return CollectionOutcome::Ambiguous(self.candidates.len());
        }

        let Some(candidate) = self.candidates.first().cloned() else {
            return CollectionOutcome::Continue;
        };
        let first_seen_at = *self.first_seen_at.get_or_insert(now);
        if now.saturating_sub(first_seen_at) >= AUTOMATION_STABILIZATION_WINDOW {
            CollectionOutcome::Ready(candidate)
        } else {
            CollectionOutcome::Continue
        }
    }

    /// Codes that arrived from senders that were not authenticated. Their
    /// codes are never included.
    fn rejected_json(&self) -> Value {
        Value::Array(
            self.rejected
                .iter()
                .map(|c| json!({"from": c.from, "sender_auth": sender_auth_json(&c.auth)}))
                .collect(),
        )
    }

    /// The JSON error when the wait ends without a code.
    fn timeout_report(&self, waited_seconds: u64) -> Value {
        if !self.candidates.is_empty() || self.rejected.is_empty() {
            return json!({"error": "timeout", "waited_seconds": waited_seconds});
        }
        let (error, reason) = if self
            .rejected
            .iter()
            .any(|c| matches!(c.auth, SenderAuth::Unverifiable(_)))
        {
            (
                "sender_unverifiable",
                "a matching code arrived, but its sender could not be authenticated from what the mail provider recorded; an operator can allow unverified senders for this account (otp.allow_unverified_senders)",
            )
        } else {
            (
                "sender_unauthenticated",
                "a matching code arrived, but its sender failed authentication for the From domain",
            )
        };
        json!({
            "error": error,
            "reason": reason,
            "waited_seconds": waited_seconds,
            "rejected_candidates": self.rejected_json(),
        })
    }
}

fn sender_auth_json(auth: &SenderAuth) -> Value {
    match auth {
        SenderAuth::Pass {
            via,
            domain,
            authserv_id,
        } => json!({"result": "pass", "via": via, "domain": domain, "authserv_id": authserv_id}),
        SenderAuth::Fail => json!({"result": "fail"}),
        SenderAuth::Unverifiable(reason) => json!({"result": "unverifiable", "reason": reason}),
    }
}

/// The JSON result for the code that was accepted.
fn candidate_json(candidate: &OtpCandidate, collection: &CandidateCollection) -> Value {
    provenance::annotate_inbound(json!({
        "code": candidate.code,
        "from": candidate.from,
        "subject": candidate.subject,
        "sender_auth": sender_auth_json(&candidate.auth),
        "rejected_candidates": collection.rejected_json(),
    }))
}

/// `envelope code` — poll IMAP for new messages and extract a verification code.
///
/// A code counts only when its sender's From domain is authenticated by the
/// trusted Authentication-Results (DMARC, or aligned DKIM), unless the operator
/// lets this account accept unverified senders. Other arrivals never end the
/// wait. The JSON surface is unattended/agent automation: it requires a
/// caller-selected account and a narrow exact mailbox or full-domain sender
/// filter, then collects candidates across a bounded stabilization window.
/// Plain-text mode requires the sender filter too.
#[tokio::main]
pub async fn run(
    account: Option<&str>,
    from_filter: Option<&str>,
    subject_filter: Option<&str>,
    wait_secs: u64,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    if let Some(error) = binding_error(json, account, from_filter) {
        if json {
            println!(
                "{}",
                json!({
                    "error": "automation_binding_required",
                    "reason": error,
                    "trust": provenance::inbound_trust(),
                })
            );
        }
        bail!("{error}");
    }

    let (_db, creds) = setup_credentials(account, backend)?;
    let allow_unverified = super::config::otp_unverified_senders_allowed(&creds.account)?;
    let threat_config = ThreatConfig::load()
        .context("threat configuration is invalid; fix it with `envelope config`")?;
    let mut client = imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;
    let session = client.session_mut();
    session
        .select("INBOX")
        .await
        .map_err(|e| anyhow::anyhow!("SELECT INBOX: {e}"))?;
    let wait = CodeWait {
        account: &creds.account,
        threat_config: &threat_config,
        from_filter,
        subject_filter,
        allow_unverified,
        json,
        timeout: std::time::Duration::from_secs(wait_secs),
    };
    let mut collected = CandidateCollection::default();
    match wait_for_code(session, &wait, &mut collected).await? {
        WaitOutcome::Code(candidate) if json => {
            let output = candidate_json(&candidate, &collected);
            println!("{}", serde_json::to_string_pretty(&output)?);
            Ok(())
        }
        WaitOutcome::Code(candidate) => {
            println!("{}", candidate.code);
            Ok(())
        }
        WaitOutcome::Ambiguous(candidate_count) if json => {
            println!(
                "{}",
                json!({
                    "error": "ambiguous_matches",
                    "candidate_count": candidate_count,
                    "trust": provenance::inbound_trust(),
                })
            );
            bail!(
                "ambiguous OTP matches: {candidate_count} messages matched; refine --from or --subject"
            );
        }
        WaitOutcome::Ambiguous(candidate_count) => bail!(
            "ambiguous OTP matches: {candidate_count} messages matched this poll; refine --from or --subject"
        ),
        WaitOutcome::TimedOut => {
            let mut report = collected.timeout_report(wait_secs);
            if json {
                report["trust"] = provenance::inbound_trust();
                println!("{report}");
            } else if report["error"] == "timeout" {
                eprintln!("Timeout: no verification code found after {wait_secs}s");
            } else {
                eprintln!(
                    "No verification code accepted after {wait_secs}s ({}): {}",
                    report["error"].as_str().unwrap_or_default(),
                    report["reason"].as_str().unwrap_or_default()
                );
            }
            std::process::exit(1);
        }
    }
}

/// What one `envelope code` wait looks for, and for how long.
struct CodeWait<'a> {
    account: &'a Account,
    threat_config: &'a ThreatConfig,
    from_filter: Option<&'a str>,
    subject_filter: Option<&'a str>,
    allow_unverified: bool,
    /// JSON automation: candidates are collected across the stabilization
    /// window instead of returned on the first poll.
    json: bool,
    timeout: std::time::Duration,
}

/// How a wait for a code ended.
#[derive(Debug, PartialEq, Eq)]
enum WaitOutcome {
    Code(OtpCandidate),
    /// More than one accepted code: across the wait (JSON) or in one poll.
    Ambiguous(usize),
    TimedOut,
}

/// Poll INBOX, open on `session`, for a code in mail that arrives after the
/// wait starts. `collected` keeps what the polls found, for the result.
async fn wait_for_code<T>(
    session: &mut async_imap::Session<T>,
    wait: &CodeWait<'_>,
    collected: &mut CandidateCollection,
) -> Result<WaitOutcome>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + std::fmt::Debug + Send,
{
    // A read the server refused ends the wait. Taken for an empty answer, it
    // would start the wait at UID 0, or release the first code before the
    // stabilization window saw a second, and return an old code.
    let account = &wait.account.username;
    let initial_max_uid = imap::list_selected_uids_in(session)
        .await
        .with_context(|| format!("could not search INBOX on {account}; no code was returned"))?
        .last()
        .copied()
        .unwrap_or(0);
    if !wait.json {
        eprintln!(
            "Watching for verification codes (timeout: {}s, starting after UID {initial_max_uid})...",
            wait.timeout.as_secs()
        );
    }

    let start = tokio::time::Instant::now();
    let mut last_seen_uid = initial_max_uid;
    loop {
        let elapsed = start.elapsed();
        if elapsed >= wait.timeout {
            return Ok(WaitOutcome::TimedOut);
        }

        let new_uids = imap::list_selected_uids_after_in(session, last_seen_uid)
            .await
            .with_context(|| {
                format!("could not search INBOX on {account} for new mail; no code was returned")
            })?;
        let mut classified = Vec::new();
        for uid in new_uids {
            last_seen_uid = uid;
            // `BODY.PEEK[]`: reading never marks the message seen. A message
            // too large to fetch whole has no bytes to authenticate and is
            // skipped.
            let Some((_, Some(raw))) = imap::fetch_message_with_raw_in(session, "INBOX", uid)
                .await
                .with_context(|| {
                    format!("could not read UID {uid} in INBOX on {account}; no code was returned")
                })?
            else {
                continue;
            };
            classified.push(classify(
                &raw,
                wait.account,
                wait.threat_config,
                wait.from_filter,
                wait.subject_filter,
            ));
        }
        let matches = collected.admit(classified, wait.allow_unverified);

        if wait.json {
            match collected.observe(elapsed, matches) {
                CollectionOutcome::Continue => {}
                CollectionOutcome::Ready(candidate) => return Ok(WaitOutcome::Code(candidate)),
                CollectionOutcome::Ambiguous(count) => return Ok(WaitOutcome::Ambiguous(count)),
            }
        } else {
            // Interactive stdout use stays low-friction: no stabilization
            // window, but the same sender authentication.
            match matches.as_slice() {
                [] => {}
                [candidate] => return Ok(WaitOutcome::Code(candidate.clone())),
                _ => return Ok(WaitOutcome::Ambiguous(matches.len())),
            }
        }

        let remaining = wait.timeout.saturating_sub(start.elapsed());
        tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
    }
}

/// Return the reason the request is not bound to a sender (and, for JSON
/// automation, an account), if any. This runs before credentials are opened
/// or any network connection is attempted.
fn binding_error(
    json: bool,
    account: Option<&str>,
    from_filter: Option<&str>,
) -> Option<&'static str> {
    if json {
        return automation_binding_error(account, from_filter);
    }
    if from_filter.is_none_or(|value| value.trim().is_empty()) {
        return Some("--from is required: the exact sender address or domain the code comes from");
    }
    None
}

/// Return the reason JSON automation is not safely bound, if any. This runs
/// before credentials are opened or any network connection is attempted.
fn automation_binding_error(
    account: Option<&str>,
    from_filter: Option<&str>,
) -> Option<&'static str> {
    if account.is_none_or(|value| value.trim().is_empty()) {
        return Some("--account is required for JSON OTP automation");
    }
    if !from_filter.is_some_and(is_narrow_sender_filter) {
        return Some(
            "--from must be an exact mailbox address or full domain for JSON OTP automation",
        );
    }
    None
}

/// JSON automation accepts only an exact address or a fully-qualified domain;
/// display names, local fragments, wildcards, and sender substrings are broad.
fn is_narrow_sender_filter(raw_filter: &str) -> bool {
    let filter = raw_filter
        .trim()
        .trim_matches('<')
        .trim_matches('>')
        .to_lowercase();
    if filter.is_empty() || filter.contains(char::is_whitespace) {
        return false;
    }

    if let Some((local, domain)) = filter.rsplit_once('@') {
        return !local.is_empty()
            && !local.contains('@')
            && is_fully_qualified_domain(&ascii_host(domain));
    }
    is_fully_qualified_domain(&ascii_host(filter.trim_start_matches('@')))
}

fn is_fully_qualified_domain(domain: &str) -> bool {
    let labels: Vec<_> = domain.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// Extract a mailbox from a header and compare exact case-insensitive mailbox
/// identity or a whole domain, domains in ASCII (IDNA) form. Display names
/// are never candidates.
fn sender_matches(raw_sender: &str, raw_filter: &str) -> bool {
    let sender = mailbox_from_header(raw_sender);
    let filter = raw_filter
        .trim()
        .trim_matches('<')
        .trim_matches('>')
        .to_lowercase();
    let Some((sender_local, sender_domain)) = sender.rsplit_once('@') else {
        return false;
    };
    let sender_domain = ascii_host(sender_domain);
    if filter.contains('@') && !filter.starts_with('@') {
        return filter.rsplit_once('@').is_some_and(|(local, domain)| {
            local == sender_local && ascii_host(domain) == sender_domain
        });
    }
    let domain = filter.trim_start_matches('@');
    !domain.is_empty() && ascii_host(domain) == sender_domain
}

fn mailbox_from_header(raw: &str) -> String {
    let candidate = raw
        .rsplit_once('<')
        .and_then(|(_, rest)| rest.split_once('>').map(|(mailbox, _)| mailbox))
        .unwrap_or(raw)
        .trim()
        .to_lowercase();
    (candidate.matches('@').count() == 1 && !candidate.contains(char::is_whitespace))
        .then_some(candidate)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(code: &str) -> OtpCandidate {
        OtpCandidate {
            code: code.to_string(),
            from: "otp@issuer.example".to_string(),
            subject: "Your verification code".to_string(),
            auth: SenderAuth::Pass {
                via: "dmarc".to_string(),
                domain: "issuer.example".to_string(),
                authserv_id: "mx1.example.org".to_string(),
            },
        }
    }

    const RECEIVED_EDGE: &str = "Received: from mail.issuer.example (mail.issuer.example [203.0.113.5]) by mx1.example.org with ESMTPS id abc; Mon, 21 Sep 2026 10:00:00 +0000";

    /// A one-time-code message with the given Authentication-Results and From.
    fn otp_message(auth_results: Option<&str>, from: &str, code: &str) -> Vec<u8> {
        let mut headers = Vec::new();
        if let Some(ar) = auth_results {
            headers.push(format!("Authentication-Results: {ar}"));
        }
        headers.push(RECEIVED_EDGE.to_string());
        headers.push(format!("From: {from}"));
        headers.push("To: me@example.org".to_string());
        headers.push("Subject: Your verification code".to_string());
        format!(
            "{}\r\n\r\nYour verification code is {code}\r\n",
            headers.join("\r\n")
        )
        .into_bytes()
    }

    const ISSUER_PASS: &str =
        "mx1.example.org; dkim=pass header.d=issuer.example; dmarc=pass header.from=issuer.example";

    /// The account `me@example.org`, whose mail `imap_host` serves.
    fn mailbox(imap_host: &str) -> Account {
        Account {
            id: "acct-test".to_string(),
            name: "Me".to_string(),
            username: "me@example.org".to_string(),
            domain: "example.org".to_string(),
            smtp_host: "smtp.example.org".to_string(),
            smtp_port: 587,
            imap_host: imap_host.to_string(),
            imap_port: 993,
            smtp_username: None,
            imap_username: None,
            display_name: None,
            signature_text: None,
            signature_html: None,
            created_at: "2026-10-03T00:00:00Z".to_string(),
        }
    }

    fn classify_issuer(raw: &[u8]) -> Classified {
        classify(
            raw,
            &mailbox("imap.example.org"),
            &ThreatConfig::default(),
            Some("issuer.example"),
            None,
        )
    }

    fn forged(code: &str) -> Classified {
        classify_issuer(&otp_message(
            Some(
                "mx1.example.org; spf=fail smtp.mailfrom=issuer.example; dkim=none; dmarc=fail header.from=issuer.example",
            ),
            "Issuer <otp@issuer.example>",
            code,
        ))
    }

    fn authenticated(code: &str) -> Classified {
        classify_issuer(&otp_message(
            Some(ISSUER_PASS),
            "Issuer <otp@issuer.example>",
            code,
        ))
    }

    fn unverifiable(code: &str) -> Classified {
        classify_issuer(&otp_message(None, "Issuer <otp@issuer.example>", code))
    }

    #[test]
    fn classify_drops_spoofed_from() {
        match forged("111111") {
            Classified::Rejected(c) => assert_eq!(c.auth, SenderAuth::Fail),
            other => panic!("a failed sender must be rejected: {other:?}"),
        }
        // A pass for another domain does not cover this From.
        let other_domain = classify_issuer(&otp_message(
            Some(
                "mx1.example.org; dkim=pass header.d=attacker.example; dmarc=pass header.from=attacker.example",
            ),
            "Issuer <otp@issuer.example>",
            "222222",
        ));
        assert!(
            matches!(other_domain, Classified::Rejected(_)),
            "{other_domain:?}"
        );
        // A sender outside the --from filter is not a match at all.
        let elsewhere = classify_issuer(&otp_message(
            Some(ISSUER_PASS),
            "otp@issuer.example.evil",
            "333333",
        ));
        assert_eq!(elsewhere, Classified::NoMatch);
        match authenticated("444444") {
            Classified::Candidate(c) => {
                assert_eq!(c.code, "444444");
                assert_eq!(c.from, "otp@issuer.example");
                assert!(matches!(c.auth, SenderAuth::Pass { .. }));
            }
            other => panic!("an authenticated sender is a candidate: {other:?}"),
        }
    }

    #[test]
    fn classify_trusts_only_the_accounts_receiver() {
        let raw = otp_message(Some(ISSUER_PASS), "Issuer <otp@issuer.example>", "555555");
        let default = ThreatConfig::default();
        let mut moved = ThreatConfig::default();
        moved
            .receiver_domains
            .insert("me@example.org".to_string(), "attacker.example".to_string());
        let mut pinned = ThreatConfig::default();
        pinned
            .receiver_domains
            .insert("me@example.org".to_string(), "example.org".to_string());

        // The IMAP host names the receiving domain, or the operator does.
        for (imap_host, config) in [("imap.example.org", &default), ("127.0.0.1", &pinned)] {
            let classified = classify(
                &raw,
                &mailbox(imap_host),
                config,
                Some("issuer.example"),
                None,
            );
            assert!(
                matches!(classified, Classified::Candidate(_)),
                "{imap_host}: {classified:?}"
            );
        }
        // Results from anywhere else prove nothing.
        for (imap_host, config) in [
            ("imap.attacker.example", &default),
            ("127.0.0.1", &default),
            ("imap.example.org", &moved),
        ] {
            match classify(
                &raw,
                &mailbox(imap_host),
                config,
                Some("issuer.example"),
                None,
            ) {
                Classified::Rejected(c) => {
                    assert!(matches!(c.auth, SenderAuth::Unverifiable(_)), "{imap_host}")
                }
                other => panic!("{imap_host}: {other:?}"),
            }
        }
    }

    #[test]
    fn classify_accepts_either_spelling_of_an_international_domain() {
        let raw = otp_message(
            Some("mx1.example.org; dmarc=pass header.from=xn--bnk-qla.example"),
            "Bank <otp@b\u{e4}nk.example>",
            "777777",
        );
        for filter in [
            "xn--bnk-qla.example",
            "b\u{e4}nk.example",
            "otp@xn--bnk-qla.example",
        ] {
            match classify(
                &raw,
                &mailbox("imap.example.org"),
                &ThreatConfig::default(),
                Some(filter),
                None,
            ) {
                Classified::Candidate(c) => assert_eq!(c.code, "777777", "{filter}"),
                other => panic!("--from {filter}: {other:?}"),
            }
        }
    }

    #[test]
    fn authenticated_wins_over_forged_in_window() {
        let mut collection = CandidateCollection::default();
        let admitted = collection.admit(vec![forged("111111")], false);
        assert_eq!(
            collection.observe(std::time::Duration::ZERO, admitted),
            CollectionOutcome::Continue
        );
        let admitted = collection.admit(vec![authenticated("222222")], false);
        assert_eq!(
            collection.observe(std::time::Duration::from_secs(5), admitted),
            CollectionOutcome::Continue
        );
        let admitted = collection.admit(vec![forged("333333")], false);
        match collection.observe(std::time::Duration::from_secs(10), admitted) {
            CollectionOutcome::Ready(c) => assert_eq!(c.code, "222222"),
            other => panic!("the authenticated code must win: {other:?}"),
        }
        let report = candidate_json(&collection.candidates[0], &collection);
        assert_eq!(report["sender_auth"]["result"], "pass");
        let rejected = report["rejected_candidates"].as_array().unwrap();
        assert_eq!(rejected.len(), 2);
        assert!(
            !report.to_string().contains("111111") && !report.to_string().contains("333333"),
            "a rejected code is never reported: {report}"
        );
    }

    #[test]
    fn two_authenticated_are_ambiguous() {
        let mut collection = CandidateCollection::default();
        let admitted = collection.admit(vec![authenticated("111111")], false);
        assert_eq!(
            collection.observe(std::time::Duration::ZERO, admitted),
            CollectionOutcome::Continue
        );
        let admitted = collection.admit(vec![authenticated("222222")], false);
        assert_eq!(
            collection.observe(std::time::Duration::from_secs(5), admitted),
            CollectionOutcome::Ambiguous(2)
        );
    }

    #[test]
    fn timeout_reports_sender_unverifiable() {
        let mut collection = CandidateCollection::default();
        let admitted = collection.admit(vec![unverifiable("111111")], false);
        assert_eq!(
            collection.observe(std::time::Duration::ZERO, admitted),
            CollectionOutcome::Continue
        );
        let report = collection.timeout_report(30);
        assert_eq!(report["error"], "sender_unverifiable", "{report}");
        assert_eq!(report["waited_seconds"], 30);
        let rejected = report["rejected_candidates"].as_array().unwrap();
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0]["from"], "otp@issuer.example");
        assert_eq!(rejected[0]["sender_auth"]["result"], "unverifiable");
        assert!(!report.to_string().contains("111111"), "{report}");

        let mut collection = CandidateCollection::default();
        collection.admit(vec![forged("222222")], false);
        assert_eq!(
            collection.timeout_report(30)["error"],
            "sender_unauthenticated"
        );

        let collection = CandidateCollection::default();
        assert_eq!(collection.timeout_report(30)["error"], "timeout");
    }

    #[test]
    fn plain_mode_requires_from() {
        assert!(binding_error(false, None, None).is_some());
        assert!(binding_error(false, None, Some("  ")).is_some());
        assert!(binding_error(false, None, Some("issuer.example")).is_none());
        assert!(binding_error(true, Some("account-1"), Some("issuer.example")).is_none());
        assert!(binding_error(true, None, Some("issuer.example")).is_some());
    }

    #[test]
    fn operator_opt_in_allows_unverifiable_and_reports_it() {
        let mut collection = CandidateCollection::default();
        let admitted = collection.admit(vec![unverifiable("555555"), forged("666666")], true);
        assert_eq!(admitted.len(), 1, "a failed sender never counts");
        assert_eq!(
            collection.observe(std::time::Duration::ZERO, admitted),
            CollectionOutcome::Continue
        );
        let ready = collection.observe(std::time::Duration::from_secs(5), Vec::new());
        let CollectionOutcome::Ready(candidate) = ready else {
            panic!("an opted-in unverifiable sender counts: {ready:?}");
        };
        assert_eq!(candidate.code, "555555");
        let report = candidate_json(&candidate, &collection);
        assert_eq!(report["sender_auth"]["result"], "unverifiable", "{report}");
        assert!(report["sender_auth"]["reason"].is_string(), "{report}");
    }

    #[test]
    fn sender_filter_is_exact_address_or_exact_domain() {
        assert!(sender_matches(
            "Trusted Name <otp@issuer.example>",
            "otp@issuer.example"
        ));
        assert!(sender_matches("otp@issuer.example", "issuer.example"));
        assert!(!sender_matches(
            "attacker@issuer.example.evil",
            "issuer.example"
        ));
        assert!(!sender_matches(
            "issuer.example <attacker@evil.test>",
            "issuer.example"
        ));
        assert!(!sender_matches("otp@issuer.example", "issuer"));
    }

    #[test]
    fn json_automation_rejects_empty_or_broad_bindings() {
        assert!(automation_binding_error(None, Some("issuer.example")).is_some());
        assert!(automation_binding_error(Some(""), Some("issuer.example")).is_some());
        assert!(automation_binding_error(Some("account-1"), None).is_some());
        assert!(automation_binding_error(Some("account-1"), Some("")).is_some());
        assert!(automation_binding_error(Some("account-1"), Some("issuer")).is_some());
        assert!(automation_binding_error(Some("account-1"), Some("otp@*.example")).is_some());
    }

    #[test]
    fn json_automation_accepts_exact_address_or_full_domain_with_account() {
        assert!(automation_binding_error(Some("account-1"), Some("otp@issuer.example")).is_none());
        assert!(automation_binding_error(Some("account-1"), Some("issuer.example")).is_none());
    }

    #[test]
    fn json_collection_waits_for_stabilization_across_polls() {
        let mut collection = CandidateCollection::default();
        assert_eq!(
            collection.observe(std::time::Duration::ZERO, [candidate("111111")]),
            CollectionOutcome::Continue
        );
        assert_eq!(
            collection.observe(std::time::Duration::from_secs(4), []),
            CollectionOutcome::Continue
        );
        assert_eq!(
            collection.observe(std::time::Duration::from_secs(5), []),
            CollectionOutcome::Ready(candidate("111111"))
        );
    }

    #[test]
    fn json_collection_fails_closed_for_candidates_from_separate_polls() {
        let mut collection = CandidateCollection::default();
        assert_eq!(
            collection.observe(std::time::Duration::ZERO, [candidate("111111")]),
            CollectionOutcome::Continue
        );
        assert_eq!(
            collection.observe(std::time::Duration::from_secs(5), [candidate("222222")]),
            CollectionOutcome::Ambiguous(2)
        );
    }

    #[test]
    fn json_collection_fails_closed_for_multiple_candidates_in_one_poll() {
        let mut collection = CandidateCollection::default();
        assert_eq!(
            collection.observe(
                std::time::Duration::ZERO,
                [candidate("111111"), candidate("222222")]
            ),
            CollectionOutcome::Ambiguous(2)
        );
    }

    /// One command the scripted server expects, by how it starts after the
    /// tag, and the lines it answers with (`{tag}` is the command's tag).
    struct Turn {
        command: String,
        reply: Vec<String>,
    }

    fn turn(command: &str, reply: &[&str]) -> Turn {
        Turn {
            command: command.to_string(),
            reply: reply.iter().map(|line| line.to_string()).collect(),
        }
    }

    const SEARCH_OK: &str = "{tag} OK Search completed";
    const REFUSED: &str = "{tag} NO [UNAVAILABLE] backend down";

    /// The turns `fetch_message_with_raw_in` takes to read message `uid`.
    fn read_turns(uid: u32, raw: &[u8]) -> Vec<Turn> {
        let raw = String::from_utf8(raw.to_vec()).unwrap();
        vec![
            turn("SELECT", &["{tag} OK [READ-WRITE] Select completed"]),
            turn(
                &format!("UID FETCH {uid} (UID RFC822.SIZE)"),
                &[
                    &format!("* 1 FETCH (UID {uid} RFC822.SIZE {})", raw.len()),
                    "{tag} OK Fetch completed",
                ],
            ),
            turn(
                &format!("UID FETCH {uid} (UID FLAGS BODY.PEEK[])"),
                &[
                    &format!(
                        "* 1 FETCH (UID {uid} FLAGS () BODY[] {{{}}}\r\n{raw})",
                        raw.len()
                    ),
                    "{tag} OK Fetch completed",
                ],
            ),
        ]
    }

    fn issuer_code(code: &str) -> Vec<u8> {
        otp_message(Some(ISSUER_PASS), "Issuer <otp@issuer.example>", code)
    }

    /// A logged-in session over an in-memory pipe whose server plays `turns`
    /// in order and answers anything else BAD. The server returns every
    /// command it was sent after LOGIN.
    async fn scripted_session(
        turns: Vec<Turn>,
    ) -> (
        async_imap::Session<tokio::io::DuplexStream>,
        tokio::task::JoinHandle<Vec<String>>,
    ) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let (read_half, mut write_half) = tokio::io::split(server_io);
            let mut reader = BufReader::new(read_half);
            write_half.write_all(b"* OK scripted\r\n").await.unwrap();
            let mut turns = turns.into_iter();
            let mut received = Vec::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                let (tag, command) = line.trim_end().split_once(' ').unwrap_or_default();
                let reply = if command.starts_with("LOGIN") {
                    vec!["{tag} OK LOGIN completed".to_string()]
                } else {
                    received.push(command.to_string());
                    match turns.next() {
                        Some(turn) if command.starts_with(&turn.command) => turn.reply,
                        _ => vec!["{tag} BAD unscripted command".to_string()],
                    }
                };
                let out: String = reply
                    .iter()
                    .map(|line| format!("{}\r\n", line.replace("{tag}", tag)))
                    .collect();
                if write_half.write_all(out.as_bytes()).await.is_err() {
                    break;
                }
            }
            received
        });
        let mut client = async_imap::Client::new(client_io);
        client.read_response().await.unwrap().unwrap();
        let session = client.login("me", "pw").await.map_err(|(e, _)| e).unwrap();
        (session, server)
    }

    /// Wait up to `timeout_secs` for a code from issuer.example to
    /// me@example.org, over the scripted `turns`. Returns how the wait ended
    /// and the commands the server was sent.
    async fn wait_scripted(
        turns: Vec<Turn>,
        json: bool,
        timeout_secs: u64,
    ) -> (Result<WaitOutcome>, Vec<String>) {
        let account = mailbox("imap.example.org");
        let config = ThreatConfig::default();
        let wait = CodeWait {
            account: &account,
            threat_config: &config,
            from_filter: Some("issuer.example"),
            subject_filter: None,
            allow_unverified: false,
            json,
            timeout: std::time::Duration::from_secs(timeout_secs),
        };
        let (mut session, server) = scripted_session(turns).await;
        let outcome = wait_for_code(&mut session, &wait, &mut CandidateCollection::default()).await;
        drop(session);
        (outcome, server.await.unwrap())
    }

    /// The error a refused read ends the wait with, as the CLI prints it. It
    /// names the account, the folder and the server's words.
    fn assert_refusal_reported(outcome: Result<WaitOutcome>) {
        let err = format!(
            "{:#}",
            outcome.expect_err("a refused read ends the wait with an error, never a code")
        );
        for expected in ["me@example.org", "INBOX", "NO [UNAVAILABLE] backend down"] {
            assert!(err.contains(expected), "missing {expected:?}: {err}");
        }
    }

    /// A refused `UID SEARCH ALL` read as an empty INBOX, so the wait started
    /// after UID 0 and took a code already in the mailbox for a new one: the
    /// agent got an old code. The refusal now ends the wait, and nothing more
    /// is read.
    #[tokio::test]
    async fn a_refused_search_never_returns_an_old_code() {
        let mut turns = vec![
            turn("UID SEARCH ALL", &[REFUSED]),
            // What a wait that started after UID 0 went on to read.
            turn("UID SEARCH UID 1:*", &["* SEARCH 3", SEARCH_OK]),
        ];
        turns.extend(read_turns(3, &issuer_code("111111")));

        let (outcome, commands) = wait_scripted(turns, false, 60).await;

        assert_refusal_reported(outcome);
        assert_eq!(
            commands,
            ["UID SEARCH ALL"],
            "nothing is read after the refusal"
        );
    }

    /// JSON automation waits out the stabilization window so that a second
    /// code arriving in it fails closed as ambiguous. A search refused in the
    /// window read as nothing new, and the first code was released though
    /// another had come. The refusal now ends the wait.
    #[tokio::test(start_paused = true)]
    async fn a_refused_search_in_the_stabilization_window_never_releases_the_first_code() {
        let mut turns = vec![
            turn("UID SEARCH ALL", &["* SEARCH 5", SEARCH_OK]),
            turn("UID SEARCH UID 6:*", &["* SEARCH 6", SEARCH_OK]),
        ];
        turns.extend(read_turns(6, &issuer_code("111111")));
        // A second code has arrived as UID 7; the search that would find it
        // is refused.
        turns.push(turn("UID SEARCH UID 7:*", &[REFUSED]));

        let (outcome, _) = wait_scripted(turns, true, 60).await;

        assert_refusal_reported(outcome);
    }

    /// A FETCH the server refused is an error, never a message that is gone:
    /// skipping it would return the older code that arrived beside it.
    #[tokio::test]
    async fn a_refused_fetch_never_returns_an_old_code() {
        let mut turns = vec![
            turn("UID SEARCH ALL", &["* SEARCH 5", SEARCH_OK]),
            turn("UID SEARCH UID 6:*", &["* SEARCH 6 7", SEARCH_OK]),
        ];
        turns.extend(read_turns(6, &issuer_code("111111")));
        turns.push(turn("SELECT", &["{tag} OK [READ-WRITE] Select completed"]));
        turns.push(turn("UID FETCH 7 (UID RFC822.SIZE)", &[REFUSED]));

        let (outcome, _) = wait_scripted(turns, false, 60).await;

        assert_refusal_reported(outcome);
    }

    /// No new mail yet is still a wait: a poll every five seconds until the
    /// wait runs out, then a timeout. An empty SEARCH, and `n:*` answering
    /// with the highest UID already seen, both mean nothing new.
    #[tokio::test(start_paused = true)]
    async fn no_new_mail_still_waits_and_times_out() {
        let turns = vec![
            turn("UID SEARCH ALL", &["* SEARCH 5", SEARCH_OK]),
            turn("UID SEARCH UID 6:*", &["* SEARCH", SEARCH_OK]),
            turn("UID SEARCH UID 6:*", &["* SEARCH 5", SEARCH_OK]),
        ];
        let started = tokio::time::Instant::now();

        let (outcome, commands) = wait_scripted(turns, false, 10).await;

        assert_eq!(outcome.unwrap(), WaitOutcome::TimedOut);
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(10));
        assert_eq!(
            commands,
            ["UID SEARCH ALL", "UID SEARCH UID 6:*", "UID SEARCH UID 6:*"]
        );
    }

    #[test]
    fn timeout_before_stabilization_never_returns_a_candidate() {
        let mut collection = CandidateCollection::default();
        assert_eq!(
            collection.observe(std::time::Duration::ZERO, [candidate("111111")]),
            CollectionOutcome::Continue
        );
        // The run loop checks this timeout before another poll, so a 4-second
        // request cannot release a candidate that requires five seconds to stabilize.
        assert!(std::time::Duration::from_secs(4) < AUTOMATION_STABILIZATION_WINDOW);
    }
}
