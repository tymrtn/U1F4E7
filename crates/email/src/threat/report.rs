// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! `threat report`: a draft to `threat.report_to` with the original message
//! attached unmodified as `message/rfc822`, copied to the RDAP abuse contact
//! of the domain the sender impersonates when the sender analyzer named one
//! and RDAP answered.
//!
//! Draft only. Nothing here sends; the draft goes out through
//! `envelope draft send` (or the dashboard's send), which runs the Governor
//! gate like every other outbound message.

use anyhow::{Context, Result, anyhow};
use envelope_email_store::{Database, Draft};
use serde::Serialize;
use serde_json::json;

use super::domains::registrable;
use super::rdap::{self, RdapFetch};
use super::{LookupRecord, ThreatInput, ThreatVerdict, sender};
use crate::imap::{self, ImapClient};

/// Local draft `created_by` for report drafts.
pub const REPORT_CREATED_BY: &str = "envelope:threat";

#[derive(Debug, Clone, PartialEq)]
pub struct ReportDraft {
    pub to: String,
    pub subject: String,
    pub body: String,
    /// Draft attachment snapshot: `filename`, `content_type`, `size`,
    /// `data_base64`.
    pub attachment: serde_json::Value,
}

/// What the report learned about the impersonated domain's abuse contact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AbuseContact {
    /// The sender analyzer named no impersonated domain; nothing was asked.
    NotApplicable,
    Found {
        domain: String,
        email: String,
    },
    /// RDAP was asked and gave no usable answer; the report goes to
    /// `threat.report_to` only.
    Failed {
        domain: String,
        reason: String,
    },
}

/// The registrable domain to look up: only when the verdict carries the
/// sender analyzer's `lookalike_domain` signal, recomputed from the message
/// and ledger rather than parsed out of evidence text.
pub fn impersonated_domain(input: &ThreatInput, verdict: Option<&ThreatVerdict>) -> Option<String> {
    let named = verdict?
        .signals
        .iter()
        .any(|s| s.code == "lookalike_domain");
    if !named {
        return None;
    }
    sender::impersonated_domain(input)
        .map(|d| registrable(&d))
        .filter(|d| !d.is_empty())
}

/// Ask RDAP for `domain`'s abuse contact. The records are for
/// `persist::record_lookups`.
pub async fn resolve_abuse_contact<F: RdapFetch>(
    fetch: &F,
    domain: Option<String>,
) -> (AbuseContact, Vec<LookupRecord>) {
    let Some(domain) = domain else {
        return (AbuseContact::NotApplicable, Vec::new());
    };
    let found = rdap::abuse_contact(fetch, &domain).await;
    let contact = match found.result {
        Ok(email) => AbuseContact::Found { domain, email },
        Err(reason) => AbuseContact::Failed { domain, reason },
    };
    (contact, found.lookups)
}

/// Recipients of a report draft, comma-separated as drafts store them.
fn recipients(report_to: &str, abuse: &AbuseContact) -> String {
    match abuse {
        AbuseContact::Found { email, .. } if !email.eq_ignore_ascii_case(report_to) => {
            format!("{report_to}, {email}")
        }
        _ => report_to.to_string(),
    }
}

pub fn build_report(
    original: &[u8],
    verdict: Option<&ThreatVerdict>,
    report_to: &str,
    abuse: &AbuseContact,
) -> ReportDraft {
    use base64::Engine as _;
    let mut body = String::from(
        "I am reporting the attached message as phishing. The original is attached \
         unmodified as message/rfc822 with its full headers.\n",
    );
    if let Some(v) = verdict {
        body.push_str(&format!(
            "\nLocal analysis (Envelope {}): {} ({}/100)\n",
            v.engine_version,
            v.level.as_str(),
            v.score
        ));
        for s in &v.signals {
            body.push_str(&format!("- {}: {}\n", s.code, s.evidence));
        }
    }
    if let AbuseContact::Found { domain, email } = abuse {
        body.push_str(&format!(
            "\nThis message impersonates {domain}. {email} is that domain's abuse contact \
             in its registration data (RDAP).\n"
        ));
    }
    let subject = ThreatInput::from_raw(original, "")
        .ok()
        .and_then(|i| {
            i.headers
                .into_iter()
                .find(|(n, _)| n.eq_ignore_ascii_case("subject"))
                .map(|(_, v)| v)
        })
        .map(|s| {
            format!(
                "Phishing report: {}",
                s.chars().take(120).collect::<String>()
            )
        })
        .unwrap_or_else(|| "Phishing report".to_string());
    ReportDraft {
        to: recipients(report_to, abuse),
        subject,
        body,
        attachment: json!({
            "filename": "original.eml",
            "content_type": "message/rfc822",
            "size": original.len(),
            "data_base64": base64::engine::general_purpose::STANDARD.encode(original),
        }),
    }
}

fn original_bytes(report: &ReportDraft) -> Result<Vec<u8>> {
    use base64::Engine as _;
    let b64 = report.attachment["data_base64"]
        .as_str()
        .ok_or_else(|| anyhow!("report attachment has no data_base64"))?;
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("report attachment is not valid base64")
}

/// RFC 822 bytes of the report draft plus its generated Message-ID
/// (bracketed).
pub fn report_rfc822(
    from_name: Option<&str>,
    from_addr: &str,
    report: &ReportDraft,
) -> Result<(Vec<u8>, String)> {
    let from = match from_name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => mail_builder::headers::address::Address::new_address(Some(name), from_addr),
        None => mail_builder::headers::address::Address::new_address(None::<&str>, from_addr),
    };
    let message_id = format!("{}@envelope.threat", uuid::Uuid::new_v4());
    let rfc822 = mail_builder::MessageBuilder::new()
        .from(from)
        .to(report
            .to
            .split(',')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .collect::<Vec<&str>>())
        .subject(report.subject.as_str())
        .message_id(message_id.as_str())
        .text_body(report.body.as_str())
        .attachment("message/rfc822", "original.eml", original_bytes(report)?)
        .write_to_vec()
        .context("failed to build the report draft")?;
    Ok((
        crate::compose::normalize_crlf(&rfc822),
        format!("<{message_id}>"),
    ))
}

/// APPEND the report to the Drafts folder and find its UID.
pub async fn append_report_draft(
    client: &mut ImapClient,
    drafts_folder: &str,
    rfc822: &[u8],
    message_id: &str,
) -> Result<Option<u32>> {
    imap::append_message(client, drafts_folder, "(\\Draft \\Seen)", rfc822)
        .await
        .with_context(|| format!("appending the report draft to '{drafts_folder}'"))?;
    let bare = message_id.trim_matches(|c| c == '<' || c == '>');
    Ok(
        imap::find_unique_uid_by_exact_message_id(client, drafts_folder, bare)
            .await
            .unwrap_or(None),
    )
}

/// The local record of a report draft already appended to `folder`.
pub fn record_report_draft(
    db: &Database,
    account_id: &str,
    report: &ReportDraft,
    folder: &str,
    imap_uid: Option<u32>,
    message_id: &str,
) -> Result<Draft> {
    let draft = db
        .create_draft(
            account_id,
            &report.to,
            Some(&report.subject),
            Some(&report.body),
            None,
            None,
            None,
            None,
            Some(REPORT_CREATED_BY),
        )
        .context("failed to create local draft record")?;
    if let Some(uid) = imap_uid {
        db.update_draft_imap_uid(&draft.id, uid)
            .context("failed to store the report draft's IMAP UID")?;
    }
    if !message_id.is_empty() {
        db.mark_draft_message_id(&draft.id, message_id)
            .context("failed to store the report draft's Message-ID")?;
    }
    db.set_draft_metadata(
        &draft.id,
        &json!({
            "agent_body_text": report.body,
            "agent_body_html": null,
            "signature_applied": false,
            "threat_report": true,
            "storage": {
                "imap_synced": true,
                "imap_folder": folder,
                "local_only": false,
            }
        }),
    )
    .context("failed to persist report draft metadata")?;
    db.update_draft_attachments(&draft.id, std::slice::from_ref(&report.attachment))
        .context("failed to persist the report's original-message attachment")?;
    db.get_draft(&draft.id)
        .context("failed to reload the report draft")?
        .ok_or_else(|| anyhow!("report draft {} vanished after creation", draft.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::threat::{Signal, combine};
    use envelope_email_store::DraftStatus;

    const RAW: &[u8] = b"Message-ID: <p@x>\r\nFrom: it@examp1e.org\r\nSubject: Password expiry\r\n\r\nverify your account\r\n";

    fn verdict() -> ThreatVerdict {
        combine(
            vec![Signal::new(
                "lookalike_domain",
                45,
                "sender domain examp1e.org imitates example.org",
            )],
            vec!["sender".into()],
            vec![],
            false,
        )
    }

    #[test]
    fn report_attaches_the_original_unmodified_and_lists_evidence() {
        let report = build_report(
            RAW,
            Some(&verdict()),
            "reportphishing@apwg.org",
            &AbuseContact::NotApplicable,
        );
        assert_eq!(report.to, "reportphishing@apwg.org");
        assert_eq!(report.subject, "Phishing report: Password expiry");
        assert_eq!(report.attachment["content_type"], "message/rfc822");
        assert_eq!(original_bytes(&report).unwrap(), RAW, "byte-exact original");
        assert!(report.body.contains("lookalike_domain"));
        assert!(
            !report.body.contains("verify your account"),
            "body not quoted"
        );
    }

    #[test]
    fn report_rfc822_carries_a_message_rfc822_part() {
        let report = build_report(
            RAW,
            None,
            "reportphishing@apwg.org",
            &AbuseContact::NotApplicable,
        );
        let (bytes, mid) = report_rfc822(Some("Me"), "me@example.org", &report).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("message/rfc822"));
        assert!(
            text.contains("To: <reportphishing@apwg.org>")
                || text.contains("To: reportphishing@apwg.org")
        );
        assert!(mid.starts_with('<') && mid.ends_with("@envelope.threat>"));
        let parsed = mail_parser::MessageParser::default().parse(&bytes).unwrap();
        assert_eq!(parsed.attachment_count(), 1);
    }

    #[test]
    fn found_abuse_contact_is_a_second_recipient_on_the_draft() {
        let abuse = AbuseContact::Found {
            domain: "example.org".into(),
            email: "abuse@registrar.example".into(),
        };
        let report = build_report(RAW, Some(&verdict()), "reportphishing@apwg.org", &abuse);
        assert_eq!(
            report.to,
            "reportphishing@apwg.org, abuse@registrar.example"
        );
        assert!(report.body.contains("impersonates example.org"));

        let (bytes, _) = report_rfc822(None, "me@example.org", &report).unwrap();
        let parsed = mail_parser::MessageParser::default().parse(&bytes).unwrap();
        let to: Vec<&str> = parsed
            .to()
            .unwrap()
            .iter()
            .filter_map(|a| a.address.as_deref())
            .collect();
        assert_eq!(
            to,
            vec!["reportphishing@apwg.org", "abuse@registrar.example"]
        );

        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acct-1', 'Me', 'me@example.org', 'example.org',
                         'smtp.example.org', 587, 'imap.example.org', 993, 'encrypted')",
                [],
            )
            .unwrap();
        let draft = record_report_draft(&db, "acct-1", &report, "Drafts", None, "<r@x>").unwrap();
        assert_eq!(
            draft.to_addr,
            "reportphishing@apwg.org, abuse@registrar.example"
        );
        assert!(draft.sent_at.is_none());
    }

    #[test]
    fn failed_or_absent_rdap_keeps_apwg_only() {
        for abuse in [
            AbuseContact::NotApplicable,
            AbuseContact::Failed {
                domain: "example.org".into(),
                reason: "registry RDAP: HTTP 503".into(),
            },
        ] {
            let report = build_report(RAW, Some(&verdict()), "reportphishing@apwg.org", &abuse);
            assert_eq!(report.to, "reportphishing@apwg.org");
            assert!(!report.body.contains("impersonates"));
        }
    }

    #[test]
    fn rdap_is_asked_only_when_the_sender_analyzer_named_a_domain() {
        let mut input = ThreatInput::from_raw(RAW, "me@example.org").unwrap();
        input.ledger = Ok(Default::default());
        assert_eq!(
            impersonated_domain(&input, Some(&verdict())).as_deref(),
            Some("example.org")
        );
        let no_lookalike = combine(
            vec![Signal::new("dmarc_fail", 30, "dmarc=fail")],
            vec![],
            vec![],
            false,
        );
        assert_eq!(impersonated_domain(&input, Some(&no_lookalike)), None);
        assert_eq!(impersonated_domain(&input, None), None);
    }

    #[tokio::test]
    async fn no_impersonated_domain_asks_nothing() {
        struct Panics;
        impl RdapFetch for Panics {
            async fn get_json(&self, url: &str) -> Result<serde_json::Value, String> {
                panic!("fetched {url}")
            }
        }
        let (contact, lookups) = resolve_abuse_contact(&Panics, None).await;
        assert_eq!(contact, AbuseContact::NotApplicable);
        assert!(lookups.is_empty());
    }

    #[test]
    fn report_creates_a_local_draft_and_sends_nothing() {
        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acct-1', 'Me', 'me@example.org', 'example.org',
                         'smtp.example.org', 587, 'imap.example.org', 993, 'encrypted')",
                [],
            )
            .unwrap();
        let report = build_report(
            RAW,
            Some(&verdict()),
            "reportphishing@apwg.org",
            &AbuseContact::NotApplicable,
        );
        let draft =
            record_report_draft(&db, "acct-1", &report, "Drafts", Some(12), "<r@x>").unwrap();
        assert_eq!(draft.status, DraftStatus::Draft);
        assert!(draft.sent_at.is_none());
        assert!(draft.send_after.is_none(), "never queued");
        assert_eq!(draft.to_addr, "reportphishing@apwg.org");
        assert_eq!(draft.attachments.len(), 1);
        assert_eq!(draft.attachments[0]["content_type"], "message/rfc822");
        assert_eq!(draft.attachments[0]["filename"], "original.eml");
    }
}
