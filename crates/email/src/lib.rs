// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

pub mod agent_policy;
pub mod attribution;
pub mod backup;
pub mod bulk;
pub mod code_extractor;
pub mod compose;
pub mod discovery;
pub mod errors;
pub mod event_delivery;
pub mod event_pipeline;
pub mod event_types;
pub mod evidence;
pub mod folders;
pub mod idle;
pub mod imap;
pub mod managesieve;
pub mod migrate;
pub mod outbound;
pub mod provider;
pub mod reply;
pub mod rules;
pub mod send_policy;
pub mod sieve;
pub mod smtp;
pub mod threading;
pub mod unsubscribe;

pub use agent_policy::{AgentPolicy, PolicyDenial};
pub use attribution::{
    AttributedSendContext, RecipientSummary, classify_sensitive_attachment,
    collect_recipient_domains, is_disposable_domain, is_freemail_domain, is_gov_domain,
};
pub use bulk::{
    BULK_UID_LIMIT, BulkError, BulkFailure, BulkOp, BulkRequest, BulkResult, BulkTarget,
    CHUNK_SIZE, coalesce_uids, execute as execute_bulk, resolve_target,
};
pub use compose::{
    AssembledBody, ContextBlock, DraftKind, abridge_words, assemble_body, build_forward_context,
    build_reply_context, message_preview_source, prefix_forward_subject,
};
pub use discovery::{DiscoveryCandidate, discover};
pub use errors::{DiscoveryError, ImapError, SmtpError};
pub use event_delivery::{
    BACKOFF_SCHEDULE, DeliveryLimits, DeliveryReport, MAX_ATTEMPTS, deliver_due_events,
    hmac_sha256_hex,
};
pub use folders::{detect_drafts_folder, detect_sent_folder};
pub use imap::ImapClient;
pub use outbound::{
    GovernorConfig, GovernorMode, GovernorOutcome, GovernorRequest, SendDisposition, SendSurface,
    gate as governor_gate, resolve_cooldown_seconds, resolve_disposition,
};
pub use provider::{ProviderType, detect_provider, resolve_folder};
pub use reply::{ReplyHeaders, build_reply_all_headers, build_reply_headers};
pub use send_policy::{
    RecipientAllowlist, SendMode, SendPolicyAuditEvent, SendPolicyDecision, SendPolicyDenial,
    SendPolicyInput, SendRuntime, audit_event_for, default_mode_for_runtime, evaluate,
    parse_recipient_email_domain,
};
pub use smtp::SmtpSender;
pub use threading::{ThreadBuildResult, build_threads, normalize_subject};
