// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! `envelope code` — poll IMAP for a verification code and extract it.

use anyhow::{Context, Result, bail};
use envelope_email_store::credential_store::CredentialBackend;
use envelope_email_transport::code_extractor::extract_code;
use envelope_email_transport::imap;
use envelope_email_transport::threat::ThreatInput;
use envelope_email_transport::threat::auth_results::{SenderAuth, sender_auth};
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
/// for the very domain in its From header.
fn classify(
    raw: &[u8],
    account: &str,
    from_filter: Option<&str>,
    subject_filter: Option<&str>,
) -> Classified {
    let Ok(input) = ThreatInput::from_raw(raw, account) else {
        return Classified::NoMatch;
    };
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
        if input.from_domain().as_deref() == Some(domain.as_str()));
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
    let mut client = imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;
    client
        .session_mut()
        .select("INBOX")
        .await
        .map_err(|e| anyhow::anyhow!("SELECT INBOX: {e}"))?;
    let initial_max_uid = get_max_uid(&mut client).await?;
    if !json {
        eprintln!(
            "Watching for verification codes (timeout: {wait_secs}s, starting after UID {initial_max_uid})..."
        );
    }

    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(wait_secs);
    let mut last_seen_uid = initial_max_uid;
    let mut collected = CandidateCollection::default();
    loop {
        let elapsed = start.elapsed();
        if elapsed >= timeout {
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

        let new_uids = search_new_uids(&mut client, last_seen_uid).await?;
        let mut classified = Vec::new();
        for uid in new_uids {
            if uid <= last_seen_uid {
                continue;
            }
            last_seen_uid = uid;
            // `BODY.PEEK[]`: reading never marks the message seen. A message
            // too large to fetch whole has no bytes to authenticate and is
            // skipped.
            let Some((_, Some(raw))) =
                imap::fetch_message_with_raw(&mut client, "INBOX", uid).await?
            else {
                continue;
            };
            classified.push(classify(
                &raw,
                &creds.account.username,
                from_filter,
                subject_filter,
            ));
        }
        let matches = collected.admit(classified, allow_unverified);

        if json {
            match collected.observe(elapsed, matches) {
                CollectionOutcome::Continue => {}
                CollectionOutcome::Ready(candidate) => {
                    let output = candidate_json(&candidate, &collected);
                    println!("{}", serde_json::to_string_pretty(&output)?);
                    return Ok(());
                }
                CollectionOutcome::Ambiguous(candidate_count) => {
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
            }
        } else {
            // Interactive stdout use stays low-friction: no stabilization
            // window, but the same sender authentication.
            match matches.as_slice() {
                [] => {}
                [candidate] => {
                    println!("{}", candidate.code);
                    return Ok(());
                }
                _ => bail!(
                    "ambiguous OTP matches: {} messages matched this poll; refine --from or --subject",
                    matches.len()
                ),
            }
        }

        let remaining = timeout.saturating_sub(start.elapsed());
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
        return !local.is_empty() && !local.contains('@') && is_fully_qualified_domain(domain);
    }
    is_fully_qualified_domain(filter.trim_start_matches('@'))
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
/// identity or a whole domain. Display names are never candidates.
fn sender_matches(raw_sender: &str, raw_filter: &str) -> bool {
    let sender = mailbox_from_header(raw_sender);
    let filter = raw_filter
        .trim()
        .trim_matches('<')
        .trim_matches('>')
        .to_lowercase();
    if sender.is_empty() || filter.is_empty() {
        return false;
    }
    if filter.contains('@') && !filter.starts_with('@') {
        return sender == filter;
    }
    let domain = filter.trim_start_matches('@');
    !domain.is_empty()
        && sender
            .rsplit_once('@')
            .is_some_and(|(_, value)| value == domain)
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

async fn get_max_uid(client: &mut imap::ImapClient) -> Result<u32> {
    let uid_set = client
        .session_mut()
        .uid_search("ALL")
        .await
        .map_err(|e| anyhow::anyhow!("UID SEARCH ALL: {e}"))?;
    Ok(uid_set.into_iter().max().unwrap_or(0))
}

async fn search_new_uids(client: &mut imap::ImapClient, since_uid: u32) -> Result<Vec<u32>> {
    let query = format!("UID {}:*", since_uid + 1);
    let uid_set = client
        .session_mut()
        .uid_search(&query)
        .await
        .map_err(|e| anyhow::anyhow!("UID SEARCH {query}: {e}"))?;
    let mut uids: Vec<u32> = uid_set.into_iter().collect();
    uids.sort_unstable();
    Ok(uids)
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

    fn classify_issuer(raw: &[u8]) -> Classified {
        classify(raw, "me@example.org", Some("issuer.example"), None)
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
