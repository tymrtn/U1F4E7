// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

pub mod code_extractor;
pub mod discovery;
pub mod errors;
pub mod event_pipeline;
pub mod event_types;
pub mod folders;
pub mod idle;
pub mod imap;
pub mod provider;
pub mod reply;
pub mod rules;
pub mod sieve;
pub mod smtp;
pub mod threading;
pub mod unsubscribe;

pub use discovery::{DiscoveryCandidate, discover};
pub use errors::{DiscoveryError, ImapError, SmtpError};
pub use folders::{detect_drafts_folder, detect_sent_folder};
pub use imap::ImapClient;
pub use provider::{ProviderType, detect_provider, resolve_folder};
pub use reply::{ReplyHeaders, build_reply_all_headers, build_reply_headers};
pub use smtp::SmtpSender;
pub use threading::{ThreadBuildResult, build_threads, normalize_subject};
