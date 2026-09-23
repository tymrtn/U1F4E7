// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! `threat report`: a draft to `threat.report_to` with the original message
//! attached unmodified as `message/rfc822`.
//!
//! Draft only. Nothing here sends; the draft goes out through
//! `envelope draft send` (or the dashboard's send), which runs the Governor
//! gate like every other outbound message.

use anyhow::{Context, Result, anyhow};
use envelope_email_store::{Database, Draft};
use serde_json::json;

use super::{ThreatInput, ThreatVerdict};
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

pub fn build_report(
    original: &[u8],
    verdict: Option<&ThreatVerdict>,
    report_to: &str,
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
        to: report_to.to_string(),
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
        .to(report.to.as_str())
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
        let report = build_report(RAW, Some(&verdict()), "reportphishing@apwg.org");
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
        let report = build_report(RAW, None, "reportphishing@apwg.org");
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
        let report = build_report(RAW, Some(&verdict()), "reportphishing@apwg.org");
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
