// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! `threat report`: a draft to `threat.report_to` with the original message
//! attached unmodified as `message/rfc822`, copied to up to two RDAP abuse
//! contacts: the registrar of the domain that sent it (the takedown target)
//! and, when the sender analyzer found a look-alike, the registrar of the
//! impersonated domain (brand protection). Recipients are deduped; a failed
//! lookup drops only its own recipient.
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

/// Why a domain's abuse contact was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AbuseRole {
    /// The From domain: the look-alike's registrar can take it down.
    Sender,
    /// The known domain the sender imitates: its registrar handles brand
    /// abuse.
    Impersonated,
}

impl AbuseRole {
    pub fn as_str(self) -> &'static str {
        match self {
            AbuseRole::Sender => "sender",
            AbuseRole::Impersonated => "impersonated",
        }
    }
}

/// The outcome of one RDAP abuse-contact lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AbuseOutcome {
    Found {
        email: String,
    },
    /// RDAP gave no usable answer; this recipient is left out.
    Failed {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AbuseContact {
    pub role: AbuseRole,
    pub domain: String,
    #[serde(flatten)]
    pub outcome: AbuseOutcome,
}

/// Registrable domains to ask RDAP about, in recipient order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReportTargets {
    pub sender: Option<String>,
    pub impersonated: Option<String>,
}

/// The From domain always; the impersonated domain only when the verdict
/// carries the sender analyzer's `lookalike_domain` signal (recomputed from
/// the message and ledger rather than parsed out of evidence text).
pub fn report_targets(input: &ThreatInput, verdict: Option<&ThreatVerdict>) -> ReportTargets {
    let sender = input
        .from_domain()
        .map(|d| registrable(&d))
        .filter(|d| !d.is_empty());
    let named = verdict.is_some_and(|v| v.signals.iter().any(|s| s.code == "lookalike_domain"));
    let impersonated = named
        .then(|| sender::impersonated_domain(input))
        .flatten()
        .map(|d| registrable(&d))
        .filter(|d| !d.is_empty() && Some(d) != sender.as_ref());
    ReportTargets {
        sender,
        impersonated,
    }
}

/// Ask RDAP for each target's abuse contact. The records are for
/// `persist::record_lookups`.
pub async fn resolve_abuse_contacts<F: RdapFetch>(
    fetch: &F,
    targets: &ReportTargets,
) -> (Vec<AbuseContact>, Vec<LookupRecord>) {
    let mut contacts = Vec::new();
    let mut lookups = Vec::new();
    for (role, domain) in [
        (AbuseRole::Sender, &targets.sender),
        (AbuseRole::Impersonated, &targets.impersonated),
    ] {
        let Some(domain) = domain else { continue };
        let found = rdap::abuse_contact(fetch, domain).await;
        lookups.extend(found.lookups);
        contacts.push(AbuseContact {
            role,
            domain: domain.clone(),
            outcome: match found.result {
                Ok(email) => AbuseOutcome::Found { email },
                Err(reason) => AbuseOutcome::Failed { reason },
            },
        });
    }
    (contacts, lookups)
}

/// Recipients of a report draft, comma-separated as drafts store them:
/// `report_to`, then each found abuse contact, case-insensitively deduped.
fn recipients(report_to: &str, abuse: &[AbuseContact]) -> String {
    let mut out: Vec<String> = vec![report_to.to_string()];
    for contact in abuse {
        if let AbuseOutcome::Found { email } = &contact.outcome
            && !out.iter().any(|r| r.eq_ignore_ascii_case(email))
        {
            out.push(email.clone());
        }
    }
    out.join(", ")
}

pub fn build_report(
    original: &[u8],
    verdict: Option<&ThreatVerdict>,
    report_to: &str,
    abuse: &[AbuseContact],
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
    for contact in abuse {
        let AbuseOutcome::Found { email } = &contact.outcome else {
            continue;
        };
        let domain = &contact.domain;
        body.push_str(&match contact.role {
            AbuseRole::Sender => format!(
                "\nThis message was sent from {domain}. {email} is the abuse contact in that \
                 domain's registration data (RDAP).\n"
            ),
            AbuseRole::Impersonated => format!(
                "\nThis message impersonates {domain}. {email} is the abuse contact in that \
                 domain's registration data (RDAP).\n"
            ),
        });
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
        let report = build_report(RAW, Some(&verdict()), "reportphishing@apwg.org", &[]);
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
        let report = build_report(RAW, None, "reportphishing@apwg.org", &[]);
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

    fn found(role: AbuseRole, domain: &str, email: &str) -> AbuseContact {
        AbuseContact {
            role,
            domain: domain.into(),
            outcome: AbuseOutcome::Found {
                email: email.into(),
            },
        }
    }

    fn failed(role: AbuseRole, domain: &str) -> AbuseContact {
        AbuseContact {
            role,
            domain: domain.into(),
            outcome: AbuseOutcome::Failed {
                reason: "registry RDAP: HTTP 503".into(),
            },
        }
    }

    #[test]
    fn both_abuse_contacts_join_apwg_on_the_draft() {
        let abuse = [
            found(
                AbuseRole::Sender,
                "examp1e.org",
                "abuse@cheap-registrar.example",
            ),
            found(
                AbuseRole::Impersonated,
                "example.org",
                "abuse@brand-registrar.example",
            ),
        ];
        let report = build_report(RAW, Some(&verdict()), "reportphishing@apwg.org", &abuse);
        let three = "reportphishing@apwg.org, abuse@cheap-registrar.example, \
                     abuse@brand-registrar.example";
        assert_eq!(report.to, three);
        assert!(report.body.contains("sent from examp1e.org"));
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
            vec![
                "reportphishing@apwg.org",
                "abuse@cheap-registrar.example",
                "abuse@brand-registrar.example"
            ]
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
        assert_eq!(draft.to_addr, three);
        assert!(draft.sent_at.is_none());
    }

    #[test]
    fn same_registrar_for_both_domains_is_one_recipient() {
        let abuse = [
            found(AbuseRole::Sender, "examp1e.org", "Abuse@MarkMonitor.com"),
            found(
                AbuseRole::Impersonated,
                "example.org",
                "abuse@markmonitor.com",
            ),
        ];
        let report = build_report(RAW, Some(&verdict()), "reportphishing@apwg.org", &abuse);
        assert_eq!(report.to, "reportphishing@apwg.org, Abuse@MarkMonitor.com");
    }

    #[test]
    fn a_failed_lookup_drops_only_its_own_recipient() {
        let abuse = [
            failed(AbuseRole::Sender, "examp1e.org"),
            found(
                AbuseRole::Impersonated,
                "example.org",
                "abuse@brand-registrar.example",
            ),
        ];
        let report = build_report(RAW, Some(&verdict()), "reportphishing@apwg.org", &abuse);
        assert_eq!(
            report.to,
            "reportphishing@apwg.org, abuse@brand-registrar.example"
        );
        assert!(!report.body.contains("sent from"));

        let report = build_report(
            RAW,
            Some(&verdict()),
            "reportphishing@apwg.org",
            &[
                failed(AbuseRole::Sender, "examp1e.org"),
                failed(AbuseRole::Impersonated, "example.org"),
            ],
        );
        assert_eq!(report.to, "reportphishing@apwg.org");
    }

    #[test]
    fn targets_are_the_sender_always_and_the_brand_only_when_named() {
        let mut input = ThreatInput::from_raw(RAW, "me@example.org").unwrap();
        input.ledger = Ok(Default::default());
        assert_eq!(
            report_targets(&input, Some(&verdict())),
            ReportTargets {
                sender: Some("examp1e.org".into()),
                impersonated: Some("example.org".into()),
            }
        );
        let no_lookalike = combine(
            vec![Signal::new("dmarc_fail", 30, "dmarc=fail")],
            vec![],
            vec![],
            false,
        );
        let sender_only = ReportTargets {
            sender: Some("examp1e.org".into()),
            impersonated: None,
        };
        assert_eq!(report_targets(&input, Some(&no_lookalike)), sender_only);
        assert_eq!(report_targets(&input, None), sender_only);
    }

    /// Serves the IANA bootstrap and one registry answer per domain.
    struct Registry(std::collections::HashMap<String, Result<serde_json::Value, String>>);

    impl RdapFetch for Registry {
        async fn get_json(&self, url: &str) -> Result<serde_json::Value, String> {
            if url == rdap::IANA_DNS_BOOTSTRAP {
                return Ok(json!({"services": [
                    [["org"], ["https://rdap.registry.example/"]]
                ]}));
            }
            self.0
                .get(url)
                .cloned()
                .unwrap_or_else(|| Err(format!("unexpected fetch {url}")))
        }
    }

    fn abuse_doc(email: &str) -> serde_json::Value {
        json!({"entities": [{"roles": ["registrar"], "entities": [{"roles": ["abuse"],
            "vcardArray": ["vcard", [["email", {}, "text", email]]]}]}]})
    }

    #[tokio::test]
    async fn both_lookups_run_and_are_audited() {
        let fetch = Registry(
            [
                (
                    "https://rdap.registry.example/domain/examp1e.org".to_string(),
                    Ok(abuse_doc("abuse@cheap-registrar.example")),
                ),
                (
                    "https://rdap.registry.example/domain/example.org".to_string(),
                    Ok(abuse_doc("abuse@brand-registrar.example")),
                ),
            ]
            .into(),
        );
        let targets = ReportTargets {
            sender: Some("examp1e.org".into()),
            impersonated: Some("example.org".into()),
        };
        let (contacts, lookups) = resolve_abuse_contacts(&fetch, &targets).await;
        assert_eq!(
            contacts,
            vec![
                found(
                    AbuseRole::Sender,
                    "examp1e.org",
                    "abuse@cheap-registrar.example"
                ),
                found(
                    AbuseRole::Impersonated,
                    "example.org",
                    "abuse@brand-registrar.example"
                ),
            ]
        );
        let audited: Vec<&str> = lookups.iter().map(|l| l.domain.as_str()).collect();
        assert_eq!(audited, vec!["examp1e.org", "example.org"]);
        assert!(lookups.iter().all(|l| l.provider == "rdap"));
        assert_eq!(
            serde_json::to_value(&contacts[0]).unwrap(),
            json!({"role": "sender", "domain": "examp1e.org", "status": "found",
                   "email": "abuse@cheap-registrar.example"})
        );
    }

    #[tokio::test]
    async fn partial_failure_keeps_the_other_contact_and_names_the_failure() {
        let fetch = Registry(
            [
                (
                    "https://rdap.registry.example/domain/examp1e.org".to_string(),
                    Err("HTTP 503".to_string()),
                ),
                (
                    "https://rdap.registry.example/domain/example.org".to_string(),
                    Ok(abuse_doc("abuse@brand-registrar.example")),
                ),
            ]
            .into(),
        );
        let targets = ReportTargets {
            sender: Some("examp1e.org".into()),
            impersonated: Some("example.org".into()),
        };
        let (contacts, lookups) = resolve_abuse_contacts(&fetch, &targets).await;
        assert_eq!(contacts[0].role, AbuseRole::Sender);
        assert!(
            matches!(&contacts[0].outcome, AbuseOutcome::Failed { reason } if reason.contains("503"))
        );
        assert_eq!(
            contacts[1],
            found(
                AbuseRole::Impersonated,
                "example.org",
                "abuse@brand-registrar.example"
            )
        );
        assert_eq!(lookups.len(), 2, "the failed query is audited too");
        assert_eq!(lookups[0].result, "error: HTTP 503");
    }

    #[tokio::test]
    async fn no_targets_asks_nothing() {
        struct Panics;
        impl RdapFetch for Panics {
            async fn get_json(&self, url: &str) -> Result<serde_json::Value, String> {
                panic!("fetched {url}")
            }
        }
        let (contacts, lookups) = resolve_abuse_contacts(&Panics, &ReportTargets::default()).await;
        assert!(contacts.is_empty() && lookups.is_empty());
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
        let report = build_report(RAW, Some(&verdict()), "reportphishing@apwg.org", &[]);
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
