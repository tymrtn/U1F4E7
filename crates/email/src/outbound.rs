// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Outbound send protections: a default actual-send cooldown (outbox queueing)
//! and a Governor decision gate that runs before any real SMTP transmission.
//!
//! These two protections exist to make "an agent sends mail too fast / without
//! oversight" structurally hard:
//!
//! 1. Any path that would actually transmit mail queues into the existing
//!    draft/outbox scheduled-send mechanism by default (see [`resolve_disposition`]).
//!    Real SMTP only happens later, when the scheduled-send sweep finds the item
//!    due — and only after Governor permits it.
//! 2. Before any real SMTP send, the caller runs [`gate`]. When Governor is
//!    configured as `required` it fails closed: missing/errored/denied/review
//!    all block the send. Only an explicit `allow` from Governor permits SMTP.
//!
//! Nothing in this module logs message bodies, full recipient addresses,
//! attachment bytes, or secrets. Governor receives only the sanitized attribute
//! **keys** an action exhibits (blind attribution) — never the content it scored.

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::attribution::{AttributedSendContext, collect_recipient_domains};

/// Governor catalog Envelope declares its send attributes against. Governor
/// scores these keys blindly; Envelope never reproduces the catalog's weights.
pub const GOVERNOR_CATALOG: &str = "envelope";

/// Default actual-send cooldown, in seconds, when nothing overrides it.
pub const DEFAULT_COOLDOWN_SECONDS: i64 = 120;

/// Stable reason code included when an allowed send is queued instead of
/// transmitted immediately.
pub const OUTBOX_COOLDOWN_REASON_CODE: &str = "safety_cooldown";

/// Human-readable reason included in queued-send JSON so agents understand the
/// outbox is intentional, not a failure or provider delay.
pub const OUTBOX_COOLDOWN_REASON: &str = "queued in the Envelope outbox for the safety cooldown, giving the agent/operator time to report and correct issues before SMTP transmission";

/// Environment variable that overrides the default actual-send cooldown.
pub const ENV_COOLDOWN_SECONDS: &str = "ENVELOPE_SEND_COOLDOWN_SECONDS";

/// Environment variable that selects the Governor gate mode.
pub const ENV_GOVERNOR_MODE: &str = "ENVELOPE_GOVERNOR_MODE";

/// Environment variable that points at the Governor CLI binary.
pub const ENV_GOVERNOR_BIN: &str = "ENVELOPE_GOVERNOR_BIN";

/// Tyler-local canonical Governor binary path. Used as a fallback before PATH so
/// Envelope does not accidentally hit an older `governor` binary on this machine.
pub const DEFAULT_LOCAL_GOVERNOR_BIN: &str =
    "/Users/tylermartin/Dropbox/Code/governor/governor2/target/release/governor";

/// Resolve the actual-send cooldown in seconds.
///
/// Precedence: explicit CLI/tool override → `ENVELOPE_SEND_COOLDOWN_SECONDS` →
/// [`DEFAULT_COOLDOWN_SECONDS`]. Negative values are clamped to zero (which a
/// caller may only honor as immediate with an explicit confirm — see
/// [`resolve_disposition`]).
pub fn resolve_cooldown_seconds(cli_override: Option<i64>) -> i64 {
    let raw = cli_override
        .or_else(|| {
            std::env::var(ENV_COOLDOWN_SECONDS)
                .ok()
                .and_then(|v| v.trim().parse::<i64>().ok())
        })
        .unwrap_or(DEFAULT_COOLDOWN_SECONDS);
    raw.max(0)
}

/// What a send primitive should do with a request that policy has *allowed*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendDisposition {
    /// Queue into the outbox with this cooldown (seconds from now) before the
    /// scheduled-send sweep is allowed to transmit it.
    Queue { cooldown_seconds: i64 },
    /// Transmit immediately (explicit, confirmed emergency bypass).
    Immediate,
    /// The caller asked to bypass the cooldown but did not supply the required
    /// confirmation. The send must be refused with a stable denial.
    NeedsConfirmation,
}

/// Stable denial code for an unconfirmed immediate-send bypass attempt.
pub const IMMEDIATE_SEND_CONFIRM_CODE: &str = "immediate_send_requires_confirmation";

/// Decide whether an allowed send should queue (the default) or transmit now.
///
/// Immediate transmission is an explicit, deliberate emergency bypass: it is
/// only granted when the caller both asks for it (`send_now` or a zero cooldown)
/// **and** supplies confirmation. Without confirmation the bypass is refused
/// rather than silently falling back to immediate send.
pub fn resolve_disposition(
    cooldown_seconds: i64,
    send_now: bool,
    confirm_send_now: bool,
) -> SendDisposition {
    let wants_immediate = send_now || cooldown_seconds <= 0;
    if wants_immediate {
        if confirm_send_now {
            SendDisposition::Immediate
        } else {
            SendDisposition::NeedsConfirmation
        }
    } else {
        SendDisposition::Queue { cooldown_seconds }
    }
}

/// The surface that originated an actual-send attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendSurface {
    Cli,
    Mcp,
    Scheduled,
}

impl SendSurface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Mcp => "mcp",
            Self::Scheduled => "scheduled",
        }
    }
}

// ── Governor gate ──────────────────────────────────────────────────────────

/// Governor gate enforcement mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GovernorMode {
    /// Fail closed: only an explicit Governor `allow` permits SMTP. Missing,
    /// errored, denied, or review verdicts all block the send.
    Required,
    /// Run Governor and record its verdict, but never block the send.
    Warn,
    /// Skip the Governor gate entirely.
    Off,
}

impl GovernorMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Warn => "warn",
            Self::Off => "off",
        }
    }

    /// Parse the mode from a string, defaulting to `required` on anything
    /// unrecognized so that misconfiguration fails safe.
    pub fn parse_or_required(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "warn" => Self::Warn,
            "off" | "disabled" | "none" => Self::Off,
            _ => Self::Required,
        }
    }
}

/// Resolved Governor gate configuration.
#[derive(Debug, Clone)]
pub struct GovernorConfig {
    pub mode: GovernorMode,
    /// Path/name of the Governor CLI binary.
    pub bin: String,
}

impl GovernorConfig {
    /// Build config from the environment. Default mode is `required` so that a
    /// Tyler-local install with no configuration still fails safe.
    pub fn from_env() -> Self {
        let mode = std::env::var(ENV_GOVERNOR_MODE)
            .ok()
            .map(|v| GovernorMode::parse_or_required(&v))
            .unwrap_or(GovernorMode::Required);
        let bin = std::env::var(ENV_GOVERNOR_BIN)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                std::path::Path::new(DEFAULT_LOCAL_GOVERNOR_BIN)
                    .exists()
                    .then(|| DEFAULT_LOCAL_GOVERNOR_BIN.to_string())
            })
            .unwrap_or_else(|| "governor".to_string());
        Self { mode, bin }
    }
}

/// Sanitized description of an actual-send attempt handed to Governor.
///
/// This intentionally excludes full recipient addresses, message bodies,
/// subjects (only a hash), attachment bytes, and any secret material. The
/// [`attributes`](Self::attributes) are the canonical Governor **envelope**
/// catalog keys the action honestly exhibits — the only thing Governor scores.
/// The remaining fields are sanitized audit metadata, never scored.
#[derive(Debug, Clone)]
pub struct GovernorRequest {
    pub account_id: String,
    pub account_domain: Option<String>,
    pub subject_hash: String,
    pub recipient_count: usize,
    pub recipient_domains: Vec<String>,
    pub recipient_classes: Vec<String>,
    pub surface: SendSurface,
    pub draft_id: Option<String>,
    pub attachment_count: usize,
    pub attachment_total_bytes: u64,
    pub attachment_types: Vec<String>,
    pub is_reply: bool,
    /// Canonical Governor envelope-catalog attribute keys this send exhibits.
    pub attributes: Vec<String>,
}

impl GovernorRequest {
    /// Construct a request from raw send inputs, deriving **structural**
    /// attributes only (thread / domain shape / attachment / recipient count).
    /// Store-relationship facts (known/frequent/cold contact) and content
    /// classifiers are left unknown here. Callers that have those facts should
    /// build an [`AttributedSendContext`] and use [`Self::from_context`] for the
    /// fuller, more honest attribution.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        account_id: &str,
        account_domain: Option<&str>,
        subject: &str,
        to: &str,
        cc: Option<&str>,
        bcc: Option<&str>,
        surface: SendSurface,
        draft_id: Option<&str>,
        attachment_sizes: &[(String, u64)],
        is_reply: bool,
    ) -> Self {
        let summary = collect_recipient_domains(to, cc, bcc);
        let ctx = AttributedSendContext {
            account_domain: account_domain.map(|d| d.to_ascii_lowercase()),
            recipient_domains: summary.domains,
            recipient_count: summary.count,
            is_reply,
            has_bcc: summary.has_bcc,
            attachment_count: attachment_sizes.len(),
            ..Default::default()
        };
        Self::from_context(
            account_id,
            subject,
            surface,
            draft_id,
            attachment_sizes,
            &ctx,
        )
    }

    /// Construct a request from a fully-derived [`AttributedSendContext`] plus the
    /// sanitized audit details (account, subject hash, attachment sizes/types).
    /// This is the honest, store-and-classifier-aware path every actual-send
    /// surface converges on. The context supplies the attribute **keys** Governor
    /// scores; the remaining arguments are audit-only metadata.
    pub fn from_context(
        account_id: &str,
        subject: &str,
        surface: SendSurface,
        draft_id: Option<&str>,
        attachment_sizes: &[(String, u64)],
        ctx: &AttributedSendContext,
    ) -> Self {
        let acct_domain = ctx.account_domain.clone();
        let classes: Vec<String> = ctx
            .recipient_domains
            .iter()
            .map(|d| match &acct_domain {
                Some(ad) if ad == d => "internal".to_string(),
                _ => "external".to_string(),
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();

        let mut types: Vec<String> = attachment_sizes
            .iter()
            .map(|(t, _)| t.to_ascii_lowercase())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        types.sort();
        let total_bytes: u64 = attachment_sizes.iter().map(|(_, n)| *n).sum();

        let attributes = ctx
            .to_governor_attrs()
            .into_iter()
            .map(str::to_string)
            .collect();

        Self {
            account_id: account_id.to_string(),
            account_domain: acct_domain,
            subject_hash: hash_subject(subject),
            recipient_count: ctx.recipient_count,
            recipient_domains: ctx.recipient_domains.clone(),
            recipient_classes: classes,
            surface,
            draft_id: draft_id.map(str::to_string),
            attachment_count: attachment_sizes.len(),
            attachment_total_bytes: total_bytes,
            attachment_types: types,
            is_reply: ctx.is_reply,
            attributes,
        }
    }

    /// Sanitized JSON payload safe to persist in audit/event rows. Records the
    /// declared attribute **keys** (what Governor scored) — never the content.
    pub fn audit_payload(&self) -> Value {
        json!({
            "surface": self.surface.as_str(),
            "catalog": GOVERNOR_CATALOG,
            "attributes": self.attributes,
            "recipient_count": self.recipient_count,
            "recipient_domains": self.recipient_domains,
            "recipient_classes": self.recipient_classes,
            "subject_hash": self.subject_hash,
            "attachment_count": self.attachment_count,
            "attachment_total_bytes": self.attachment_total_bytes,
            "attachment_types": self.attachment_types,
            "is_reply": self.is_reply,
            "draft_id": self.draft_id,
        })
    }

    /// Content-free justification passed to Governor for its own audit trail:
    /// `<surface>:<draft-id or ->`. Contains no recipient address, subject, or
    /// body — only the surface and the local draft id (a UUID/UID).
    pub fn justification(&self) -> String {
        format!(
            "envelope-send {}:{}",
            self.surface.as_str(),
            self.draft_id.as_deref().unwrap_or("-")
        )
    }
}

/// Parsed, sanitized outcome of a Governor gate evaluation.
#[derive(Debug, Clone, PartialEq)]
pub struct GovernorOutcome {
    /// Whether SMTP is permitted to proceed.
    pub allowed: bool,
    pub mode: GovernorMode,
    /// Raw Governor decision string (`allow`/`deny`/`review`) or a gate-internal
    /// status (`disabled`/`unavailable`/`unparseable`).
    pub decision: String,
    pub state: Option<String>,
    pub score: Option<f64>,
    pub review_ticket_id: Option<String>,
    /// Stable denial code when the send is blocked (None when allowed).
    pub block_code: Option<String>,
    pub block_reason: Option<String>,
}

impl GovernorOutcome {
    /// Sanitized JSON for both audit events and agent-facing denial payloads.
    pub fn audit_json(&self) -> Value {
        json!({
            "allowed": self.allowed,
            "mode": self.mode.as_str(),
            "decision": self.decision,
            "state": self.state,
            "score": self.score,
            "review_ticket_id": self.review_ticket_id,
            "block_code": self.block_code,
            "block_reason": self.block_reason,
        })
    }

    /// Stable agent-facing denial body (`{code, reason}`) when blocked.
    pub fn denial_json(&self) -> Value {
        json!({
            "code": self.block_code.clone().unwrap_or_else(|| "governor_blocked".to_string()),
            "reason": self
                .block_reason
                .clone()
                .unwrap_or_else(|| "governor did not permit this send".to_string()),
            "governor": self.audit_json(),
        })
    }
}

/// Decision fields parsed from Governor's `shell --json` output.
#[derive(Debug, Clone, PartialEq)]
pub struct GovernorVerdict {
    pub decision: String,
    pub state: Option<String>,
    pub score: Option<f64>,
    pub review_ticket_id: Option<String>,
}

/// Parse Governor's JSON output into a verdict. Returns `None` if the output is
/// not parseable JSON with a `decision` field.
pub fn parse_governor_verdict(stdout: &str) -> Option<GovernorVerdict> {
    let value: Value = serde_json::from_str(stdout.trim()).ok()?;
    let decision = value.get("decision")?.as_str()?.to_ascii_lowercase();
    let state = value
        .get("state")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let score = value.get("score").and_then(|v| v.as_f64());
    let review_ticket_id = value
        .get("review_ticket")
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Some(GovernorVerdict {
        decision,
        state,
        score,
        review_ticket_id,
    })
}

/// Apply a parsed verdict against the gate mode to produce the final outcome.
pub fn decide_from_verdict(mode: GovernorMode, verdict: GovernorVerdict) -> GovernorOutcome {
    let permitted = matches!(verdict.decision.as_str(), "allow" | "allowed");
    let allowed = permitted || mode == GovernorMode::Warn;
    let (block_code, block_reason) = if permitted {
        (None, None)
    } else {
        (
            Some("governor_blocked".to_string()),
            Some(format!(
                "governor decision '{}'{} did not permit this send",
                verdict.decision,
                verdict
                    .state
                    .as_deref()
                    .map(|s| format!(" (state '{s}')"))
                    .unwrap_or_default()
            )),
        )
    };
    GovernorOutcome {
        allowed,
        mode,
        decision: verdict.decision,
        state: verdict.state,
        score: verdict.score,
        review_ticket_id: verdict.review_ticket_id,
        // In warn mode we never block even on a non-allow verdict.
        block_code: if allowed { None } else { block_code },
        block_reason: if allowed { None } else { block_reason },
    }
}

fn fail_outcome(mode: GovernorMode, decision: &str, reason: &str) -> GovernorOutcome {
    let allowed = mode == GovernorMode::Warn;
    GovernorOutcome {
        allowed,
        mode,
        decision: decision.to_string(),
        state: None,
        score: None,
        review_ticket_id: None,
        block_code: if allowed {
            None
        } else {
            Some("governor_unavailable".to_string())
        },
        block_reason: if allowed {
            None
        } else {
            Some(reason.to_string())
        },
    }
}

/// Run the Governor gate for an actual-send attempt using **blind attribution**.
///
/// Envelope declares the canonical envelope-catalog attribute keys the send
/// exhibits (`req.attributes`); Governor scores them opaquely against the
/// envelope catalog and returns allow/review/deny. Envelope never reconstructs
/// weights or thresholds, and never sends a fabricated command string.
///
/// Fails closed in `required` mode: a missing binary, spawn error, unparseable
/// output, or any non-`allow` verdict blocks the send. In `warn` mode the
/// verdict is recorded but never blocks. In `off` mode the gate is skipped.
pub fn gate(config: &GovernorConfig, req: &GovernorRequest) -> GovernorOutcome {
    if config.mode == GovernorMode::Off {
        return GovernorOutcome {
            allowed: true,
            mode: GovernorMode::Off,
            decision: "disabled".to_string(),
            state: None,
            score: None,
            review_ticket_id: None,
            block_code: None,
            block_reason: None,
        };
    }

    // `governor score --catalog envelope --attr <k> ... --json`: pure blind
    // attribution over the declared keys. The `--just` string is logged by
    // Governor but never scored, and carries only the surface + draft id (no PII).
    let mut command = std::process::Command::new(&config.bin);
    command
        .arg("score")
        .arg("--catalog")
        .arg(GOVERNOR_CATALOG)
        .arg("--json");
    for attr in &req.attributes {
        command.arg("--attr").arg(attr);
    }
    command.arg("--just").arg(req.justification());

    match command.output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            match parse_governor_verdict(&stdout) {
                Some(verdict) => decide_from_verdict(config.mode, verdict),
                None => fail_outcome(
                    config.mode,
                    "unparseable",
                    "governor produced no parseable decision",
                ),
            }
        }
        Err(_) => fail_outcome(
            config.mode,
            "unavailable",
            "governor binary could not be executed",
        ),
    }
}

/// SHA-256 hash of a subject, hex-encoded and prefixed. Never the raw subject.
pub fn hash_subject(subject: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(subject.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{}", &hex[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cooldown_is_120_seconds() {
        // Explicit override wins.
        assert_eq!(resolve_cooldown_seconds(Some(45)), 45);
        // Negative override clamps to zero.
        assert_eq!(resolve_cooldown_seconds(Some(-5)), 0);
    }

    #[test]
    fn default_disposition_queues_with_cooldown() {
        assert_eq!(
            resolve_disposition(120, false, false),
            SendDisposition::Queue {
                cooldown_seconds: 120
            }
        );
    }

    #[test]
    fn immediate_bypass_requires_confirmation() {
        // send_now without confirm => refused.
        assert_eq!(
            resolve_disposition(120, true, false),
            SendDisposition::NeedsConfirmation
        );
        // zero cooldown without confirm => refused.
        assert_eq!(
            resolve_disposition(0, false, false),
            SendDisposition::NeedsConfirmation
        );
        // send_now + confirm => immediate.
        assert_eq!(
            resolve_disposition(120, true, true),
            SendDisposition::Immediate
        );
        // zero cooldown + confirm => immediate.
        assert_eq!(
            resolve_disposition(0, false, true),
            SendDisposition::Immediate
        );
    }

    #[test]
    fn governor_mode_parses_safe_default() {
        assert_eq!(GovernorMode::parse_or_required("warn"), GovernorMode::Warn);
        assert_eq!(GovernorMode::parse_or_required("off"), GovernorMode::Off);
        assert_eq!(
            GovernorMode::parse_or_required("required"),
            GovernorMode::Required
        );
        // Unknown values fail safe to required.
        assert_eq!(
            GovernorMode::parse_or_required("banana"),
            GovernorMode::Required
        );
    }

    fn sample_request() -> GovernorRequest {
        GovernorRequest::build(
            "acct-1",
            Some("envelope.test"),
            "Quarterly numbers",
            "Alice <alice@example.com>, bob@example.com",
            Some("carol@envelope.test"),
            None,
            SendSurface::Scheduled,
            Some("draft-9"),
            &[("application/pdf".to_string(), 1024)],
            false,
        )
    }

    #[test]
    fn request_attrs_justification_and_audit_never_leak_address_or_subject() {
        let req = sample_request();
        let audit = req.audit_payload().to_string();
        let just = req.justification();
        let attrs = req.attributes.join(",");

        for needle in [
            "alice@example.com",
            "bob@example.com",
            "carol@envelope.test",
            "Quarterly numbers",
        ] {
            assert!(!audit.contains(needle), "audit leaked {needle}: {audit}");
            assert!(
                !just.contains(needle),
                "justification leaked {needle}: {just}"
            );
            assert!(
                !attrs.contains(needle),
                "attributes leaked {needle}: {attrs}"
            );
        }
        // Sanitized audit facts are present; the justification carries only the
        // surface + draft id.
        assert!(audit.contains("sha256:"));
        assert!(audit.contains("\"catalog\":\"envelope\""));
        assert_eq!(just, "envelope-send scheduled:draft-9");
        assert_eq!(req.recipient_count, 3);
        assert!(req.recipient_classes.contains(&"internal".to_string()));
        assert!(req.recipient_classes.contains(&"external".to_string()));
        // Mixed internal+external recipients with an attachment: structural
        // attributes are declared, and every one is a canonical envelope key.
        assert!(req.attributes.contains(&"has_attachment".to_string()));
        assert!(!req.attributes.contains(&"internal_domain".to_string()));
    }

    #[test]
    fn structural_attributes_are_derived_by_build() {
        // A reply with a BCC and six recipients declares the structural keys.
        let req = GovernorRequest::build(
            "acct-1",
            Some("martin.fm"),
            "Re: hello",
            "a@x.com, b@x.com, c@x.com, d@x.com, e@x.com",
            None,
            Some("f@x.com"),
            SendSurface::Cli,
            None,
            &[],
            true,
        );
        assert!(req.attributes.contains(&"reply_to_thread".to_string()));
        assert!(req.attributes.contains(&"has_bcc".to_string()));
        assert!(req.attributes.contains(&"bulk_send".to_string()));
    }

    #[test]
    fn from_context_carries_store_facts_into_attributes() {
        let ctx = AttributedSendContext {
            account_domain: Some("martin.fm".into()),
            recipient_domains: vec!["martin.fm".into()],
            recipient_count: 1,
            is_reply: true,
            known_contact: Some(true),
            human_approved: true,
            ..Default::default()
        };
        let req = GovernorRequest::from_context(
            "acct-1",
            "Subject",
            SendSurface::Mcp,
            Some("d-1"),
            &[],
            &ctx,
        );
        assert!(req.attributes.contains(&"reply_to_thread".to_string()));
        assert!(req.attributes.contains(&"known_contact".to_string()));
        assert!(req.attributes.contains(&"internal_domain".to_string()));
        assert!(req.attributes.contains(&"tyler_approved".to_string()));
    }

    #[test]
    fn allow_verdict_permits_send() {
        let outcome = decide_from_verdict(
            GovernorMode::Required,
            GovernorVerdict {
                decision: "allow".to_string(),
                state: Some("allowed".to_string()),
                score: Some(0.9),
                review_ticket_id: None,
            },
        );
        assert!(outcome.allowed);
        assert!(outcome.block_code.is_none());
    }

    #[test]
    fn review_and_deny_block_when_required() {
        for decision in ["review", "deny", "block"] {
            let outcome = decide_from_verdict(
                GovernorMode::Required,
                GovernorVerdict {
                    decision: decision.to_string(),
                    state: Some("review_required".to_string()),
                    score: Some(-0.1),
                    review_ticket_id: Some("review-1".to_string()),
                },
            );
            assert!(!outcome.allowed, "{decision} should block");
            assert_eq!(outcome.block_code.as_deref(), Some("governor_blocked"));
        }
    }

    #[test]
    fn warn_mode_never_blocks_but_records_verdict() {
        let outcome = decide_from_verdict(
            GovernorMode::Warn,
            GovernorVerdict {
                decision: "deny".to_string(),
                state: None,
                score: None,
                review_ticket_id: None,
            },
        );
        assert!(outcome.allowed);
        assert_eq!(outcome.decision, "deny");
        assert!(outcome.block_code.is_none());
    }

    #[test]
    fn missing_governor_fails_closed_when_required() {
        let config = GovernorConfig {
            mode: GovernorMode::Required,
            bin: "/nonexistent/governor-binary-xyz".to_string(),
        };
        let outcome = gate(&config, &sample_request());
        assert!(!outcome.allowed);
        assert_eq!(outcome.block_code.as_deref(), Some("governor_unavailable"));
    }

    #[test]
    fn missing_governor_warns_open_when_warn() {
        let config = GovernorConfig {
            mode: GovernorMode::Warn,
            bin: "/nonexistent/governor-binary-xyz".to_string(),
        };
        let outcome = gate(&config, &sample_request());
        assert!(outcome.allowed);
    }

    #[test]
    fn off_mode_skips_gate() {
        let config = GovernorConfig {
            mode: GovernorMode::Off,
            bin: "/nonexistent/governor-binary-xyz".to_string(),
        };
        let outcome = gate(&config, &sample_request());
        assert!(outcome.allowed);
        assert_eq!(outcome.decision, "disabled");
    }

    #[test]
    fn parse_verdict_extracts_decision_state_score_ticket() {
        let stdout = r#"{
            "decision": "review",
            "state": "review_required",
            "score": -0.04,
            "review_ticket": { "id": "review-123", "path": "/x" }
        }"#;
        let v = parse_governor_verdict(stdout).unwrap();
        assert_eq!(v.decision, "review");
        assert_eq!(v.state.as_deref(), Some("review_required"));
        assert_eq!(v.score, Some(-0.04));
        assert_eq!(v.review_ticket_id.as_deref(), Some("review-123"));
    }

    #[test]
    fn parse_verdict_rejects_non_json() {
        assert!(parse_governor_verdict("not json").is_none());
        assert!(parse_governor_verdict("{}").is_none());
    }
}
