// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Durable, revision-keyed attribution declaration persisted in draft metadata.
//!
//! A bot-originated send that is *queued* (outbox cooldown) or *scheduled*
//! (`--at`) is validated for attribution at queue time, but the actual Governor
//! decision and SMTP transmission happen later, in the scheduled-send sweep.
//! Between those two moments the bot is gone: the sweep cannot ask it to
//! re-declare. So the validated declaration must travel with the draft.
//!
//! This module is the honest carrier. It persists the bot's *declared* attribute
//! keys (plus protocol/catalog version and bounded attempt state) into the draft
//! metadata under the [`ATTRIBUTION_METADATA_KEY`] key, keyed to the draft
//! `revision` at which it was validated. Two invariants fall out of the
//! revision-keying for free:
//!
//! 1. **A material draft revision invalidates the declaration.** Recipients,
//!    subject/body, or attachment edits bump `draft.revision` (see the store's
//!    `update_draft_content`/`update_draft_attachments`), so a persisted
//!    declaration whose `revision` no longer matches the row is *stale* and is
//!    treated as no declaration at all — never carried across a material change.
//! 2. **Attempt state resets on the same boundary.** The bounded attribution
//!    correction counter lives inside the same revision-keyed block, so a
//!    material revision resets it too (a stale block's attempts are ignored).
//!
//! Nothing here embeds a score, weight, or threshold: only the declared keys,
//! the catalog/version they were validated against, and the attempt bookkeeping.

use serde_json::{Value, json};

use crate::attribution::AttributionResolution;
use crate::governor_catalog::{ATTRIBUTION_PROTOCOL, CATALOG_NAME, catalog_version};

/// Draft-metadata key under which the persisted declaration lives.
pub const ATTRIBUTION_METADATA_KEY: &str = "attribution";

/// Documented bound on attribution correction attempts at SMTP time before a
/// bot-originated queued draft is parked for human review. The Nth attempt that
/// still fails attribution (N == this value) parks; attempts below it retry.
pub const MAX_ATTRIBUTION_ATTEMPTS: u32 = 3;

/// Stable `park_reason` recorded when a draft exhausts its attribution attempts.
pub const PARK_REASON_ATTRIBUTION_EXHAUSTED: &str = "attribution_exhausted";

/// Who took responsibility for the declaration carried on a queued draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclarationOrigin {
    /// An AI agent declared factual attributes; the sweep requires a valid
    /// declaration and never lets host-derived facts substitute for it.
    Bot,
    /// A human durably attested the send (revision-bound dashboard approval); the
    /// sweep does not require a bot declaration for it.
    Human,
}

impl DeclarationOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bot => "bot",
            Self::Human => "human",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "human" => Self::Human,
            _ => Self::Bot,
        }
    }
}

/// The durable declaration persisted on a queued/scheduled draft.
#[derive(Debug, Clone, PartialEq)]
pub struct PersistedDeclaration {
    pub protocol: String,
    pub catalog: String,
    pub catalog_version: u32,
    pub origin: DeclarationOrigin,
    /// The raw attribute keys the bot declared, verbatim and deduped. Empty for a
    /// human-attested draft that carried no bot declaration.
    pub declared_attrs: Vec<String>,
    /// The draft `revision` this declaration was validated at. When it no longer
    /// matches the row's revision the declaration is stale (invalidated).
    pub revision: i64,
    /// Attribution correction attempts consumed at SMTP time for this revision.
    pub attempts: u32,
    /// Set to [`PARK_REASON_ATTRIBUTION_EXHAUSTED`] only when the draft was parked
    /// for exhausting its attempts; `None` while still retryable.
    pub park_reason: Option<String>,
}

impl PersistedDeclaration {
    /// A fresh bot declaration for a newly validated queue/schedule acceptance.
    /// `revision` is a best-effort stamp; the store re-stamps it from the row's
    /// authoritative `revision` column on write, so callers need not chase the
    /// post-mutation value.
    pub fn new_bot(declared: &[String], revision: i64) -> Self {
        let mut deduped: Vec<String> = Vec::new();
        for k in declared {
            let k = k.trim();
            if !k.is_empty() && !deduped.iter().any(|d| d == k) {
                deduped.push(k.to_string());
            }
        }
        Self {
            protocol: ATTRIBUTION_PROTOCOL.to_string(),
            catalog: CATALOG_NAME.to_string(),
            catalog_version: catalog_version(),
            origin: DeclarationOrigin::Bot,
            declared_attrs: deduped,
            revision,
            attempts: 0,
            park_reason: None,
        }
    }

    /// Serialize to the metadata sub-object stored under
    /// [`ATTRIBUTION_METADATA_KEY`]. Contains no score/weight/threshold.
    pub fn to_value(&self) -> Value {
        json!({
            "protocol": self.protocol,
            "catalog": self.catalog,
            "catalog_version": self.catalog_version,
            "origin": self.origin.as_str(),
            "declared_attrs": self.declared_attrs,
            "revision": self.revision,
            "attempts": self.attempts,
            "park_reason": self.park_reason,
        })
    }

    /// Parse the persisted declaration out of a draft's full metadata blob, or
    /// `None` when absent/malformed.
    pub fn from_metadata(metadata: Option<&Value>) -> Option<Self> {
        let block = metadata?.get(ATTRIBUTION_METADATA_KEY)?;
        let protocol = block.get("protocol").and_then(Value::as_str)?.to_string();
        let catalog = block
            .get("catalog")
            .and_then(Value::as_str)
            .unwrap_or(CATALOG_NAME)
            .to_string();
        let catalog_version = block
            .get("catalog_version")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let origin = block
            .get("origin")
            .and_then(Value::as_str)
            .map(DeclarationOrigin::parse)
            .unwrap_or(DeclarationOrigin::Bot);
        let declared_attrs = block
            .get("declared_attrs")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let revision = block.get("revision").and_then(Value::as_i64).unwrap_or(-1);
        let attempts = block.get("attempts").and_then(Value::as_u64).unwrap_or(0) as u32;
        let park_reason = block
            .get("park_reason")
            .and_then(Value::as_str)
            .map(str::to_string);
        Some(Self {
            protocol,
            catalog,
            catalog_version,
            origin,
            declared_attrs,
            revision,
            attempts,
            park_reason,
        })
    }

    /// Whether this declaration is *current* for a draft at `draft_revision`: the
    /// protocol matches and it was validated at exactly this revision. A stale
    /// declaration (any material edit bumped the revision) is not current and must
    /// be treated as if no declaration were present.
    pub fn is_current(&self, draft_revision: i64) -> bool {
        self.protocol == ATTRIBUTION_PROTOCOL && self.revision == draft_revision
    }
}

/// The durable origin of a queued/scheduled send, decided from provenance facts
/// the sweep can trust — never from a header a bot could set for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduledOrigin {
    /// An AI agent authored this send (an agent/mcp/cli surface, or a persisted
    /// bot declaration). A valid current bot declaration is mandatory even after
    /// a human later approves it; host facts and human approval never substitute.
    Bot,
    /// A human authored this send via an authenticated host surface
    /// (`created_by` = `human:*`). Combined with a current human attestation the
    /// send may proceed on its durable host attestation without a bot declaration.
    Human,
    /// Provenance could not be established (legacy/malformed rows). Treated as
    /// bot-originated so an unattested draft fails closed rather than sending.
    Unknown,
}

impl ScheduledOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bot => "bot",
            Self::Human => "human",
            Self::Unknown => "unknown",
        }
    }

    /// The [`DeclarationOrigin`] to stamp on a persisted declaration for this
    /// scheduled origin. `Human` is preserved; `Bot` and `Unknown` both stamp
    /// `Bot` (an unknown-provenance draft fails closed as bot). This is the one
    /// place the origin mapping lives, so a human origin is never rewritten to bot
    /// by accident.
    pub fn declaration_origin(self) -> DeclarationOrigin {
        match self {
            Self::Human => DeclarationOrigin::Human,
            Self::Bot | Self::Unknown => DeclarationOrigin::Bot,
        }
    }
}

/// Decide the durable origin of a scheduled draft from provenance facts.
///
/// A **current** persisted bot declaration is the strongest signal — a bot
/// already took attribution responsibility for this exact revision, so the send
/// is bot-originated regardless of `created_by`. Otherwise the draft's
/// `created_by` marker decides: `human:*` is a genuinely human-authored send,
/// the agent surfaces (`agent`/`mcp`/`cli`) are bot-originated, and anything else
/// (including `None`) is unknown and fails closed as bot.
pub fn scheduled_origin(
    created_by: Option<&str>,
    persisted: Option<&PersistedDeclaration>,
    draft_revision: i64,
) -> ScheduledOrigin {
    if let Some(decl) = persisted {
        if decl.is_current(draft_revision) && decl.origin == DeclarationOrigin::Bot {
            return ScheduledOrigin::Bot;
        }
    }
    match created_by {
        Some(cb) if cb.starts_with("human:") => ScheduledOrigin::Human,
        Some("agent") | Some("mcp") | Some("cli") => ScheduledOrigin::Bot,
        _ => ScheduledOrigin::Unknown,
    }
}

/// The attribution inputs the scheduled-send sweep resolves with, derived from a
/// draft's durable state.
///
/// Origin is decided from durable provenance ([`scheduled_origin`]), never from
/// the human attestation alone. Only a **genuinely human-originated** draft that
/// also carries a current human attestation (`human_approved`) lifts the
/// bot-declaration requirement — it then proceeds on its durable host attestation
/// (`tyler_approved`, derived by Envelope) without a fabricated bot declaration.
///
/// Every bot-originated draft requires a valid, *current* persisted declaration
/// **even after a human approves it**: approval supplements the declaration
/// (adding `tyler_approved` to the derived set) but never erases the bot's
/// attribution responsibility. Unknown-provenance drafts fail closed as bot.
/// A non-empty derived set never substitutes for a missing/stale declaration.
///
/// Returns `(declared_attrs, require_declaration)`.
pub fn scheduled_attribution_inputs(
    created_by: Option<&str>,
    human_approved: bool,
    persisted: Option<&PersistedDeclaration>,
    draft_revision: i64,
) -> (Vec<String>, bool) {
    let current = persisted.filter(|d| d.is_current(draft_revision));
    let declared = current
        .map(|d| d.declared_attrs.clone())
        .unwrap_or_default();
    let origin = scheduled_origin(created_by, persisted, draft_revision);
    // The ONLY path that lifts the bot-declaration requirement: a genuinely
    // human-originated send that is currently human-attested. Bot and unknown
    // origins always require a declaration, human approval notwithstanding.
    let require_declaration = !(origin == ScheduledOrigin::Human && human_approved);
    (declared, require_declaration)
}

/// The bounded-attempt decision when a bot-originated queued draft fails
/// attribution at SMTP time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// Still retryable: leave the draft due and record `attempts`.
    Retry { attempts: u32 },
    /// Bound reached: park the draft for review and record `attempts`.
    Park { attempts: u32 },
}

impl AttemptOutcome {
    pub fn attempts(&self) -> u32 {
        match self {
            Self::Retry { attempts } | Self::Park { attempts } => *attempts,
        }
    }

    pub fn is_park(&self) -> bool {
        matches!(self, Self::Park { .. })
    }
}

/// Advance the attribution attempt counter for a failed SMTP-time attempt.
///
/// `prior` is the attempt count already recorded for the *current* revision (0
/// when there is no current declaration — a material revision reset it). The
/// returned outcome carries the incremented count and whether the bound
/// ([`MAX_ATTRIBUTION_ATTEMPTS`]) has been reached.
pub fn advance_attempt(prior: u32) -> AttemptOutcome {
    let attempts = prior.saturating_add(1);
    if attempts >= MAX_ATTRIBUTION_ATTEMPTS {
        AttemptOutcome::Park { attempts }
    } else {
        AttemptOutcome::Retry { attempts }
    }
}

/// Build the attribution block to persist after a failed SMTP-time attempt: the
/// **passed-through** origin and declared set, keyed to `revision`, with the
/// advanced attempt count and (on park) the exhaustion `park_reason`.
///
/// The `origin` is preserved verbatim — a genuinely human-originated draft is
/// never rewritten as bot. In practice the sweep only builds a failed-attempt
/// block for Bot/Unknown origins (a human failure parks for re-approval without
/// fabricating any declaration), but preserving the origin here is the
/// defense-in-depth that keeps the invariant true regardless of the call site.
pub fn failed_attempt_value(
    origin: DeclarationOrigin,
    declared: &[String],
    revision: i64,
    outcome: &AttemptOutcome,
) -> Value {
    let park_reason = if outcome.is_park() {
        Some(PARK_REASON_ATTRIBUTION_EXHAUSTED.to_string())
    } else {
        None
    };
    let decl = PersistedDeclaration {
        protocol: ATTRIBUTION_PROTOCOL.to_string(),
        catalog: CATALOG_NAME.to_string(),
        catalog_version: catalog_version(),
        origin,
        declared_attrs: declared.to_vec(),
        revision,
        attempts: outcome.attempts(),
        park_reason,
    };
    decl.to_value()
}

/// What the scheduled-send sweep should do when a queued draft fails the
/// attribution precondition at SMTP time, decided from the draft's durable
/// [`ScheduledOrigin`].
#[derive(Debug, Clone, PartialEq)]
pub enum AttributionFailureAction {
    /// Bot/Unknown origin, still under the bound: record the advanced attempt and
    /// leave the draft due for a later sweep to retry.
    Retry { value: Value },
    /// Bot/Unknown origin, bound reached: park `pending_review` with the exhausted
    /// attempt state (`park_reason = attribution_exhausted`).
    Park { value: Value },
    /// Human origin: park `pending_review` for honest human re-approval. NO bot
    /// declaration or attempt state is fabricated — the human origin/attestation
    /// is preserved so a re-approval can recover the send.
    HumanReview,
}

/// Decide the [`AttributionFailureAction`] for a sweep-time attribution failure.
///
/// The bounded correction loop applies ONLY to Bot/Unknown origins. A genuine
/// human-originated draft (whose approval went stale) is routed to human review
/// without any fabricated bot provenance, so it never becomes bot-originated and
/// can still recover through a fresh human attestation.
pub fn attribution_failure_action(
    origin: ScheduledOrigin,
    declared: &[String],
    revision: i64,
    prior_attempts: u32,
) -> AttributionFailureAction {
    if origin == ScheduledOrigin::Human {
        return AttributionFailureAction::HumanReview;
    }
    let outcome = advance_attempt(prior_attempts);
    let value = failed_attempt_value(origin.declaration_origin(), declared, revision, &outcome);
    if outcome.is_park() {
        AttributionFailureAction::Park { value }
    } else {
        AttributionFailureAction::Retry { value }
    }
}

/// The additive `attribution` block for a **successful** outbound result.
///
/// Carries the three attribute sets + rejections + state (from the resolution)
/// and, when a Governor verdict actually ran (immediate sends), its
/// decision/route. Queue/scheduled acceptances have no verdict yet, so
/// `governor` is `null` and a `pending` marker records that the real decision
/// happens at the scheduled-send sweep. Never a score/weight/threshold, body,
/// raw recipient, secret, or attachment byte.
pub fn success_attribution_block(
    resolution: &AttributionResolution,
    governor_decision: Option<&str>,
    governor_route: Option<&str>,
    deferred_to_sweep: bool,
) -> Value {
    let mut block = resolution.to_json();
    if let Value::Object(map) = &mut block {
        let governor = match governor_decision {
            Some(decision) => json!({
                "decision": decision,
                "route": governor_route,
            }),
            None => Value::Null,
        };
        map.insert("governor".into(), governor);
        if deferred_to_sweep {
            map.insert(
                "governor_decision_pending".into(),
                json!("the Governor decision runs at the scheduled-send sweep, just before SMTP"),
            );
        }
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribution::{AttributedSendContext, resolve};

    fn sample_ctx() -> AttributedSendContext {
        AttributedSendContext {
            account_domain: Some("martin.fm".into()),
            recipient_domains: vec!["acme.example".into()],
            recipient_count: 1,
            attachment_count: 1,
            ..Default::default()
        }
    }

    #[test]
    fn new_bot_dedupes_and_stamps_protocol_catalog_version() {
        let decl = PersistedDeclaration::new_bot(
            &[
                "financial_content".into(),
                " financial_content ".into(),
                "".into(),
            ],
            7,
        );
        assert_eq!(decl.declared_attrs, vec!["financial_content".to_string()]);
        assert_eq!(decl.protocol, ATTRIBUTION_PROTOCOL);
        assert_eq!(decl.catalog, CATALOG_NAME);
        assert_eq!(decl.catalog_version, catalog_version());
        assert_eq!(decl.origin, DeclarationOrigin::Bot);
        assert_eq!(decl.revision, 7);
        assert_eq!(decl.attempts, 0);
        assert_eq!(decl.park_reason, None);
    }

    #[test]
    fn round_trips_through_metadata() {
        let decl = PersistedDeclaration::new_bot(&["informational".into()], 3);
        let meta = json!({ ATTRIBUTION_METADATA_KEY: decl.to_value() });
        let parsed = PersistedDeclaration::from_metadata(Some(&meta)).expect("parse");
        assert_eq!(parsed, decl);
    }

    #[test]
    fn from_metadata_none_when_absent() {
        assert!(PersistedDeclaration::from_metadata(None).is_none());
        assert!(PersistedDeclaration::from_metadata(Some(&json!({}))).is_none());
        assert!(
            PersistedDeclaration::from_metadata(Some(&json!({ "draft_kind": "reply" }))).is_none()
        );
    }

    #[test]
    fn is_current_only_at_the_validated_revision() {
        let decl = PersistedDeclaration::new_bot(&["financial_content".into()], 5);
        assert!(decl.is_current(5));
        assert!(!decl.is_current(6), "a material revision invalidates it");
        assert!(!decl.is_current(4));
    }

    #[test]
    fn is_current_false_on_protocol_mismatch() {
        let mut decl = PersistedDeclaration::new_bot(&["financial_content".into()], 5);
        decl.protocol = "envelope.attribution.v0".into();
        assert!(!decl.is_current(5));
    }

    #[test]
    fn scheduled_origin_from_durable_provenance() {
        // A current persisted bot declaration is authoritative: bot even with a
        // human `created_by`.
        let decl = PersistedDeclaration::new_bot(&["informational".into()], 4);
        assert_eq!(
            scheduled_origin(Some("human:dashboard"), Some(&decl), 4),
            ScheduledOrigin::Bot
        );
        // A stale declaration is ignored; `created_by` decides.
        assert_eq!(
            scheduled_origin(Some("agent"), Some(&decl), 5),
            ScheduledOrigin::Bot
        );
        // Agent surfaces are bot; human surfaces are human; everything else is
        // unknown and treated as bot (fail closed).
        assert_eq!(
            scheduled_origin(Some("agent"), None, 1),
            ScheduledOrigin::Bot
        );
        assert_eq!(scheduled_origin(Some("mcp"), None, 1), ScheduledOrigin::Bot);
        assert_eq!(scheduled_origin(Some("cli"), None, 1), ScheduledOrigin::Bot);
        assert_eq!(
            scheduled_origin(Some("human:dashboard"), None, 1),
            ScheduledOrigin::Human
        );
        assert_eq!(scheduled_origin(None, None, 1), ScheduledOrigin::Unknown);
        assert_eq!(
            scheduled_origin(Some("mystery"), None, 1),
            ScheduledOrigin::Unknown
        );
    }

    #[test]
    fn scheduled_inputs_bot_requires_declaration_and_carries_current_declared() {
        let decl = PersistedDeclaration::new_bot(&["financial_content".into()], 2);
        let (declared, require) =
            scheduled_attribution_inputs(Some("agent"), false, Some(&decl), 2);
        assert!(require, "bot-originated drafts require a declaration");
        assert_eq!(declared, vec!["financial_content".to_string()]);
    }

    #[test]
    fn scheduled_inputs_bot_human_approved_still_requires_declaration() {
        // Human approval SUPPLEMENTS a bot send; it never erases the bot's
        // attribution responsibility. A bot draft with a human attestation but no
        // declaration still requires one and fails closed.
        let (declared, require) = scheduled_attribution_inputs(Some("agent"), true, None, 1);
        assert!(require, "human approval does not waive the bot declaration");
        assert!(declared.is_empty());
        let mut ctx = sample_ctx();
        ctx.human_approved = true;
        let res = resolve(&declared, &ctx, require);
        assert_eq!(
            res.state,
            crate::attribution::AttributionState::Unattributed
        );
        assert!(res.governor_attrs.is_empty(), "nothing reaches Governor");
    }

    #[test]
    fn scheduled_inputs_unknown_origin_fails_closed_even_when_approved() {
        // An unknown-provenance draft is never silently treated as human, even
        // with a (mismatched) human attestation present.
        let (_declared, require) = scheduled_attribution_inputs(None, true, None, 1);
        assert!(require, "unknown provenance fails closed as bot");
    }

    #[test]
    fn scheduled_inputs_stale_declaration_is_dropped_and_still_requires_declaration() {
        // Declaration validated at revision 2, but the draft is now revision 3
        // (a material edit): the declaration is stale and must be dropped, yet a
        // bot-originated draft still requires one — it will fail closed.
        let decl = PersistedDeclaration::new_bot(&["financial_content".into()], 2);
        let (declared, require) =
            scheduled_attribution_inputs(Some("agent"), false, Some(&decl), 3);
        assert!(require);
        assert!(
            declared.is_empty(),
            "stale declared attrs never cross a material revision"
        );
    }

    #[test]
    fn scheduled_inputs_bot_with_no_declaration_fails_closed_via_resolve() {
        // The core non-negotiable: a bot draft with no valid declaration but a
        // rich derived context resolves to Unattributed — host facts never
        // substitute for the missing declaration.
        let (declared, require) = scheduled_attribution_inputs(Some("agent"), false, None, 1);
        assert!(require);
        assert!(declared.is_empty());
        let res = resolve(&declared, &sample_ctx(), require);
        assert_eq!(
            res.state,
            crate::attribution::AttributionState::Unattributed
        );
        assert!(!res.derived_attrs.is_empty(), "context derives facts");
        assert!(res.governor_attrs.is_empty(), "nothing reaches Governor");
    }

    #[test]
    fn scheduled_inputs_human_does_not_require_declaration() {
        let (declared, require) =
            scheduled_attribution_inputs(Some("human:dashboard"), true, None, 1);
        assert!(!require, "human attestation lifts the bot-declaration rule");
        assert!(declared.is_empty());
        // With human approval, tyler_approved is derived and resolve is attributed
        // without any bot declaration.
        let mut ctx = sample_ctx();
        ctx.human_approved = true;
        let res = resolve(&declared, &ctx, require);
        assert_eq!(res.state, crate::attribution::AttributionState::Attributed);
        assert!(res.governor_attrs.iter().any(|a| a == "tyler_approved"));
    }

    #[test]
    fn scheduled_inputs_human_origin_without_attestation_fails_closed() {
        // A human-authored draft whose approval went stale (no current
        // attestation) still requires a declaration — it cannot auto-send on
        // provenance alone.
        let (_declared, require) =
            scheduled_attribution_inputs(Some("human:dashboard"), false, None, 1);
        assert!(
            require,
            "human origin without a current attestation fails closed"
        );
    }

    #[test]
    fn advance_attempt_retries_then_parks_at_the_bound() {
        // Attempts 1 and 2 retry; attempt 3 parks. (MAX_ATTRIBUTION_ATTEMPTS == 3)
        assert_eq!(advance_attempt(0), AttemptOutcome::Retry { attempts: 1 });
        assert_eq!(advance_attempt(1), AttemptOutcome::Retry { attempts: 2 });
        assert_eq!(advance_attempt(2), AttemptOutcome::Park { attempts: 3 });
        // Past the bound stays parked.
        assert_eq!(advance_attempt(3), AttemptOutcome::Park { attempts: 4 });
    }

    #[test]
    fn failed_attempt_value_sets_park_reason_only_on_park() {
        let retry = failed_attempt_value(
            DeclarationOrigin::Bot,
            &[],
            2,
            &AttemptOutcome::Retry { attempts: 2 },
        );
        assert_eq!(retry["attempts"], 2);
        assert_eq!(retry["park_reason"], Value::Null);
        assert_eq!(retry["revision"], 2);

        let park = failed_attempt_value(
            DeclarationOrigin::Bot,
            &[],
            2,
            &AttemptOutcome::Park { attempts: 3 },
        );
        assert_eq!(park["attempts"], 3);
        assert_eq!(park["park_reason"], PARK_REASON_ATTRIBUTION_EXHAUSTED);
    }

    // ── Block 3: preserve human origin across failure/sweep ─────────────────

    #[test]
    fn failed_attempt_value_preserves_the_passed_origin() {
        // Never rewrite Human as Bot: the persisted origin is exactly what was
        // passed, so a human draft's failed attempt stays human-originated.
        let human = failed_attempt_value(
            DeclarationOrigin::Human,
            &[],
            1,
            &AttemptOutcome::Retry { attempts: 1 },
        );
        assert_eq!(human["origin"], "human");
        let bot = failed_attempt_value(
            DeclarationOrigin::Bot,
            &[],
            1,
            &AttemptOutcome::Retry { attempts: 1 },
        );
        assert_eq!(bot["origin"], "bot");
    }

    #[test]
    fn attribution_failure_action_human_origin_parks_without_fabricating_bot() {
        // A human-originated draft that fails attribution (stale approval) is
        // routed to human review — no bot declaration or attempt state is written.
        let action = attribution_failure_action(ScheduledOrigin::Human, &[], 4, 0);
        assert_eq!(action, AttributionFailureAction::HumanReview);
        // Even at/over the bound, human origin never enters the bot park loop.
        let action = attribution_failure_action(ScheduledOrigin::Human, &[], 4, 5);
        assert_eq!(action, AttributionFailureAction::HumanReview);
    }

    #[test]
    fn attribution_failure_action_bot_origin_runs_bounded_loop() {
        // Attempts 1,2 retry; attempt 3 parks. Origin stays bot throughout.
        match attribution_failure_action(ScheduledOrigin::Bot, &["financial_content".into()], 2, 0)
        {
            AttributionFailureAction::Retry { value } => {
                assert_eq!(value["attempts"], 1);
                assert_eq!(value["origin"], "bot");
                assert_eq!(value["declared_attrs"][0], "financial_content");
            }
            other => panic!("expected retry, got {other:?}"),
        }
        match attribution_failure_action(ScheduledOrigin::Bot, &[], 2, 2) {
            AttributionFailureAction::Park { value } => {
                assert_eq!(value["attempts"], 3);
                assert_eq!(value["park_reason"], PARK_REASON_ATTRIBUTION_EXHAUSTED);
                assert_eq!(value["origin"], "bot");
            }
            other => panic!("expected park, got {other:?}"),
        }
    }

    #[test]
    fn attribution_failure_action_unknown_origin_fails_closed_as_bot_loop() {
        // Unknown provenance is never treated as human: it enters the bot loop and
        // its persisted origin is bot (fail closed), never human.
        match attribution_failure_action(ScheduledOrigin::Unknown, &[], 1, 0) {
            AttributionFailureAction::Retry { value } => assert_eq!(value["origin"], "bot"),
            other => panic!("expected retry, got {other:?}"),
        }
    }

    #[test]
    fn success_block_carries_sets_and_governor_verdict() {
        let res = resolve(&["financial_content".into()], &sample_ctx(), true);
        let block = success_attribution_block(&res, Some("allow"), None, false);
        assert_eq!(block["attribution_state"], "attributed");
        assert!(block["declared_attrs"].as_array().unwrap().len() == 1);
        assert_eq!(block["governor"]["decision"], "allow");
        assert!(block.get("governor_decision_pending").is_none());
        // No score/weight/threshold ever.
        let text = block.to_string();
        for banned in ["\"score\"", "weight", "threshold"] {
            assert!(!text.contains(banned), "leaked {banned}");
        }
    }

    #[test]
    fn success_block_marks_pending_for_deferred_queue_acceptance() {
        let res = resolve(&["financial_content".into()], &sample_ctx(), true);
        let block = success_attribution_block(&res, None, None, true);
        assert_eq!(block["governor"], Value::Null);
        assert!(block.get("governor_decision_pending").is_some());
    }
}
