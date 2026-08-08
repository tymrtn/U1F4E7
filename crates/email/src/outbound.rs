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

use crate::attribution::{
    AttributedSendContext, AttributionResolution, AttributionState, collect_recipient_domains,
    resolve,
};
use crate::attribution_suggest::{self, Suggestion};
use crate::governor_catalog::HONESTY_RULES;

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
    /// Host-**derived** Governor envelope-catalog attribute keys this send
    /// exhibits (structural/store facts). This is the derived set only; the
    /// bot's declarations live in [`Self::declared_attrs`] and the validated
    /// union actually submitted lives in [`Self::resolution`].
    pub attributes: Vec<String>,
    /// The raw attribute keys the bot **declared** (verbatim, deduped). Empty on
    /// legacy construction paths.
    pub declared_attrs: Vec<String>,
    /// Whether this surface requires at least one factual bot declaration (host
    /// facts never substitute). `true` for bot-originated actual-send surfaces.
    pub require_declaration: bool,
    /// The resolved attribution (declared/derived/governor + rejections + state).
    /// `None` on legacy paths that predate the attribution protocol; when set,
    /// [`gate_with_attribution`] enforces it before spawning Governor.
    pub resolution: Option<AttributionResolution>,
    /// Optional per-agent identity, carried as **audit metadata only**. This is
    /// never added to [`Self::attributes`] and never influences Governor scoring
    /// or any allow/deny decision. Defaults to `None` on every construction path
    /// so behavior is byte-identical until a caller explicitly threads it in.
    pub agent_id: Option<String>,
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
            declared_attrs: Vec::new(),
            require_declaration: false,
            resolution: None,
            agent_id: None,
        }
    }

    /// Construct a request and resolve the bot's `declared` attributes against
    /// the derived facts in one step. This is the attribution-protocol path every
    /// bot-originated actual-send surface uses: it stores the full
    /// [`AttributionResolution`] so [`gate_with_attribution`] can refuse an
    /// invalid or unattributed request **before** Governor is spawned.
    #[allow(clippy::too_many_arguments)]
    pub fn from_context_with_declared(
        account_id: &str,
        subject: &str,
        surface: SendSurface,
        draft_id: Option<&str>,
        attachment_sizes: &[(String, u64)],
        ctx: &AttributedSendContext,
        declared: &[String],
        require_declaration: bool,
    ) -> Self {
        let mut req = Self::from_context(
            account_id,
            subject,
            surface,
            draft_id,
            attachment_sizes,
            ctx,
        );
        let resolution = resolve(declared, ctx, require_declaration);
        req.declared_attrs = resolution.declared_attrs.clone();
        req.require_declaration = require_declaration;
        req.resolution = Some(resolution);
        req
    }

    /// A sanitized, content-free one-line description of the action, for the
    /// recovery `reason`. Contains no address, subject, or body — only shape.
    pub fn action_echo(&self) -> String {
        let kind = if self.is_reply {
            "reply"
        } else {
            "new message"
        };
        let classes = if self.recipient_classes.is_empty() {
            "external".to_string()
        } else {
            self.recipient_classes.join("+")
        };
        let attach = match self.attachment_count {
            0 => "no attachments".to_string(),
            1 => "one attachment".to_string(),
            n => format!("{n} attachments"),
        };
        format!(
            "{kind} to {} {classes} recipient(s), {attach}",
            self.recipient_count
        )
    }

    /// Attach a per-agent identity as **audit metadata only**.
    ///
    /// The `agent_id` appears in [`Self::audit_payload`] for provenance but is
    /// never pushed into [`Self::attributes`], so it cannot reach Governor's
    /// scoring inputs or change any verdict. Omitting the call (or passing
    /// `None`) leaves the request identical to today.
    pub fn with_agent_id(mut self, agent_id: Option<&str>) -> Self {
        self.agent_id = agent_id.map(str::to_string);
        self
    }

    /// Sanitized JSON payload safe to persist in audit/event rows. Records the
    /// declared attribute **keys** (what Governor scored) — never the content.
    pub fn audit_payload(&self) -> Value {
        let mut payload = json!({
            "surface": self.surface.as_str(),
            "catalog": GOVERNOR_CATALOG,
            "attributes": self.attributes,
            "declared_attrs": self.declared_attrs,
            "require_declaration": self.require_declaration,
            "recipient_count": self.recipient_count,
            "recipient_domains": self.recipient_domains,
            "recipient_classes": self.recipient_classes,
            "subject_hash": self.subject_hash,
            "attachment_count": self.attachment_count,
            "attachment_total_bytes": self.attachment_total_bytes,
            "attachment_types": self.attachment_types,
            "is_reply": self.is_reply,
            "draft_id": self.draft_id,
        });
        // Audit metadata only, and omitted when absent so today's payload is
        // byte-identical. Deliberately not part of `attributes`: Governor never
        // scores the agent id.
        if let Some(agent_id) = &self.agent_id
            && let Value::Object(map) = &mut payload
        {
            map.insert("agent_id".to_string(), Value::String(agent_id.clone()));
        }
        payload
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

/// Parsed, sanitized outcome of a gate evaluation.
///
/// **No numeric score, weight, or threshold is ever stored or serialized here.**
/// The Governor route/state is the entire agent-facing and durable contract.
#[derive(Debug, Clone, PartialEq)]
pub struct GovernorOutcome {
    /// Whether SMTP is permitted to proceed.
    pub allowed: bool,
    pub mode: GovernorMode,
    /// Raw Governor decision string (`allow`/`deny`/`review`) or a gate-internal
    /// status (`disabled`/`unavailable`/`unparseable`), or `attributes_required`
    /// / `attributes_invalid` for a pre-spawn attribute-declaration refusal.
    pub decision: String,
    pub state: Option<String>,
    pub review_ticket_id: Option<String>,
    /// Stable denial code when the send is blocked (None when allowed).
    pub block_code: Option<String>,
    pub block_reason: Option<String>,
    /// `review` or `deny` for a `governor_blocked` outcome (the next-actor signal).
    pub route: Option<String>,
    /// The resolved attribution sets/rejections, when the attribution protocol ran.
    pub resolution: Option<AttributionResolution>,
    /// Deterministic recovery suggestions (attribution failures / review).
    pub suggestions: Vec<Suggestion>,
    /// Originating surface, for surface-specific recovery instructions.
    pub surface: Option<SendSurface>,
    /// Sanitized one-line action echo for the recovery `reason`.
    pub action_echo: Option<String>,
    /// Whether a real draft was **atomically parked** for human review as part of
    /// this outcome. Set true ONLY by a caller that successfully performed the
    /// park transition (see [`Self::with_parked`]). Stateless immediate sends
    /// leave it `false`, so recovery never claims a nonexistent draft was parked.
    pub parked: bool,
    /// The parked draft's id, present only when [`Self::parked`] is true — the
    /// durable review handle. Never inferred from `draft_id` or the decision.
    pub parked_draft_id: Option<String>,
}

impl GovernorOutcome {
    /// A gate outcome with the attribution-protocol fields defaulted empty. Used
    /// by the legacy (non-attributed) construction paths.
    fn bare(
        allowed: bool,
        mode: GovernorMode,
        decision: &str,
        state: Option<String>,
        review_ticket_id: Option<String>,
        block_code: Option<String>,
        block_reason: Option<String>,
        route: Option<String>,
    ) -> Self {
        Self {
            allowed,
            mode,
            decision: decision.to_string(),
            state,
            review_ticket_id,
            block_code,
            block_reason,
            route,
            resolution: None,
            suggestions: Vec::new(),
            surface: None,
            action_echo: None,
            parked: false,
            parked_draft_id: None,
        }
    }

    /// Record that a real draft was atomically parked for human review as part of
    /// this outcome. Only a caller that actually performed the park transition may
    /// call this — it is what turns the recovery's review branch from "nothing was
    /// parked" into a truthful parked-draft review handle.
    pub fn with_parked(mut self, draft_id: &str) -> Self {
        self.parked = true;
        self.parked_draft_id = Some(draft_id.to_string());
        self
    }

    /// Sanitized JSON for **durable operator-facing** audit/event rows. Records
    /// the route/state and the three attribution sets + state + count — never a
    /// score, weight, threshold, or breakdown.
    pub fn audit_json(&self) -> Value {
        let mut obj = json!({
            "allowed": self.allowed,
            "mode": self.mode.as_str(),
            "decision": self.decision,
            "state": self.state,
            "review_ticket_id": self.review_ticket_id,
            "block_code": self.block_code,
            "block_reason": self.block_reason,
            "route": self.route,
        });
        if let (Value::Object(map), Some(res)) = (&mut obj, &self.resolution) {
            map.insert("attribution".into(), res.to_json());
            map.insert(
                "attribution_state".into(),
                Value::String(res.state.as_str().to_string()),
            );
            map.insert("attribute_count".into(), json!(res.governor_attrs.len()));
        }
        obj
    }

    /// Whether this is a pre-spawn attribute-declaration refusal (a missing/invalid
    /// `attributes` INPUT), not a Governor verdict.
    pub fn is_attribution_failure(&self) -> bool {
        matches!(
            self.block_code.as_deref(),
            Some("attributes_required") | Some("attributes_invalid")
        )
    }

    /// Top-level response status: `invalid` for attribution refusals, `blocked`
    /// for Governor/gate blocks.
    pub fn status_str(&self) -> &'static str {
        if self.is_attribution_failure() {
            "invalid"
        } else {
            "blocked"
        }
    }

    /// The additive attribution block for a **successful** (allowed) response.
    pub fn attribution_block(&self) -> Option<Value> {
        self.resolution.as_ref().map(AttributionResolution::to_json)
    }

    /// The additive `attribution` block for a **successful** outbound result: the
    /// resolved sets/rejections/state plus the Governor decision/route that
    /// permitted this send. `None` on legacy paths that never resolved
    /// attribution. Never carries a score, weight, threshold, body, raw
    /// recipient, secret, or attachment byte.
    pub fn success_attribution(&self) -> Option<Value> {
        self.resolution.as_ref().map(|res| {
            crate::attribution_persist::success_attribution_block(
                res,
                Some(self.decision.as_str()),
                self.route.as_deref(),
                false,
            )
        })
    }

    /// The narrowed agent-facing Governor block — `{decision, state, mode,
    /// review_ticket_id}` only. `score`, `allowed`, `block_code`, `block_reason`
    /// are intentionally absent (redundant or leaking).
    fn governor_block(&self) -> Value {
        json!({
            "decision": self.decision,
            "state": self.state,
            "mode": self.mode.as_str(),
            "review_ticket_id": self.review_ticket_id,
        })
    }

    /// The full canonical error object.
    ///
    /// A missing/invalid-attribute refusal (`attributes_required`/
    /// `attributes_invalid`) returns `{code, reason, attributes, help, recovery}`:
    /// the caller's declared/rejected INPUT plus self-contained `--help`-quality
    /// guidance. A genuine Governor block (`governor_blocked`/`governor_unavailable`)
    /// returns `{code, reason, [route], [governor], [attribution], recovery}` and is
    /// never handed irrelevant attribute help. Never contains a score.
    pub fn error_json(&self) -> Value {
        let code = self
            .block_code
            .clone()
            .unwrap_or_else(|| "governor_blocked".to_string());
        let mut obj = json!({
            "code": code,
            "reason": self.reason_string(),
            "recovery": self.recovery_json(),
        });
        if let Value::Object(map) = &mut obj {
            if self.is_attribution_failure() {
                // Missing/invalid INPUT: echo the declared/rejected attribute sets
                // and attach the recovery-complete `--help` guidance. The internal
                // three-set `attribution` resolution is intentionally NOT exposed
                // here — the caller needs the input to fix, not protocol internals.
                map.insert("attributes".into(), self.attributes_block());
                map.insert("help".into(), self.help_json());
            } else {
                if let Some(route) = &self.route {
                    map.insert("route".into(), Value::String(route.clone()));
                }
                // The narrowed Governor block only accompanies a genuine gate block.
                if self.block_code.as_deref() == Some("governor_blocked") {
                    map.insert("governor".into(), self.governor_block());
                }
                if let Some(attr) = self.attribution_block() {
                    map.insert("attribution".into(), attr);
                }
            }
        }
        obj
    }

    /// The public `attributes` INPUT block for a missing/invalid-declaration
    /// refusal: the keys the caller declared and any that were rejected (each with
    /// its stable per-key reason code and nearest-key `did_you_mean`). Distinct
    /// from the internal three-set `attribution` resolution echoed on a *successful*
    /// send — this is the input the caller must supply or correct.
    fn attributes_block(&self) -> Value {
        match &self.resolution {
            Some(res) => json!({
                "declared": res.declared_attrs,
                "rejected": res.rejected_attrs.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
            }),
            None => json!({ "declared": [], "rejected": [] }),
        }
    }

    /// The self-contained, `--help`-quality guidance attached to every
    /// missing/invalid-attribute refusal: what attributes are, both declaration
    /// syntaxes, concrete contextual examples, where to list the full catalog, and
    /// the honesty rules. Never contains a score, weight, threshold, body, raw
    /// recipient, or secret.
    fn help_json(&self) -> Value {
        let examples: Vec<Value> = self
            .suggestions
            .iter()
            .map(suggestion_example_json)
            .collect();
        json!({
            "what_are_attributes":
                "Attributes are bounded, factual signals Governor uses to assess the stakes and risk \
                 of THIS action. A signal may describe relevant context such as the relationship, \
                 message structure, content, authorization state, or consequences (for example \
                 `informational`, `financial_content`, or `reply_to_thread`). Envelope derives facts \
                 it can observe; declare every other catalog key you know is honestly true and omit \
                 the rest. Attributes do not request or guarantee permission, and they never expose \
                 Governor scoring.",
            "syntax": {
                "cli": "--attr <key> (repeatable, one per fact); e.g. `envelope send --to you@example.com --subject Hi --body \"…\" --attr informational`",
                "mcp": {
                    "field": "attributes",
                    "example": ["informational"],
                    "on": "send / reply / send_draft"
                }
            },
            "examples": examples,
            "list_attributes": {
                "mcp_tool": "governor_catalog",
                "cli": "envelope governor catalog --json",
                "skill": "envelope-governor-attribution"
            },
            "rules": HONESTY_RULES,
        })
    }

    /// The full canonical response `{status, error}` a blocked/invalid send
    /// returns to the agent across CLI `--json` and MCP.
    pub fn response_json(&self) -> Value {
        json!({ "status": self.status_str(), "error": self.error_json() })
    }

    /// Back-compatible alias: the error object for callers that wrap it in their
    /// own `{status, error}` envelope.
    pub fn denial_json(&self) -> Value {
        self.error_json()
    }

    /// Surface-specific description of exactly how to retry.
    fn retry_change(&self) -> &'static str {
        match self.surface {
            Some(SendSurface::Cli) => "re-run with a repeatable `--attr <key>` per factual key",
            _ => "retry with `attributes`: [<factual catalog keys>]",
        }
    }

    /// The exact, surface-appropriate retry shape (machine-friendly): a shell
    /// command fragment on the CLI, the JSON field on MCP.
    fn retry_example(&self) -> Value {
        match self.surface {
            Some(SendSurface::Cli) => json!("envelope send … --attr informational"),
            _ => json!({ "attributes": ["informational"] }),
        }
    }

    /// The recovery-complete `reason` string (§5.1 order): what failed, the
    /// sanitized action, exactly what to do next (parameter + ≥1 concrete key),
    /// where the catalog is, and what happens after.
    pub fn reason_string(&self) -> String {
        let echo = self
            .action_echo
            .clone()
            .unwrap_or_else(|| "this send".into());
        let param = match self.surface {
            Some(SendSurface::Cli) => {
                "re-run `envelope send` with `--attr informational` (a status/FYI note) or `--attr financial_content` (if it discusses money) — one `--attr <key>` per fact true of this message"
            }
            _ => {
                "retry the same send/reply/send_draft call with an `attributes` array — e.g. `informational` for a status/FYI note, or `financial_content` if it discusses money"
            }
        };
        let catalog = "Full catalog: the `governor_catalog` tool, `envelope governor catalog --json`, or the envelope-governor-attribution skill.";
        match self.block_code.as_deref() {
            Some("attributes_required") => format!(
                "This send is missing its required `attributes`: every send must declare at least one factual attribute, and none were provided. This attempt: {echo}. {param}. {catalog} On success the send queues in the outbox for the safety cooldown; if honestly-declared facts route to review, the draft parks for human approval."
            ),
            Some("attributes_invalid") => {
                let rejected = self
                    .resolution
                    .as_ref()
                    .map(|r| r.rejected_attrs.as_slice())
                    .unwrap_or(&[]);
                let total = self
                    .resolution
                    .as_ref()
                    .map(|r| r.declared_attrs.len())
                    .unwrap_or(rejected.len());
                let clauses: Vec<String> = rejected.iter().map(reject_clause).collect();
                format!(
                    "{} of {} declared attribute(s) were rejected: {}. Correct or remove the rejected keys ({}) and keep the valid ones. {catalog}",
                    rejected.len(),
                    total,
                    clauses.join("; "),
                    self.retry_change(),
                )
            }
            Some("governor_unavailable") => format!(
                "The Governor gate is unavailable: {}. This is an operator/infrastructure issue, not an attribution problem — notify the operator and do not spend retries; no attributes will change it.",
                self.block_reason
                    .as_deref()
                    .unwrap_or("gate could not be evaluated")
            ),
            _ => {
                // governor_blocked (review or deny)
                if self.route.as_deref() == Some("deny") {
                    format!(
                        "Governor routed this send to deny: {echo}. Do not retry unchanged — only new true facts or a human decision change the outcome."
                    )
                } else if self.parked {
                    // A real draft was atomically parked — claim it, with its id.
                    let handle = self
                        .parked_draft_id
                        .as_deref()
                        .map(|id| format!("draft {id}"))
                        .unwrap_or_else(|| "the draft".to_string());
                    format!(
                        "Governor routed this send to review: {echo}. {handle} is parked pending_review. A human can approve it in the dashboard (Drafts → pending review); approval re-queues it. If a declared fact was wrong, correct the draft — an edit resets attribution."
                    )
                } else {
                    // Stateless/immediate review: no draft exists to park.
                    format!(
                        "Governor routed this send to review: {echo}. Nothing was sent, created, or parked. To obtain human review, re-submit as a normal queued send (omit the immediate-send bypass) — a queued send that routes to review parks as a draft for approval — or save it draft-only. Correcting a wrong declared fact also changes the outcome."
                    )
                }
            }
        }
    }

    /// The machine-readable `recovery` block.
    ///
    /// For a missing/invalid-attribute refusal this stays compact and
    /// machine-friendly — `next_action` (with the exact retry shape) plus an
    /// idempotency-affirming `retry` note. Definitions, examples, catalog pointers,
    /// and rules live in the `help` block, never buried only here.
    pub fn recovery_json(&self) -> Value {
        let surface = self
            .surface
            .map(SendSurface::as_str)
            .unwrap_or("mcp")
            .to_string();
        match self.block_code.as_deref() {
            Some("attributes_required") | Some("attributes_invalid") => json!({
                "next_action": {
                    "surface": surface,
                    "change": self.retry_change(),
                    "example": self.retry_example()
                },
                "retry": {
                    "idempotent": true,
                    "note": "nothing was sent or created; retry the corrected request"
                }
            }),
            Some("governor_unavailable") => json!({
                "next_action": { "surface": "operator", "action": "notify_operator" },
                "retry": {
                    "idempotent": false,
                    "note": "gate infrastructure failure; retries will not help until the operator restores Governor"
                }
            }),
            _ if self.route.as_deref() == Some("deny") => json!({
                "next_action": { "surface": surface, "change": "do not retry unchanged" },
                "suggested_attrs": [],
                "retry": {
                    "idempotent": false,
                    "note": "only new true facts or a human decision change a deny"
                }
            }),
            _ if self.parked => {
                // A real draft was atomically parked: the review handle is genuine.
                let mut next = json!({
                    "surface": "dashboard",
                    "action": "human_approval",
                    "path": "/drafts?status=pending_review",
                    "then": "a human approves the parked draft and the sweep re-evaluates it"
                });
                if let (Value::Object(map), Some(id)) = (&mut next, &self.parked_draft_id) {
                    map.insert("draft_id".into(), Value::String(id.clone()));
                }
                json!({
                    "next_action": next,
                    "suggested_attrs": self.suggestions.iter().map(Suggestion::to_json).collect::<Vec<_>>(),
                    "retry": {
                        "idempotent": false,
                        "note": "resubmitting identical facts returns the same route; only new true facts or human approval change the outcome"
                    }
                })
            }
            _ => json!({
                // Stateless/immediate review: no draft was created or parked, so
                // there is nothing to approve and no review handle to link.
                "next_action": {
                    "surface": surface,
                    "action": "queue_for_review",
                    "change": "re-submit as a normal queued send (omit the immediate-send bypass) so a review routes to a parked draft, or save it draft-only for human approval"
                },
                "suggested_attrs": self.suggestions.iter().map(Suggestion::to_json).collect::<Vec<_>>(),
                "retry": {
                    "idempotent": true,
                    "note": "no draft was created and nothing was sent or parked; re-submit as a queued send for human review"
                }
            }),
        }
    }
}

/// Render one deterministic suggestion as a `--help` example, keeping its key, a
/// plain-language description (from the vendored catalog), when-to-use language,
/// and provenance — never a bare key. This is what makes a missing-attribute
/// error usable without a second discovery round-trip.
fn suggestion_example_json(s: &Suggestion) -> Value {
    let when = s
        .declare_if
        .clone()
        .or_else(|| s.note.clone())
        .unwrap_or_else(|| "declare when this fact is true of the message".to_string());
    let mut obj = json!({
        "key": s.key,
        "description": crate::governor_catalog::description_of(&s.key).unwrap_or(""),
        "when": when,
        "provenance": s.provenance,
    });
    if let (Value::Object(map), Some(note)) = (&mut obj, &s.note) {
        map.insert("note".into(), Value::String(note.clone()));
    }
    obj
}

/// One human-readable clause for a rejected declaration (used in `reason`).
fn reject_clause(r: &crate::attribution::RejectedAttr) -> String {
    match r.code.as_str() {
        "unknown_attribute" => {
            let dym = r
                .did_you_mean
                .first()
                .map(|k| format!(" (did you mean `{k}`?)"))
                .unwrap_or_default();
            format!("`{}` is not in catalog envelope{dym}", r.key)
        }
        "attestation_required" => {
            format!(
                "`{}` is attestation-only and can never be agent-declared",
                r.key
            )
        }
        "conflicts_with_host_observation" => format!(
            "`{}` contradicts the host observation ({})",
            r.key,
            r.detail.as_deref().unwrap_or("host-observed fact")
        ),
        "host_verification_unavailable" => format!(
            "`{}` could not be verified by the host ({})",
            r.key,
            r.detail.as_deref().unwrap_or(
                "declare a fact Envelope can corroborate or an author-context attribute"
            )
        ),
        "conflicting_attributes" => format!(
            "`{}` is an impossible combination ({})",
            r.key,
            r.detail.as_deref().unwrap_or("mutually exclusive")
        ),
        other => format!("`{}` was rejected ({other})", r.key),
    }
}

/// Decision fields parsed from Governor's `score --json` output.
///
/// The numeric `score` Governor may emit is **not** parsed or retained — the
/// route/state is the entire Envelope-side contract, and no score reaches any
/// Envelope payload.
#[derive(Debug, Clone, PartialEq)]
pub struct GovernorVerdict {
    pub decision: String,
    pub state: Option<String>,
    pub review_ticket_id: Option<String>,
}

/// Parse Governor's JSON output into a verdict. Returns `None` if the output is
/// not parseable JSON with a `decision` field. Any `score` field present in the
/// output is deliberately ignored.
pub fn parse_governor_verdict(stdout: &str) -> Option<GovernorVerdict> {
    let value: Value = serde_json::from_str(stdout.trim()).ok()?;
    let decision = value.get("decision")?.as_str()?.to_ascii_lowercase();
    let state = value
        .get("state")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let review_ticket_id = value
        .get("review_ticket")
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Some(GovernorVerdict {
        decision,
        state,
        review_ticket_id,
    })
}

/// The route (`review`/`deny`) implied by a non-allow decision.
fn route_for(decision: &str) -> Option<String> {
    match decision {
        "review" | "review_required" => Some("review".to_string()),
        "deny" | "denied" | "block" | "blocked" => Some("deny".to_string()),
        _ => None,
    }
}

/// Apply a parsed verdict against the gate mode to produce the final outcome.
pub fn decide_from_verdict(mode: GovernorMode, verdict: GovernorVerdict) -> GovernorOutcome {
    let permitted = matches!(verdict.decision.as_str(), "allow" | "allowed");
    let allowed = permitted || mode == GovernorMode::Warn;
    let block_reason = if permitted {
        None
    } else {
        Some(format!(
            "governor decision '{}'{} did not permit this send",
            verdict.decision,
            verdict
                .state
                .as_deref()
                .map(|s| format!(" (state '{s}')"))
                .unwrap_or_default()
        ))
    };
    let route = route_for(&verdict.decision);
    GovernorOutcome::bare(
        allowed,
        mode,
        &verdict.decision,
        verdict.state,
        verdict.review_ticket_id,
        // In warn mode we never block even on a non-allow verdict.
        if allowed {
            None
        } else {
            Some("governor_blocked".to_string())
        },
        if allowed { None } else { block_reason },
        route,
    )
}

fn fail_outcome(mode: GovernorMode, decision: &str, reason: &str) -> GovernorOutcome {
    let allowed = mode == GovernorMode::Warn;
    GovernorOutcome::bare(
        allowed,
        mode,
        decision,
        None,
        None,
        if allowed {
            None
        } else {
            Some("governor_unavailable".to_string())
        },
        if allowed {
            None
        } else {
            Some(reason.to_string())
        },
        None,
    )
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
        return off_outcome();
    }
    spawn_and_interpret(config, &req.attributes, &req.justification())
}

/// The gate with the full attribution protocol: it refuses an empty/invalid
/// declared+derived set **before** Governor is ever spawned, and only submits a
/// validated, non-empty union for scoring.
///
/// This is the entry point every bot-originated actual-send surface uses. It
/// requires `req.resolution` to be set (via
/// [`GovernorRequest::from_context_with_declared`]); without it, it falls back to
/// the legacy raw [`gate`].
pub fn gate_with_attribution(config: &GovernorConfig, req: &GovernorRequest) -> GovernorOutcome {
    if config.mode == GovernorMode::Off {
        // Off explicitly disables both the gate and the attribution requirement.
        let mut o = off_outcome();
        o.surface = Some(req.surface);
        o.resolution = req.resolution.clone();
        return o;
    }
    let Some(resolution) = req.resolution.clone() else {
        return gate(config, req);
    };

    if resolution.state != AttributionState::Attributed {
        // Governor is NEVER invoked for an unattributed or invalid request.
        return attribution_failure_outcome(config.mode, req, resolution);
    }

    let mut outcome = spawn_and_interpret(config, &resolution.governor_attrs, &req.justification());
    outcome.surface = Some(req.surface);
    outcome.action_echo = Some(req.action_echo());
    outcome.resolution = Some(resolution);
    if outcome.block_code.as_deref() == Some("governor_blocked") {
        outcome.suggestions = suggestions_for(req, outcome.route.as_deref() == Some("review"));
    }
    outcome
}

/// The allowed, gate-skipped outcome for `off` mode.
fn off_outcome() -> GovernorOutcome {
    GovernorOutcome::bare(
        true,
        GovernorMode::Off,
        "disabled",
        None,
        None,
        None,
        None,
        None,
    )
}

/// Build a pre-spawn attribution refusal outcome. Governor is not invoked.
///
/// Attribution is Envelope's own protocol precondition, distinct from Governor
/// scoring. It fails closed in **every enforcing mode** — `warn` softens a
/// Governor *verdict* on an already-attributed request, but it never waives the
/// attribution precondition: a missing/invalid declaration on a bot-originated
/// action always blocks (`allowed == false`), never reaching Governor or SMTP.
fn attribution_failure_outcome(
    mode: GovernorMode,
    req: &GovernorRequest,
    resolution: AttributionResolution,
) -> GovernorOutcome {
    let code = resolution.failure_code().unwrap_or("attributes_required");
    let mut o = GovernorOutcome::bare(
        false,
        mode,
        code,
        None,
        None,
        Some(code.to_string()),
        Some(format!("attribution refused: {code}")),
        None,
    );
    o.surface = Some(req.surface);
    o.action_echo = Some(req.action_echo());
    o.suggestions = suggestions_for(req, false);
    o.resolution = Some(resolution);
    o
}

/// Deterministic suggestions for this request's context (reconstructs a minimal
/// sanitized context from the sanitized request fields — no content needed).
fn suggestions_for(req: &GovernorRequest, route_is_review: bool) -> Vec<Suggestion> {
    let sctx = AttributedSendContext {
        account_domain: req.account_domain.clone(),
        recipient_domains: req.recipient_domains.clone(),
        recipient_count: req.recipient_count,
        attachment_count: req.attachment_count,
        is_reply: req.is_reply,
        ..Default::default()
    };
    let rejected = req
        .resolution
        .as_ref()
        .map(|r| r.rejected_attrs.clone())
        .unwrap_or_default();
    let has_attestation = req.attributes.iter().any(|a| a == "tyler_approved");
    attribution_suggest::suggest(&sctx, &rejected, route_is_review, has_attestation)
}

/// Spawn `governor score --catalog envelope --attr <k> ... --json` over the
/// given validated attribute set: pure blind attribution. The `--just` string is
/// logged by Governor but never scored, and carries only the surface + draft id
/// (no PII). Fails closed on spawn/exit/parse error per mode.
fn spawn_and_interpret(
    config: &GovernorConfig,
    attrs: &[String],
    justification: &str,
) -> GovernorOutcome {
    let mut command = std::process::Command::new(&config.bin);
    command
        .arg("score")
        .arg("--catalog")
        .arg(GOVERNOR_CATALOG)
        .arg("--json");
    for attr in attrs {
        command.arg("--attr").arg(attr);
    }
    command.arg("--just").arg(justification);

    match command.output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            interpret_governor_invocation(config.mode, out.status.success(), &stdout)
        }
        Err(_) => fail_outcome(
            config.mode,
            "unavailable",
            "governor binary could not be executed",
        ),
    }
}

/// Interpret a completed `governor score` invocation.
///
/// The process exit status is authoritative and is checked BEFORE stdout: a
/// nonzero exit fails closed (per mode) even when stdout happens to contain a
/// parseable `allow` — a crashed, partially-executed, or tampered Governor
/// must never grant a send on the strength of whatever it printed first.
pub fn interpret_governor_invocation(
    mode: GovernorMode,
    exited_successfully: bool,
    stdout: &str,
) -> GovernorOutcome {
    if !exited_successfully {
        return fail_outcome(mode, "unavailable", "governor exited with a failure status");
    }
    match parse_governor_verdict(stdout) {
        Some(verdict) => decide_from_verdict(mode, verdict),
        None => fail_outcome(
            mode,
            "unparseable",
            "governor produced no parseable decision",
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

    // ── Governor invocation interpretation (exit status before stdout) ──

    #[test]
    fn nonzero_exit_fails_closed_even_with_parseable_allow_stdout() {
        let allow_json = r#"{"decision": "allow", "state": "green", "score": 0.1}"#;
        // Sanity: with a successful exit this stdout would allow.
        assert!(interpret_governor_invocation(GovernorMode::Required, true, allow_json).allowed);

        // A failure exit must override the parseable allow.
        let outcome = interpret_governor_invocation(GovernorMode::Required, false, allow_json);
        assert!(!outcome.allowed, "failed exit must never grant a send");
        assert_eq!(outcome.decision, "unavailable");
        assert_eq!(outcome.block_code.as_deref(), Some("governor_unavailable"));

        // Warn mode records the failure but does not block (existing warn
        // semantics preserved).
        let warn = interpret_governor_invocation(GovernorMode::Warn, false, allow_json);
        assert!(warn.allowed);
        assert_eq!(warn.decision, "unavailable");
    }

    #[test]
    fn successful_exit_with_unparseable_stdout_still_fails_closed() {
        let outcome = interpret_governor_invocation(GovernorMode::Required, true, "not json");
        assert!(!outcome.allowed);
        assert_eq!(outcome.decision, "unparseable");
    }

    /// End-to-end fixture: a local throwaway executable that prints a valid
    /// `allow` verdict but exits nonzero must be refused by the real gate in
    /// required mode. No secrets, no network — the fixture is a two-line shell
    /// script in the test temp dir.
    #[cfg(unix)]
    #[test]
    fn gate_refuses_allow_stdout_from_failing_governor_process() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "envelope-governor-exit-fixture-{}.sh",
            std::process::id()
        ));
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "#!/bin/sh").unwrap();
            writeln!(f, "echo '{{\"decision\": \"allow\"}}'").unwrap();
            writeln!(f, "exit 3").unwrap();
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let config = GovernorConfig {
            mode: GovernorMode::Required,
            bin: path.to_string_lossy().into_owned(),
        };
        let req = GovernorRequest::build(
            "acc1",
            Some("example.com"),
            "subject",
            "to@example.net",
            None,
            None,
            SendSurface::Scheduled,
            Some("draft-1"),
            &[],
            false,
        );
        let outcome = gate(&config, &req);
        let _ = std::fs::remove_file(&path);

        assert!(
            !outcome.allowed,
            "required mode must fail closed on nonzero exit despite allow stdout"
        );
        assert_eq!(outcome.block_code.as_deref(), Some("governor_unavailable"));
    }

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
    fn agent_id_defaults_none_and_is_omitted_from_audit_payload() {
        let req = sample_request();
        assert_eq!(req.agent_id, None);
        let payload = req.audit_payload();
        let obj = payload.as_object().unwrap();
        assert!(
            !obj.contains_key("agent_id"),
            "agent_id must be absent when None: {payload}"
        );
    }

    #[test]
    fn agent_id_is_audit_metadata_only_never_a_scoring_input() {
        let base = sample_request();
        let attributed = sample_request().with_agent_id(Some("skippy-agent-42"));

        // Threading agent_id must not touch the scored attribute keys.
        assert_eq!(base.attributes, attributed.attributes);
        assert!(
            !attributed
                .attributes
                .iter()
                .any(|a| a.contains("skippy-agent-42")),
            "agent_id must never enter Governor scoring attributes"
        );

        // It appears only in the audit payload, and only when Some.
        let payload = attributed.audit_payload();
        assert_eq!(
            payload.get("agent_id"),
            Some(&json!("skippy-agent-42")),
            "agent_id should serialize into the audit payload when set"
        );

        // Redaction of recipients/subject still holds with agent_id present.
        let audit = payload.to_string();
        for needle in ["alice@example.com", "bob@example.com", "Quarterly numbers"] {
            assert!(!audit.contains(needle), "audit leaked {needle}: {audit}");
        }
        assert!(audit.contains("sha256:"));

        // Every field except agent_id is byte-identical to the un-threaded payload.
        let mut with_id = payload.as_object().unwrap().clone();
        with_id.remove("agent_id");
        assert_eq!(Value::Object(with_id), base.audit_payload());
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

    fn attributed_req(declared: &[&str], require: bool) -> GovernorRequest {
        let ctx = AttributedSendContext {
            account_domain: Some("martin.fm".into()),
            recipient_domains: vec!["acme.example".into()],
            recipient_count: 1,
            attachment_count: 1,
            sensitive_attachment: true,
            ..Default::default()
        };
        let decl: Vec<String> = declared.iter().map(|s| s.to_string()).collect();
        GovernorRequest::from_context_with_declared(
            "acct-1",
            "Subj",
            SendSurface::Mcp,
            Some("d-1"),
            &[("application/pdf".into(), 10)],
            &ctx,
            &decl,
            require,
        )
    }

    fn nonexistent_required() -> GovernorConfig {
        GovernorConfig {
            mode: GovernorMode::Required,
            bin: "/nonexistent/governor-binary-xyz".to_string(),
        }
    }

    #[test]
    fn empty_declaration_refused_before_governor_is_spawned() {
        // A rich derived context (attachment, external recipient) but zero
        // declarations must be attributes_required, and Governor must NOT be
        // spawned (a missing binary would otherwise yield governor_unavailable).
        let req = attributed_req(&[], true);
        let outcome = gate_with_attribution(&nonexistent_required(), &req);
        assert!(!outcome.allowed);
        assert_eq!(outcome.block_code.as_deref(), Some("attributes_required"));
        assert_ne!(
            outcome.decision, "unavailable",
            "Governor must never be spawned for an unattributed request"
        );
        assert_eq!(outcome.status_str(), "invalid");

        // The reason string ALONE is recovery-complete (J8): it names the
        // parameter, a concrete key, and the catalog tool.
        let reason = outcome.reason_string();
        assert!(
            reason.contains("attributes"),
            "names the parameter: {reason}"
        );
        assert!(
            reason.contains("financial_content") || reason.contains("informational"),
            "names a concrete key: {reason}"
        );
        assert!(
            reason.contains("governor_catalog"),
            "names the catalog: {reason}"
        );

        // Suggestions include at least one risk key.
        assert!(
            outcome
                .suggestions
                .iter()
                .any(|s| crate::attribution_suggest::is_risk_key(&s.key)),
            "must include a risk key"
        );
    }

    #[test]
    fn typo_declaration_refused_before_spawn_with_did_you_mean() {
        let req = attributed_req(&["informationl"], true);
        let outcome = gate_with_attribution(&nonexistent_required(), &req);
        assert!(!outcome.allowed);
        assert_eq!(outcome.block_code.as_deref(), Some("attributes_invalid"));
        assert_ne!(outcome.decision, "unavailable");
        let payload = outcome.response_json().to_string();
        assert!(payload.contains("did_you_mean"));
        assert!(payload.contains("informational"));
    }

    #[test]
    fn attributes_required_error_is_help_complete() {
        // The missing-INPUT refusal must be self-contained `--help`-quality: the
        // caller learns what attributes are, both declaration syntaxes, concrete
        // contextual examples, where to list the catalog, and the honesty rules —
        // without a second round-trip.
        let req = attributed_req(&[], true);
        let outcome = gate_with_attribution(&nonexistent_required(), &req);
        assert_eq!(outcome.block_code.as_deref(), Some("attributes_required"));

        let resp = outcome.response_json();
        assert_eq!(resp["status"], "invalid");
        let err = &resp["error"];
        assert_eq!(err["code"], "attributes_required");

        // reason is recovery-complete on its own (survives a double-encode).
        let reason = err["reason"].as_str().unwrap();
        assert!(
            reason.contains("attribute"),
            "reason names the input: {reason}"
        );
        assert!(
            reason.contains("governor_catalog"),
            "reason points to catalog: {reason}"
        );

        // `attributes` echoes the declared/rejected INPUT sets (not the internal
        // three-set resolution).
        assert!(err["attributes"]["declared"].is_array());
        assert!(err["attributes"]["rejected"].is_array());

        // help: plain-language definition.
        let help = &err["help"];
        assert!(
            help["what_are_attributes"].as_str().unwrap().len() > 60,
            "definition must be plain language"
        );
        // help.syntax: BOTH exact surfaces.
        assert!(help["syntax"]["cli"].as_str().unwrap().contains("--attr"));
        assert_eq!(help["syntax"]["mcp"]["field"], "attributes");
        assert!(
            help["syntax"]["mcp"]["example"]
                .as_array()
                .unwrap()
                .iter()
                .any(|k| k == "informational")
        );
        // help.examples: >=1 contextual suggestion, each with key/description/when.
        let examples = help["examples"].as_array().unwrap();
        assert!(
            !examples.is_empty(),
            "missing-attribute errors must always include contextual examples"
        );
        for ex in examples {
            assert!(ex["key"].is_string(), "example has a key");
            assert!(
                ex["description"].is_string(),
                "example retains a description"
            );
            assert!(
                ex["when"].is_string(),
                "example retains when-to-use language"
            );
        }
        // help.list_attributes: all three discovery pointers.
        assert_eq!(help["list_attributes"]["mcp_tool"], "governor_catalog");
        assert_eq!(
            help["list_attributes"]["cli"],
            "envelope governor catalog --json"
        );
        assert_eq!(
            help["list_attributes"]["skill"],
            "envelope-governor-attribution"
        );
        // help.rules: honesty/all-or-nothing rules in plain language.
        assert!(help["rules"].as_array().unwrap().len() >= 3);

        // recovery stays compact + machine-friendly: next_action + retry only.
        let recovery = &err["recovery"];
        assert!(recovery["next_action"].is_object());
        assert_eq!(recovery["retry"]["idempotent"], true);
        let retry_note = recovery["retry"]["note"].as_str().unwrap();
        assert!(
            retry_note.contains("nothing was sent") || retry_note.contains("no draft"),
            "retry note states nothing was created/sent: {retry_note}"
        );
        assert!(
            recovery.get("suggested_attrs").is_none()
                && recovery.get("catalog").is_none()
                && recovery.get("rules").is_none(),
            "definitions/catalog/examples belong in help, not buried in recovery"
        );

        // No leakage of scoring internals anywhere in the payload, and no message
        // content: the fixture's subject `Subj` must never surface (only the
        // sanitized recipient counts/domains may). The CLI syntax example uses a
        // documentation placeholder address, so a raw-recipient check targets the
        // fixture's own content, not the static help text.
        let text = resp.to_string();
        for banned in ["score", "weight", "threshold", "Subj"] {
            assert!(!text.contains(banned), "help payload leaked `{banned}`");
        }
    }

    #[test]
    fn attributes_invalid_error_lists_rejected_keys() {
        let req = attributed_req(&["informationl"], true); // typo
        let outcome = gate_with_attribution(&nonexistent_required(), &req);
        assert_eq!(outcome.block_code.as_deref(), Some("attributes_invalid"));

        let resp = outcome.response_json();
        let err = &resp["error"];
        assert_eq!(err["code"], "attributes_invalid");

        // The rejected key + its per-key reason + nearest suggestion are in an
        // obvious `attributes.rejected` structure.
        let rejected = err["attributes"]["rejected"].as_array().unwrap();
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0]["key"], "informationl");
        assert_eq!(rejected[0]["code"], "unknown_attribute");
        assert!(
            rejected[0]["did_you_mean"]
                .as_array()
                .unwrap()
                .iter()
                .any(|k| k == "informational")
        );
        // declared echoes exactly what the caller submitted.
        assert!(
            err["attributes"]["declared"]
                .as_array()
                .unwrap()
                .iter()
                .any(|k| k == "informationl")
        );
        // Same help affordances as the required case.
        assert!(!err["help"]["examples"].as_array().unwrap().is_empty());
        // reason tells the caller to correct/remove the rejected key.
        let reason = err["reason"].as_str().unwrap();
        assert!(
            reason.contains("informationl"),
            "reason names rejected key: {reason}"
        );
    }

    #[test]
    fn governor_block_error_omits_attribute_help() {
        // A genuine Governor deny is not a missing-input error — it must NOT be
        // handed irrelevant attribute help or an `attributes` input block.
        let denied = decide_from_verdict(
            GovernorMode::Required,
            GovernorVerdict {
                decision: "deny".to_string(),
                state: None,
                review_ticket_id: None,
            },
        );
        let resp = denied.response_json();
        assert_eq!(resp["status"], "blocked");
        assert!(
            resp["error"].get("help").is_none(),
            "governor deny must not receive attribute help"
        );
        assert!(
            resp["error"].get("attributes").is_none(),
            "governor deny is not a missing-input error"
        );
    }

    #[test]
    fn attested_request_spawns_and_fails_closed_when_unavailable() {
        // A validly-declared request reaches Governor; a missing binary is a
        // governor_unavailable (operator) failure — distinct from every
        // attribution_* shape. (J11)
        let req = attributed_req(&["financial_content"], true);
        let outcome = gate_with_attribution(&nonexistent_required(), &req);
        assert!(!outcome.allowed);
        assert_eq!(outcome.block_code.as_deref(), Some("governor_unavailable"));
        assert!(!outcome.is_attribution_failure());
        // An infrastructure failure is not a missing-input error: no attribute help.
        let resp = outcome.response_json();
        assert!(resp["error"].get("help").is_none());
        assert!(resp["error"].get("attributes").is_none());
    }

    #[test]
    fn no_score_appears_in_any_attribution_payload() {
        for declared in [vec![], vec!["informationl"], vec!["financial_content"]] {
            let req = attributed_req(&declared, true);
            let outcome = gate_with_attribution(&nonexistent_required(), &req);
            let payload = outcome.response_json().to_string();
            let audit = outcome.audit_json().to_string();
            assert!(!payload.contains("\"score\""), "agent payload leaked score");
            assert!(!audit.contains("\"score\""), "audit payload leaked score");
        }
    }

    #[test]
    fn warn_mode_fails_closed_on_attribution_failure() {
        // Warn mode NEVER waives the attribution precondition: a bot-originated
        // request with no declaration is blocked with attributes_required, and
        // Governor is never spawned — identical to required mode for this refusal.
        let config = GovernorConfig {
            mode: GovernorMode::Warn,
            bin: "/nonexistent/governor-binary-xyz".to_string(),
        };
        let req = attributed_req(&[], true);
        let outcome = gate_with_attribution(&config, &req);
        assert!(
            !outcome.allowed,
            "warn must fail closed on a missing declaration"
        );
        assert_eq!(outcome.block_code.as_deref(), Some("attributes_required"));
        assert_ne!(
            outcome.decision, "unavailable",
            "Governor must never be spawned for an unattributed request, even in warn"
        );
        assert_eq!(outcome.status_str(), "invalid");
    }

    #[test]
    fn warn_mode_fails_closed_on_invalid_declaration() {
        let config = GovernorConfig {
            mode: GovernorMode::Warn,
            bin: "/nonexistent/governor-binary-xyz".to_string(),
        };
        let req = attributed_req(&["informationl"], true); // typo → invalid
        let outcome = gate_with_attribution(&config, &req);
        assert!(
            !outcome.allowed,
            "warn must fail closed on an invalid declaration"
        );
        assert_eq!(outcome.block_code.as_deref(), Some("attributes_invalid"));
        assert_ne!(outcome.decision, "unavailable");
    }

    #[test]
    fn warn_mode_softens_governor_verdict_on_attributed_request() {
        // The carve-out warn IS allowed to keep: a VALID attributed request whose
        // Governor verdict is deny/review/unavailable proceeds (record but never
        // block). Attribution passed first; only the Governor verdict is softened.
        let denied = decide_from_verdict(
            GovernorMode::Warn,
            GovernorVerdict {
                decision: "deny".to_string(),
                state: None,
                review_ticket_id: None,
            },
        );
        assert!(
            denied.allowed,
            "warn softens a Governor deny on a valid request"
        );

        let unavailable = fail_outcome(GovernorMode::Warn, "unavailable", "gate down");
        assert!(
            unavailable.allowed,
            "warn softens a governor_unavailable on a valid request"
        );
    }

    #[test]
    fn parse_verdict_extracts_decision_state_ticket_and_ignores_score() {
        let stdout = r#"{
            "decision": "review",
            "state": "review_required",
            "score": -0.04,
            "review_ticket": { "id": "review-123", "path": "/x" }
        }"#;
        let v = parse_governor_verdict(stdout).unwrap();
        assert_eq!(v.decision, "review");
        assert_eq!(v.state.as_deref(), Some("review_required"));
        assert_eq!(v.review_ticket_id.as_deref(), Some("review-123"));
        // The score in the output is deliberately not retained anywhere: no
        // `"score"` key appears in any durable or agent-facing payload.
        let outcome = decide_from_verdict(GovernorMode::Required, v);
        assert!(!outcome.audit_json().to_string().contains("\"score\""));
        assert!(!outcome.error_json().to_string().contains("\"score\""));
        assert!(!outcome.response_json().to_string().contains("\"score\""));
    }

    #[test]
    fn parse_verdict_rejects_non_json() {
        assert!(parse_governor_verdict("not json").is_none());
        assert!(parse_governor_verdict("{}").is_none());
    }

    // ── Block 4: truthful parking ───────────────────────────────────────────

    fn review_outcome(surface: SendSurface) -> GovernorOutcome {
        let mut o = decide_from_verdict(
            GovernorMode::Required,
            GovernorVerdict {
                decision: "review".to_string(),
                state: Some("review_required".to_string()),
                review_ticket_id: None,
            },
        );
        o.surface = Some(surface);
        o.action_echo = Some("new message to 1 external recipient(s), no attachments".to_string());
        o
    }

    #[test]
    fn stateless_review_never_claims_a_draft_was_parked() {
        // A stateless immediate send (no draft created) routed to review must not
        // claim the draft is parked, nor link a nonexistent pending_review draft.
        let o = review_outcome(SendSurface::Mcp);
        assert!(!o.parked, "no atomic park happened");
        let reason = o.reason_string();
        assert!(
            !reason.contains("is parked") && !reason.contains("pending_review"),
            "stateless review must not claim the draft is parked: {reason}"
        );
        assert!(
            reason.contains("Nothing was sent, created, or parked"),
            "must state nothing was created: {reason}"
        );
        let recovery = o.recovery_json();
        let recovery_s = recovery.to_string();
        assert!(
            !recovery_s.contains("pending_review"),
            "recovery must not link a nonexistent parked draft: {recovery_s}"
        );
        // The recovery is explicitly idempotent: nothing to clean up.
        assert_eq!(recovery["retry"]["idempotent"], serde_json::json!(true));
    }

    #[test]
    fn parked_review_reports_the_real_draft_handle() {
        // Only after a successful atomic park does the outcome claim parking, with
        // the real draft id and the dashboard review path.
        let o = review_outcome(SendSurface::Scheduled).with_parked("draft-42");
        assert!(o.parked);
        let reason = o.reason_string();
        assert!(
            reason.contains("parked"),
            "parked review claims parking: {reason}"
        );
        assert!(
            reason.contains("draft-42"),
            "names the real draft: {reason}"
        );
        let recovery = o.recovery_json();
        assert_eq!(
            recovery["next_action"]["path"],
            "/drafts?status=pending_review"
        );
        assert_eq!(recovery["next_action"]["draft_id"], "draft-42");
    }
}
