// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

pub mod accounts;
pub mod action_log;
pub mod address_book;
pub mod agent_cockpit;
pub mod agent_identity;
pub mod build_info;
pub mod contacts;
pub mod credential_store;
pub mod crypto;
pub mod db;
pub mod drafts;
pub mod errors;
pub mod event_catalog;
pub mod event_deliveries;
pub mod event_routes;
pub mod events;
pub mod license_store;
pub mod message_index;
pub mod migration;
pub mod migrations;
pub mod models;
pub mod ops_primitives;
pub mod paths;
pub mod relationship_facts;
pub mod rule_store;
pub mod sent_relationships;
pub mod snoozed;
pub mod tag_store;
pub mod threads;

pub use action_log::{ACTION_FAILURE_STATUSES, is_action_failure_status};
pub use address_book::{ADDRESS_HISTORY_CHUNK_ROWS, AddressHistoryReconcile, AddressSuggestion};
pub use agent_cockpit::{AgentActivityCounts, GovernorVerdict};
pub use agent_identity::{
    AgentIdentity, AgentPolicy, DEFAULT_SEND_MODE_CEILING, NewAgentToken, SendModeCeiling,
};
pub use build_info::{BuildInfo, VERSION};
pub use credential_store::CredentialBackend;
pub use db::Database;
pub use drafts::{
    CONTEXT_CORRECTION_CONTRACT, CONTEXT_REFINED_EVENT, CONTEXT_REFINEMENT_INVALIDATED_EVENT,
    ContextCorrection, SyncClaim,
};
pub use errors::StoreError;
pub use event_deliveries::{DeliveryStatusFilter, RESPONSE_SNIPPET_CAP_BYTES};
pub use models::*;
pub use ops_primitives::{RuleRunAuditInput, WatchUpsert};
pub use paths::{app_data_dir, config_root_dir, credential_file_path, database_path};
pub use relationship_facts::{RELATIONSHIP_FACT_RECIPIENT_LIMIT, RelationshipFacts};
pub use sent_relationships::{SentRelationship, SentRelationshipPage, SentRelationshipSignal};
pub use threads::ThreadContext;
