// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! HTTP handlers for the dashboard REST API, organized by feature.
//!
//! Each handler module owns a small set of related endpoints and is wired
//! into the router in [`crate::serve`]. All handlers share [`crate::state::AppState`]
//! which provides access to the SQLite database and a per-account IMAP
//! connection pool.

pub mod accounts;
pub mod attachments;
pub mod compose;
pub mod drafts;
pub mod folders;
pub mod messages;
pub mod snoozed;
pub mod stats;
pub mod threads;
