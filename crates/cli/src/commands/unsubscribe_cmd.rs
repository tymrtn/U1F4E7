// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result};
use envelope_email_store::credential_store::CredentialBackend;
use envelope_email_transport::imap;
use envelope_email_transport::smtp::SmtpSender;
use envelope_email_transport::unsubscribe;

use super::common::setup_credentials;

/// `envelope unsubscribe <uid>` — parse List-Unsubscribe and optionally execute.
///
/// Default is dry-run: shows what it would do. Pass `--confirm` to execute.
/// For mailto fallback, sends an empty unsubscribe email via SMTP.
#[tokio::main]
#[allow(clippy::too_many_arguments)]
pub async fn run(
    uid: u32,
    folder: &str,
    account: Option<&str>,
    confirm: bool,
    attr: &[String],
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;

    let mut client = imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    // Fetch message summary for display
    let msg = imap::fetch_message(&mut client, folder, uid)
        .await
        .context("failed to fetch message")?
        .ok_or_else(|| anyhow::anyhow!("message UID {uid} not found in {folder}"))?;

    // Fetch List-Unsubscribe headers (separate fetch for raw headers)
    let (list_unsub, list_unsub_post) =
        imap::fetch_list_unsubscribe_headers(&mut client, folder, uid)
            .await
            .context("failed to fetch List-Unsubscribe headers")?;

    let list_unsub_str = match &list_unsub {
        Some(h) => h.as_str(),
        None => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "uid": uid,
                        "folder": folder,
                        "subject": msg.subject,
                        "from": msg.from_addr,
                        "status": "no_header",
                        "message": "No List-Unsubscribe header found",
                    })
                );
            } else {
                println!("UID {uid} ({folder})");
                println!("  From:    {}", msg.from_addr);
                println!("  Subject: {}", msg.subject);
                println!();
                println!("No List-Unsubscribe header found in this message.");
                println!("This sender does not support automated unsubscribe.");
            }
            // A confirmed unsubscribe against a message with no unsubscribe
            // header has no supported action: fail nonzero, do not exit success.
            if confirm {
                anyhow::bail!(
                    "no List-Unsubscribe header on UID {uid} ({folder}): this sender exposes no supported unsubscribe action"
                );
            }
            return Ok(());
        }
    };

    let info = match unsubscribe::parse_list_unsubscribe(list_unsub_str, list_unsub_post.as_deref())
    {
        Some(info) => info,
        None => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "uid": uid,
                        "folder": folder,
                        "subject": msg.subject,
                        "from": msg.from_addr,
                        "raw_header": list_unsub_str,
                        "status": "parse_failed",
                        "message": "Could not parse List-Unsubscribe header",
                    })
                );
            } else {
                println!("UID {uid} ({folder})");
                println!("  From:    {}", msg.from_addr);
                println!("  Subject: {}", msg.subject);
                println!("  Header:  {list_unsub_str}");
                println!();
                println!("Could not parse List-Unsubscribe header.");
            }
            // A confirmed unsubscribe against an unparseable header has no
            // supported action: fail nonzero, do not exit success.
            if confirm {
                anyhow::bail!(
                    "unparseable List-Unsubscribe header on UID {uid} ({folder}): this sender exposes no supported unsubscribe action"
                );
            }
            return Ok(());
        }
    };

    // For mailto: build an SMTP send closure that first runs the Governor gate,
    // closing the previously ungated `mailto:` unsubscribe path. The `mailto:`
    // unsubscribe is a real SMTP surface, so it requires a factual `--attr`
    // declaration and fails closed (before Governor/SMTP) when it is
    // missing/invalid; audit is recorded either way.
    let creds_for_smtp = &creds;
    let db_ref = &db;
    let account_id = creds.account.id.clone();
    let account_dom = super::governor_gate::account_domain(&creds.account.username);
    let declared: Vec<String> = attr.to_vec();
    // Capture the resolved attribution of the mailto send. On success this is the
    // additive `attribution` block; on a gate refusal it is the canonical
    // attribution/Governor error object (code, reason, recovery) so the agent
    // learns exactly what to declare — never a claim of a send that did not run.
    let captured_attribution: std::cell::RefCell<Option<serde_json::Value>> =
        std::cell::RefCell::new(None);
    let captured_error: std::cell::RefCell<Option<serde_json::Value>> =
        std::cell::RefCell::new(None);
    // A real ASYNC mailto sender: the returned future runs the Governor gate,
    // then (only when allowed) awaits the SMTP send directly. No `block_on` — the
    // future is awaited on this task, so an allowed unsubscribe actually sends
    // instead of panicking inside the runtime.
    let smtp_send: Box<unsubscribe::MailtoSender> = Box::new(|addr: &str| {
        let addr = addr.to_string();
        let account_id = &account_id;
        let account_dom = &account_dom;
        let declared = &declared;
        let captured_attribution = &captured_attribution;
        let captured_error = &captured_error;
        Box::pin(async move {
            let req = super::governor_gate::unsubscribe_request(
                db_ref,
                account_id,
                account_dom.clone(),
                &addr,
                declared,
            );
            let outcome = super::governor_gate::gate_and_record(db_ref, account_id, &req);
            if !outcome.allowed {
                *captured_error.borrow_mut() = Some(outcome.error_json());
                return Err(envelope_email_transport::SmtpError::Send(format!(
                    "governor gate did not permit the unsubscribe send: {}",
                    outcome.reason_string()
                )));
            }
            *captured_attribution.borrow_mut() = outcome.success_attribution();
            SmtpSender::send_simple(
                creds_for_smtp,
                &addr,
                "unsubscribe",
                Some("unsubscribe"),
                None,
                None,
                None,
                None,
            )
            .await
            .map(|_msg_id| ())
        })
    });

    let result = unsubscribe::execute_unsubscribe(&info, confirm, Some(smtp_send.as_ref())).await;
    // Release the closure's borrow before reclaiming the captured values.
    drop(smtp_send);
    // Present only when a mailto SMTP unsubscribe actually ran through the gate.
    let unsubscribe_attribution = captured_attribution.into_inner();
    // Present only when the gate refused the mailto send (missing/invalid
    // declaration or a Governor block) — the canonical recovery error object.
    let unsubscribe_error = captured_error.into_inner();

    // ── Confirmed-failure handling: every confirmed failure exits NONZERO ──
    //
    // Three kinds of confirmed failure, each with FAILURE-SPECIFIC recovery:
    //   1. Gate refusal (attribution/Governor) — the canonical error object is
    //      present. Only an attribution failure gets an `--attr` retry; a
    //      Governor deny/review is never told to "retry unchanged".
    //   2. An allowed mailto whose SMTP transport failed (`result.status ==
    //      "failed"`, no gate error) — transient, retry later.
    //   3. No usable unsubscribe method (`method == "none"`) — no supported
    //      action.
    // A failed send never presents a success `attribution` block.
    // (An HTTPS one-click unsubscribe never reaches the gate.)
    let failure_kind = classify_unsub_failure(
        confirm,
        &result.status,
        &result.method,
        unsubscribe_error.as_ref(),
    );
    let failure = failure_kind.map(|kind| (kind, unsubscribe_error));

    if let Some((kind, gate_error)) = failure {
        let (reason, retry_command) = kind.recovery(uid, folder, account, &declared);
        // Build/augment the canonical error object with the failure-specific
        // recovery. A gate refusal already carries a canonical {code, reason,
        // recovery}; a transport/no-method failure gets a minimal one.
        let mut error_obj = gate_error.unwrap_or_else(|| {
            serde_json::json!({
                "code": kind.code(),
                "reason": reason.clone(),
                "detail": result.message,
            })
        });
        if let Some(cmd) = &retry_command
            && let Some(obj) = error_obj.as_object_mut()
        {
            let recovery = obj
                .entry("recovery")
                .or_insert_with(|| serde_json::json!({}));
            if let Some(recovery_obj) = recovery.as_object_mut() {
                recovery_obj.insert(
                    "retry_command".to_string(),
                    serde_json::Value::String(cmd.clone()),
                );
            }
        }
        let bail_reason = error_obj
            .get("reason")
            .and_then(|r| r.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| reason.clone());
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "uid": uid,
                    "folder": folder,
                    "subject": msg.subject,
                    "from": msg.from_addr,
                    "confirm": confirm,
                    "status": kind.status_label(),
                    "info": {
                        "https_urls": info.https_urls,
                        "mailto_urls": info.mailto_urls,
                        "one_click_post": info.one_click_post,
                    },
                    "result": {
                        "method": result.method,
                        "url": result.url,
                        "status": result.status,
                        "message": result.message,
                    },
                    "error": error_obj,
                }))?
            );
        }
        // Nonzero exit: a confirmed failure is not a success. Following the repo
        // convention (see `send`), the descriptive bail IS the plain-mode error
        // (anyhow prints `Error: …` to stderr) — no separate eprintln that would
        // duplicate the JSON/stderr detail. The `status: <blocked|failed>` prefix
        // keeps the message failure-specific.
        let label = kind.status_label();
        match &retry_command {
            Some(cmd) => anyhow::bail!("unsubscribe {label}: {bail_reason} Retry: {cmd}"),
            None => anyhow::bail!("unsubscribe {label}: {bail_reason}"),
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "uid": uid,
                "folder": folder,
                "subject": msg.subject,
                "from": msg.from_addr,
                "confirm": confirm,
                "info": {
                    "https_urls": info.https_urls,
                    "mailto_urls": info.mailto_urls,
                    "one_click_post": info.one_click_post,
                },
                "result": {
                    "method": result.method,
                    "url": result.url,
                    "status": result.status,
                    "message": result.message,
                },
                "attribution": unsubscribe_attribution,
            }))?
        );
    } else {
        println!("UID {uid} ({folder})");
        println!("  From:    {}", msg.from_addr);
        println!("  Subject: {}", msg.subject);
        println!();

        if !info.https_urls.is_empty() {
            println!("  HTTPS:   {}", info.https_urls.join(", "));
        }
        if !info.mailto_urls.is_empty() {
            println!("  Mailto:  {}", info.mailto_urls.join(", "));
        }
        if info.one_click_post {
            println!("  RFC 8058 one-click POST supported");
        }
        println!();

        match result.status.as_str() {
            "dry_run" => {
                println!("DRY RUN: {}", result.message);
                println!();
                println!("Pass --confirm to execute.");
            }
            "success" => {
                println!("SUCCESS: {}", result.message);
            }
            "failed" => {
                println!("FAILED: {}", result.message);
            }
            _ => {
                println!("{}: {}", result.status, result.message);
            }
        }
    }

    Ok(())
}

/// The classified reason a confirmed unsubscribe failed, driving failure-SPECIFIC
/// recovery. A Governor deny/review is never told to "retry unchanged"; only an
/// attribution failure gets an `--attr` retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnsubFailure {
    /// Missing/invalid declaration — declare a factual attribute and retry.
    Attribution,
    /// Governor denied the send — must not be retried unchanged.
    GovernorDeny,
    /// Governor routed the send to human review.
    GovernorReview,
    /// Governor was unreachable — retry once it is back.
    GovernorUnavailable,
    /// The declaration was valid and Governor allowed it, but the SMTP send failed.
    SmtpTransport,
    /// The message exposes no machine-usable unsubscribe method.
    NoMethod,
}

/// Decide whether a completed unsubscribe attempt is a confirmed failure, and of
/// which kind. Returns `None` for a dry run or a genuine success (→ exit zero);
/// `Some` for every confirmed failure (→ nonzero). A present gate-error object
/// is always a refusal; otherwise a confirmed `failed` result is a transport
/// failure (or no-method when no method was usable).
fn classify_unsub_failure(
    confirm: bool,
    result_status: &str,
    result_method: &str,
    gate_error: Option<&serde_json::Value>,
) -> Option<UnsubFailure> {
    if let Some(err) = gate_error {
        return Some(classify_gate_refusal(err));
    }
    if confirm && result_status == "failed" {
        return Some(if result_method == "none" {
            UnsubFailure::NoMethod
        } else {
            UnsubFailure::SmtpTransport
        });
    }
    None
}

/// Classify a gate-refusal error object (`GovernorOutcome::error_json`) from its
/// stable `code` and `route`.
fn classify_gate_refusal(err_obj: &serde_json::Value) -> UnsubFailure {
    let code = err_obj.get("code").and_then(|c| c.as_str()).unwrap_or("");
    let route = err_obj.get("route").and_then(|r| r.as_str());
    match code {
        "attributes_required" | "attributes_invalid" => UnsubFailure::Attribution,
        "governor_unavailable" => UnsubFailure::GovernorUnavailable,
        "governor_blocked" => match route {
            Some("review") => UnsubFailure::GovernorReview,
            // deny, or an unrouted block: never suggest an unchanged retry.
            _ => UnsubFailure::GovernorDeny,
        },
        // Any other/unknown gate block: conservatively do not suggest a retry.
        _ => UnsubFailure::GovernorDeny,
    }
}

impl UnsubFailure {
    /// The response `status` label: gate refusals are policy `blocked`; a
    /// transport/no-method failure is `failed`.
    fn status_label(&self) -> &'static str {
        match self {
            UnsubFailure::Attribution
            | UnsubFailure::GovernorDeny
            | UnsubFailure::GovernorReview
            | UnsubFailure::GovernorUnavailable => "blocked",
            UnsubFailure::SmtpTransport | UnsubFailure::NoMethod => "failed",
        }
    }

    /// A stable error code, used only to synthesize an error object for the
    /// transport/no-method cases (gate refusals keep their canonical code).
    fn code(&self) -> &'static str {
        match self {
            UnsubFailure::SmtpTransport => "unsubscribe_send_failed",
            UnsubFailure::NoMethod => "no_unsubscribe_method",
            UnsubFailure::Attribution => "attributes_required",
            UnsubFailure::GovernorDeny | UnsubFailure::GovernorReview => "governor_blocked",
            UnsubFailure::GovernorUnavailable => "governor_unavailable",
        }
    }

    /// Failure-specific recovery: a short reason line and, when re-running could
    /// plausibly help, the EXACT shell-quoted retry command. Only an attribution
    /// failure gets the `--attr <key>` retry; a Governor deny/review gets NO
    /// retry command (it must not be retried unchanged); Governor unavailable and
    /// a transient SMTP failure get a same-attributes retry; no-method gets none.
    fn recovery(
        &self,
        uid: u32,
        folder: &str,
        account: Option<&str>,
        declared: &[String],
    ) -> (String, Option<String>) {
        match self {
            UnsubFailure::Attribution => (
                "the mailto unsubscribe is a real SMTP send and needs at least one factual attribute; declare one and retry".to_string(),
                Some(build_unsubscribe_retry(uid, folder, account)),
            ),
            UnsubFailure::GovernorDeny => (
                "Governor denied this unsubscribe send; do not retry it unchanged (revise the request or seek human review)".to_string(),
                None,
            ),
            UnsubFailure::GovernorReview => (
                "Governor routed this unsubscribe to human review; a human must approve it before it can send".to_string(),
                None,
            ),
            UnsubFailure::GovernorUnavailable => (
                "Governor was unavailable; retry once it is reachable (check ENVELOPE_GOVERNOR_MODE / ENVELOPE_GOVERNOR_BIN)".to_string(),
                Some(build_unsubscribe_retry_with_attrs(uid, folder, account, declared)),
            ),
            UnsubFailure::SmtpTransport => (
                "the unsubscribe email could not be delivered (transient SMTP transport failure); retry later".to_string(),
                Some(build_unsubscribe_retry_with_attrs(uid, folder, account, declared)),
            ),
            UnsubFailure::NoMethod => (
                "this sender exposes no machine-usable unsubscribe method (no mailto or HTTPS one-click); there is no supported action".to_string(),
                None,
            ),
        }
    }
}

/// POSIX shell single-quoting for an interpolated retry-command argument. A safe
/// bare word passes through unchanged; anything else (spaces, quotes, shell
/// metacharacters) is single-quoted with embedded single quotes escaped, so the
/// printed retry command is never re-split or interpreted when run.
fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'@' | b'/' | b':' | b'-' | b'+')
        });
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// The attribution-repair retry command: the operator/agent must supply a factual
/// `--attr <key>` (the mailto path is a real SMTP send). Interpolated
/// account/folder are shell-quoted; `<key>` is a literal placeholder.
fn build_unsubscribe_retry(uid: u32, folder: &str, account: Option<&str>) -> String {
    let account = account
        .map(|a| format!(" --account {}", shell_quote(a)))
        .unwrap_or_default();
    format!(
        "envelope unsubscribe {uid} --folder {}{account} --attr <key> --confirm",
        shell_quote(folder)
    )
}

/// The same-declaration retry command for a transient failure (Governor
/// unavailable, SMTP transport): re-run with the SAME already-valid attributes.
/// Every interpolated account/folder/attribute is shell-quoted. Falls back to the
/// `<key>` placeholder when no attributes were supplied.
fn build_unsubscribe_retry_with_attrs(
    uid: u32,
    folder: &str,
    account: Option<&str>,
    declared: &[String],
) -> String {
    if declared.is_empty() {
        return build_unsubscribe_retry(uid, folder, account);
    }
    let account = account
        .map(|a| format!(" --account {}", shell_quote(a)))
        .unwrap_or_default();
    let attrs: String = declared
        .iter()
        .map(|a| format!(" --attr {}", shell_quote(a)))
        .collect();
    format!(
        "envelope unsubscribe {uid} --folder {}{account}{attrs} --confirm",
        shell_quote(folder)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn retry_command_carries_the_exact_attr_confirm_syntax() {
        let cmd = build_unsubscribe_retry(42, "INBOX", None);
        assert_eq!(
            cmd,
            "envelope unsubscribe 42 --folder INBOX --attr <key> --confirm"
        );
        let with_acct = build_unsubscribe_retry(7, "Lists", Some("me@example.test"));
        assert_eq!(
            with_acct,
            "envelope unsubscribe 7 --folder Lists --account me@example.test --attr <key> --confirm"
        );
    }

    #[test]
    fn shell_quote_passes_safe_words_and_quotes_metacharacters() {
        // Safe bare words (incl. common account/folder chars) are unchanged.
        assert_eq!(shell_quote("INBOX"), "INBOX");
        assert_eq!(shell_quote("me@example.test"), "me@example.test");
        assert!(shell_quote("[Gmail]/All Mail").contains('\''));
        // Spaces, quotes, and metacharacters are single-quoted and escaped.
        assert_eq!(shell_quote("My Folder"), "'My Folder'");
        assert_eq!(shell_quote("a;rm -rf b"), "'a;rm -rf b'");
        assert_eq!(shell_quote("it's $HOME `x`"), "'it'\\''s $HOME `x`'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn retry_commands_shell_quote_every_interpolated_argument() {
        // A folder with a space and an account/attr with metacharacters must be
        // single-quoted so a shell treats each as ONE argument.
        let cmd = build_unsubscribe_retry(9, "My Folder", Some("a b@x.test"));
        assert!(
            cmd.contains("--folder 'My Folder'"),
            "folder with a space must be quoted: {cmd}"
        );
        assert!(
            cmd.contains("--account 'a b@x.test'"),
            "account with a space must be quoted: {cmd}"
        );

        let cmd = build_unsubscribe_retry_with_attrs(
            9,
            "Lists 2024",
            None,
            &["financial content".to_string(), "informational".to_string()],
        );
        assert!(cmd.contains("--folder 'Lists 2024'"), "{cmd}");
        assert!(cmd.contains("--attr 'financial content'"), "{cmd}");
        assert!(cmd.contains("--attr informational"), "{cmd}");
        // No unquoted injection survives.
        let cmd = build_unsubscribe_retry_with_attrs(
            1,
            "INBOX",
            Some("x; touch /tmp/pwned"),
            &["informational".to_string()],
        );
        assert!(
            cmd.contains("--account 'x; touch /tmp/pwned'"),
            "metacharacters must be quoted: {cmd}"
        );
    }

    #[test]
    fn confirmed_failures_are_classified_nonzero_and_successes_are_not() {
        // Dry run / success → None (exit zero).
        assert_eq!(
            classify_unsub_failure(false, "dry_run", "mailto", None),
            None
        );
        assert_eq!(
            classify_unsub_failure(true, "success", "mailto", None),
            None
        );
        // Confirmed transport failure → SmtpTransport (nonzero).
        assert_eq!(
            classify_unsub_failure(true, "failed", "mailto", None),
            Some(UnsubFailure::SmtpTransport)
        );
        // Confirmed no-method → NoMethod (nonzero).
        assert_eq!(
            classify_unsub_failure(true, "failed", "none", None),
            Some(UnsubFailure::NoMethod)
        );
    }

    #[test]
    fn gate_refusals_map_to_specific_failures() {
        assert_eq!(
            classify_gate_refusal(&json!({ "code": "attributes_required" })),
            UnsubFailure::Attribution
        );
        assert_eq!(
            classify_gate_refusal(&json!({ "code": "attributes_invalid" })),
            UnsubFailure::Attribution
        );
        assert_eq!(
            classify_gate_refusal(&json!({ "code": "governor_unavailable" })),
            UnsubFailure::GovernorUnavailable
        );
        assert_eq!(
            classify_gate_refusal(&json!({ "code": "governor_blocked", "route": "review" })),
            UnsubFailure::GovernorReview
        );
        assert_eq!(
            classify_gate_refusal(&json!({ "code": "governor_blocked", "route": "deny" })),
            UnsubFailure::GovernorDeny
        );
        // An unrouted/unknown gate block never suggests an unchanged retry.
        assert_eq!(
            classify_gate_refusal(&json!({ "code": "governor_blocked" })),
            UnsubFailure::GovernorDeny
        );
    }

    #[test]
    fn recovery_is_failure_specific() {
        let declared = vec!["informational".to_string()];
        // Attribution → the --attr <key> placeholder retry.
        let (_r, retry) = UnsubFailure::Attribution.recovery(3, "INBOX", None, &declared);
        assert_eq!(
            retry.as_deref(),
            Some("envelope unsubscribe 3 --folder INBOX --attr <key> --confirm")
        );
        // Governor deny/review → NO retry command (never retry unchanged).
        assert!(
            UnsubFailure::GovernorDeny
                .recovery(3, "INBOX", None, &declared)
                .1
                .is_none()
        );
        assert!(
            UnsubFailure::GovernorReview
                .recovery(3, "INBOX", None, &declared)
                .1
                .is_none()
        );
        // Governor unavailable / SMTP transport → same-attributes retry.
        let (_r, retry) = UnsubFailure::GovernorUnavailable.recovery(3, "INBOX", None, &declared);
        assert_eq!(
            retry.as_deref(),
            Some("envelope unsubscribe 3 --folder INBOX --attr informational --confirm")
        );
        let (_r, retry) = UnsubFailure::SmtpTransport.recovery(3, "INBOX", None, &declared);
        assert!(retry.unwrap().contains("--attr informational"));
        // No-method → no retry command.
        assert!(
            UnsubFailure::NoMethod
                .recovery(3, "INBOX", None, &declared)
                .1
                .is_none()
        );
    }

    #[test]
    fn status_labels_separate_policy_blocks_from_transport_failures() {
        assert_eq!(UnsubFailure::Attribution.status_label(), "blocked");
        assert_eq!(UnsubFailure::GovernorDeny.status_label(), "blocked");
        assert_eq!(UnsubFailure::GovernorReview.status_label(), "blocked");
        assert_eq!(UnsubFailure::GovernorUnavailable.status_label(), "blocked");
        assert_eq!(UnsubFailure::SmtpTransport.status_label(), "failed");
        assert_eq!(UnsubFailure::NoMethod.status_label(), "failed");
    }
}
