// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Sent relationship history: a read-only aggregate of the deep thread cache
//! (`threads` + `thread_messages`) grouped by outbound counterparty.
//!
//! This is context for the operator's review page, not a task queue. A row
//! says "this mailbox has written to plans@tripit.com 382 times across 332
//! threads, last seen November 2025" — it never says "awaiting reply", because
//! nothing here reads intent. The signal is derived only from aggregate
//! topology (outbound/inbound balance) and recency of the last observed
//! message.
//!
//! Coverage is what the thread scan has cached, no more: folders the scan has
//! not visited contribute nothing, so callers must present this as observed
//! thread history rather than a complete mailbox census.
//!
//! The aggregate never opens a socket, never reconciles the address history,
//! and never writes: one account-scoped walk of the thread cache per call.
//! Recipient headers are split with the same parser compose autocomplete
//! uses ([`crate::address_book::parse_address_list`]), so a multi-recipient
//! To line attributes to each individual address and a malformed entry is
//! dropped rather than surfaced as a garbage counterparty. Addresses
//! configured as any Envelope account's username are the operator's own and
//! are never counterparties, and neither is a `+tag` variant of one at the
//! same domain; everyone else's plus-addresses are kept verbatim as distinct
//! counterparties.

use std::collections::{HashMap, HashSet};

use rusqlite::params;

use crate::address_book::{normalize_email, normalize_timestamp, parse_address_list};
use crate::db::Database;
use crate::errors::Result;

/// Outbound-to-inbound ratio at or above which a correspondence reads as
/// one-way. TripIt-style relationships (hundreds outbound, a stray receipt or
/// two inbound) land far above it; anything resembling a conversation lands
/// below and reads as bilateral.
pub const SENT_RELATIONSHIP_ONE_WAY_RATIO: i64 = 10;

/// Days since the last observed message within which an outbound-dominant
/// relationship reads as recent rather than historical.
pub const SENT_RELATIONSHIP_RECENT_DAYS: i64 = 90;

/// Fixed, truthful relationship signal derived only from aggregate topology
/// and recency. Serialized names are a public contract; keep them stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentRelationshipSignal {
    /// Predominantly outbound and quiet for longer than the recency window.
    HistoricalOneWay,
    /// Predominantly outbound with activity inside the recency window.
    RecentOutboundHistory,
    /// Enough inbound relative to outbound to read as a two-way
    /// correspondence, whatever its age.
    BilateralHistory,
}

impl SentRelationshipSignal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HistoricalOneWay => "historical_one_way",
            Self::RecentOutboundHistory => "recent_outbound_history",
            Self::BilateralHistory => "bilateral_history",
        }
    }
}

/// One observed outbound counterparty for one account. The email address is
/// the relationship identity and is carried deliberately; no subject, snippet,
/// or body material rides along.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentRelationship {
    /// Normalized (lowercased, bracket-stripped) counterparty address.
    pub counterparty_email: String,
    pub account_id: String,
    /// Observed messages involving the counterparty, either direction.
    pub message_count: i64,
    pub outbound_count: i64,
    pub inbound_count: i64,
    /// Distinct threads the counterparty was observed in.
    pub thread_count: i64,
    pub first_observed: Option<String>,
    pub last_observed: Option<String>,
    pub signal: SentRelationshipSignal,
}

/// A capped relationship list with its true total, so a capped page can say
/// "showing N of M" honestly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentRelationshipPage {
    /// Qualifying relationships for the account, before the cap.
    pub total: i64,
    pub items: Vec<SentRelationship>,
}

/// The operator's own identity, used only to exclude counterparties. Exact
/// configured usernames are self, and so is a `+tag` variant of one at the
/// exact same domain (`me+receipts@example.test` when `me@example.test` is
/// configured). This never rewrites a counterparty's identity — a non-self
/// plus-address stays its own distinct row — and there is deliberately no
/// Gmail-dot guessing or any other provider aliasing heuristic.
struct OwnAddresses {
    exact: HashSet<String>,
    /// `base@domain` per configured username, base = local part before `+`.
    plus_bases: HashSet<String>,
}

impl OwnAddresses {
    fn insert(&mut self, email: String) {
        if let Some((local, domain)) = email.split_once('@') {
            let base = local.split_once('+').map_or(local, |(base, _)| base);
            self.plus_bases.insert(format!("{base}@{domain}"));
        }
        self.exact.insert(email);
    }

    fn is_self(&self, email: &str) -> bool {
        if self.exact.contains(email) {
            return true;
        }
        let Some((local, domain)) = email.split_once('@') else {
            return false;
        };
        // Only an explicit `+tag` form maps back to a configured base; a
        // bare address never matches through this branch.
        match local.split_once('+') {
            Some((base, _tag)) => self.plus_bases.contains(&format!("{base}@{domain}")),
            None => false,
        }
    }
}

/// Per-counterparty accumulator while walking the thread cache.
#[derive(Default)]
struct Tally {
    outbound: i64,
    inbound: i64,
    threads: HashSet<String>,
    first: Option<String>,
    last: Option<String>,
}

impl Tally {
    fn observe(&mut self, thread_id: &str, at: &Option<String>) {
        self.threads.insert(thread_id.to_string());
        if let Some(at) = at {
            if self.first.as_ref().is_none_or(|first| at < first) {
                self.first = Some(at.clone());
            }
            if self.last.as_ref().is_none_or(|last| at > last) {
                self.last = Some(at.clone());
            }
        }
    }
}

impl Database {
    /// Aggregate one account's observed thread history by outbound
    /// counterparty. Read-only: two SELECTs over local SQLite, no IMAP, no
    /// reconcile, no writes of any kind.
    ///
    /// Only addresses this account has written to qualify — an inbound-only
    /// sender is not a sent relationship. Rows are ordered by outbound volume
    /// then recency then address, and capped at `limit` with the true total
    /// reported alongside.
    ///
    /// `now` is a `%Y-%m-%dT%H:%M:%S` UTC timestamp used solely to derive the
    /// recency half of the signal.
    pub fn aggregate_sent_relationships(
        &self,
        account_id: &str,
        now: &str,
        limit: usize,
    ) -> Result<SentRelationshipPage> {
        // Every configured account's address is the operator's own identity;
        // none of them — nor a same-domain `+tag` variant of one — is ever a
        // counterparty, whichever account is aggregated.
        let own_addresses = {
            let mut stmt = self.conn().prepare("SELECT username FROM accounts")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut own = OwnAddresses {
                exact: HashSet::new(),
                plus_bases: HashSet::new(),
            };
            for email in rows
                .filter_map(|row| row.ok())
                .filter_map(|username| normalize_email(&username))
            {
                own.insert(email);
            }
            own
        };

        let mut tallies: HashMap<String, Tally> = HashMap::new();

        // Pass 1 — outbound recipients. Only addresses observed here qualify:
        // this is sent relationship history, so the outbound leg defines
        // membership. To, Cc, and Bcc are all recipients; an address on more
        // than one of them counts once per message.
        {
            let mut stmt = self.conn().prepare(
                "SELECT tm.thread_id, tm.to_addresses, tm.cc_addresses, tm.bcc_addresses, tm.date
                 FROM thread_messages tm
                 JOIN threads t ON t.thread_id = tm.thread_id
                 WHERE t.account_id = ?1 AND tm.is_outbound = 1",
            )?;
            let rows = stmt.query_map(params![account_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })?;
            for row in rows {
                let (thread_id, to, cc, bcc, date) = row?;
                let at = date.as_deref().and_then(normalize_timestamp);
                let mut recipients: HashSet<String> = HashSet::new();
                for raw in [to.as_deref(), cc.as_deref(), bcc.as_deref()]
                    .into_iter()
                    .flatten()
                {
                    for parsed in parse_address_list(raw) {
                        if !own_addresses.is_self(&parsed.email) {
                            recipients.insert(parsed.email);
                        }
                    }
                }
                for email in recipients {
                    let tally = tallies.entry(email).or_default();
                    tally.outbound += 1;
                    tally.observe(&thread_id, &at);
                }
            }
        }

        // Pass 2 — inbound messages from addresses already written to. An
        // inbound-only sender never qualifies, so unknown senders drop here.
        {
            let mut stmt = self.conn().prepare(
                "SELECT tm.thread_id, tm.from_address, tm.date
                 FROM thread_messages tm
                 JOIN threads t ON t.thread_id = tm.thread_id
                 WHERE t.account_id = ?1 AND tm.is_outbound = 0
                   AND tm.from_address IS NOT NULL",
            )?;
            let rows = stmt.query_map(params![account_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?;
            for row in rows {
                let (thread_id, from, date) = row?;
                let Some(sender) = parse_address_list(&from).into_iter().next() else {
                    continue;
                };
                let Some(tally) = tallies.get_mut(&sender.email) else {
                    continue;
                };
                let at = date.as_deref().and_then(normalize_timestamp);
                tally.inbound += 1;
                tally.observe(&thread_id, &at);
            }
        }

        let recent_floor = recent_floor(now);
        let mut items: Vec<SentRelationship> = tallies
            .into_iter()
            .map(|(email, tally)| {
                let signal = derive_signal(&tally, recent_floor.as_deref());
                SentRelationship {
                    counterparty_email: email,
                    account_id: account_id.to_string(),
                    message_count: tally.outbound + tally.inbound,
                    outbound_count: tally.outbound,
                    inbound_count: tally.inbound,
                    thread_count: tally.threads.len() as i64,
                    first_observed: tally.first,
                    last_observed: tally.last,
                    signal,
                }
            })
            .collect();

        // Highest signal first: outbound volume, then recency, then address
        // for a stable order.
        items.sort_by(|a, b| {
            b.outbound_count
                .cmp(&a.outbound_count)
                .then_with(|| b.last_observed.cmp(&a.last_observed))
                .then_with(|| a.counterparty_email.cmp(&b.counterparty_email))
        });

        let total = items.len() as i64;
        items.truncate(limit);
        Ok(SentRelationshipPage { total, items })
    }
}

/// The oldest `%Y-%m-%dT%H:%M:%S` timestamp that still counts as recent, or
/// `None` when `now` does not parse — in which case nothing reads as recent,
/// which errs toward the quieter "historical" label rather than inventing
/// freshness.
fn recent_floor(now: &str) -> Option<String> {
    chrono::NaiveDateTime::parse_from_str(now, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|parsed| {
            (parsed - chrono::Duration::days(SENT_RELATIONSHIP_RECENT_DAYS))
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string()
        })
}

/// Fixed signal from aggregate topology and recency only. No per-message
/// content is read and nothing here may ever imply "awaiting reply" — that
/// label is reserved for explicit durable snooze state elsewhere.
fn derive_signal(tally: &Tally, recent_floor: Option<&str>) -> SentRelationshipSignal {
    let one_way = tally.inbound == 0
        || tally.outbound
            >= tally
                .inbound
                .saturating_mul(SENT_RELATIONSHIP_ONE_WAY_RATIO);
    if !one_way {
        return SentRelationshipSignal::BilateralHistory;
    }
    let recent = match (recent_floor, tally.last.as_deref()) {
        (Some(floor), Some(last)) => last >= floor,
        _ => false,
    };
    if recent {
        SentRelationshipSignal::RecentOutboundHistory
    } else {
        SentRelationshipSignal::HistoricalOneWay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-08-30T12:00:00";

    fn db_with_accounts() -> Database {
        let db = Database::open_memory().unwrap();
        for (id, username) in [
            ("acct-a", "me@example.test"),
            ("acct-b", "personal@example.org"),
        ] {
            db.conn()
                .execute(
                    "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                     imap_host, imap_port, encrypted_password)
                     VALUES (?1, ?1, ?2, 'example.test', 'smtp.example.test', 587,
                             'imap.example.test', 993, 'x')",
                    [id, username],
                )
                .unwrap();
        }
        db
    }

    fn new_thread(db: &Database, account: &str, label: &str) -> String {
        db.create_thread(label, "2020-01-01T00:00:00", "2020-01-01T00:00:00", account)
            .unwrap()
            .thread_id
    }

    #[allow(clippy::too_many_arguments)]
    fn add_message(
        db: &Database,
        thread_id: &str,
        uid: u32,
        folder: &str,
        from: &str,
        to: &str,
        cc: Option<&str>,
        bcc: Option<&str>,
        date: &str,
        outbound: bool,
    ) {
        db.upsert_thread_message(
            thread_id,
            uid,
            Some(&format!("<{folder}-{uid}@fixture.test>")),
            None,
            None,
            folder,
            from,
            to,
            cc,
            bcc,
            date,
            "subject-sentinel-private",
            outbound,
            Some("snippet-sentinel-private"),
        )
        .unwrap();
    }

    fn outbound(db: &Database, thread_id: &str, uid: u32, to: &str, date: &str) {
        add_message(
            db,
            thread_id,
            uid,
            "Sent",
            "me@example.test",
            to,
            None,
            None,
            date,
            true,
        );
    }

    fn inbound(db: &Database, thread_id: &str, uid: u32, from: &str, date: &str) {
        add_message(
            db,
            thread_id,
            uid,
            "INBOX",
            from,
            "me@example.test",
            None,
            None,
            date,
            false,
        );
    }

    /// The product-correcting fixture: 382 outbound / 2 inbound across 332
    /// threads, last active November 2025. It must surface as historical
    /// one-way relationship context with exact observed counts — never as any
    /// kind of awaiting-reply obligation.
    #[test]
    fn tripit_scale_history_reads_as_historical_one_way_context() {
        let db = db_with_accounts();

        // Threads 0..331 carry one outbound each; the last thread carries the
        // remaining 51 outbound plus the 2 stray inbound receipts.
        let mut uid = 0u32;
        for t in 0..332 {
            let thread = new_thread(&db, "acct-a", &format!("trip {t}"));
            let per_thread = if t == 331 { 51 } else { 1 };
            for i in 0..per_thread {
                uid += 1;
                let date = if uid == 1 {
                    "2024-05-01T00:00:00"
                } else if t == 331 && i == per_thread - 1 {
                    "2025-11-15T09:30:00"
                } else {
                    "2025-06-01T08:00:00"
                };
                outbound(&db, &thread, uid, "plans@tripit.com", date);
            }
            if t == 331 {
                inbound(
                    &db,
                    &thread,
                    9001,
                    "plans@tripit.com",
                    "2025-07-01T10:00:00",
                );
                inbound(
                    &db,
                    &thread,
                    9002,
                    "plans@tripit.com",
                    "2025-08-01T10:00:00",
                );
            }
        }

        let page = db.aggregate_sent_relationships("acct-a", NOW, 25).unwrap();

        assert_eq!(page.total, 1);
        assert_eq!(page.items.len(), 1);
        let rel = &page.items[0];
        assert_eq!(rel.counterparty_email, "plans@tripit.com");
        assert_eq!(rel.account_id, "acct-a");
        assert_eq!(rel.outbound_count, 382);
        assert_eq!(rel.inbound_count, 2);
        assert_eq!(rel.message_count, 384);
        assert_eq!(rel.thread_count, 332);
        assert_eq!(rel.first_observed.as_deref(), Some("2024-05-01T00:00:00"));
        assert_eq!(rel.last_observed.as_deref(), Some("2025-11-15T09:30:00"));
        // Nov 2025 against an Aug 2026 "now" is far outside the recency
        // window, and 382:2 is far above the one-way ratio.
        assert_eq!(rel.signal, SentRelationshipSignal::HistoricalOneWay);
        assert_eq!(rel.signal.as_str(), "historical_one_way");
    }

    /// A multi-recipient outbound message attributes to each individual
    /// parsed address — never to the raw comma-separated header string — and
    /// an address on both To and Cc counts once for that message.
    #[test]
    fn multi_recipient_outbound_splits_into_individual_counterparties() {
        let db = db_with_accounts();
        let thread = new_thread(&db, "acct-a", "team");
        add_message(
            &db,
            &thread,
            1,
            "Sent",
            "me@example.test",
            "\"Doe, Jane\" <Jane@x.test>, bob@x.test",
            Some("carol@x.test, bob@x.test"),
            None,
            "2026-08-01T00:00:00",
            true,
        );

        let page = db.aggregate_sent_relationships("acct-a", NOW, 25).unwrap();

        assert_eq!(page.total, 3);
        let mut emails: Vec<&str> = page
            .items
            .iter()
            .map(|rel| rel.counterparty_email.as_str())
            .collect();
        emails.sort_unstable();
        assert_eq!(emails, vec!["bob@x.test", "carol@x.test", "jane@x.test"]);
        for rel in &page.items {
            assert_eq!(rel.outbound_count, 1, "{}", rel.counterparty_email);
            assert_eq!(rel.message_count, 1, "{}", rel.counterparty_email);
            assert_eq!(rel.thread_count, 1);
        }
    }

    /// The operator's own configured addresses (any account's username),
    /// empty entries, and malformed addresses never become counterparties.
    #[test]
    fn self_addresses_and_malformed_recipients_are_excluded() {
        let db = db_with_accounts();
        let thread = new_thread(&db, "acct-a", "mixed");
        add_message(
            &db,
            &thread,
            1,
            "Sent",
            "me@example.test",
            "me@example.test, personal@example.org, not-an-email, , real@x.test",
            Some(", ,"),
            Some("root@localhost"),
            "2026-08-01T00:00:00",
            true,
        );

        let page = db.aggregate_sent_relationships("acct-a", NOW, 25).unwrap();

        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].counterparty_email, "real@x.test");
    }

    /// A `+tag` variant of a configured account address at the same domain is
    /// still the operator (`me+receipts@example.test` for `me@example.test`),
    /// for any configured account. The same base at a different domain is
    /// someone else.
    #[test]
    fn plus_addressed_variants_of_own_accounts_are_excluded() {
        let db = db_with_accounts();
        let thread = new_thread(&db, "acct-a", "receipts");
        add_message(
            &db,
            &thread,
            1,
            "Sent",
            "me@example.test",
            "me+receipts@example.test, personal+travel@example.org, real@x.test",
            None,
            None,
            "2026-08-01T00:00:00",
            true,
        );
        add_message(
            &db,
            &thread,
            2,
            "Sent",
            "me@example.test",
            "me+tag@elsewhere.test",
            None,
            None,
            "2026-08-02T00:00:00",
            true,
        );

        let page = db.aggregate_sent_relationships("acct-a", NOW, 25).unwrap();

        assert_eq!(page.total, 2);
        let mut emails: Vec<&str> = page
            .items
            .iter()
            .map(|rel| rel.counterparty_email.as_str())
            .collect();
        emails.sort_unstable();
        assert_eq!(emails, vec!["me+tag@elsewhere.test", "real@x.test"]);
    }

    /// Self-exclusion must not grow into alias normalization: an external
    /// plus-address stays a distinct counterparty under its verbatim identity,
    /// even at the same domain as a configured account, and independent tags
    /// are never grouped together or collapsed onto the base address.
    #[test]
    fn external_plus_addresses_stay_distinct_counterparties() {
        let db = db_with_accounts();
        let thread = new_thread(&db, "acct-a", "vendor");
        outbound(
            &db,
            &thread,
            1,
            "vendor+trip@example.test",
            "2026-08-01T00:00:00",
        );
        outbound(
            &db,
            &thread,
            2,
            "vendor+hotel@example.test",
            "2026-08-02T00:00:00",
        );
        outbound(
            &db,
            &thread,
            3,
            "vendor@example.test",
            "2026-08-03T00:00:00",
        );

        let page = db.aggregate_sent_relationships("acct-a", NOW, 25).unwrap();

        assert_eq!(page.total, 3);
        let mut emails: Vec<&str> = page
            .items
            .iter()
            .map(|rel| rel.counterparty_email.as_str())
            .collect();
        emails.sort_unstable();
        assert_eq!(
            emails,
            vec![
                "vendor+hotel@example.test",
                "vendor+trip@example.test",
                "vendor@example.test"
            ]
        );
        for rel in &page.items {
            assert_eq!(rel.outbound_count, 1, "{}", rel.counterparty_email);
        }
    }

    /// The page carries relationship identity and aggregate counts only. The
    /// fixture plants sentinel subject/snippet text on every message and a
    /// sentinel thread label; none of it may appear anywhere in the output.
    #[test]
    fn page_carries_no_subject_snippet_or_thread_label_material() {
        let db = db_with_accounts();
        let thread = new_thread(&db, "acct-a", "thread-label-sentinel-private");
        outbound(
            &db,
            &thread,
            1,
            "vendor+trip@example.test",
            "2026-08-01T00:00:00",
        );
        inbound(
            &db,
            &thread,
            2,
            "vendor+trip@example.test",
            "2026-08-02T00:00:00",
        );

        let page = db.aggregate_sent_relationships("acct-a", NOW, 25).unwrap();

        assert_eq!(page.total, 1);
        let rendered = format!("{page:?}");
        assert!(
            !rendered.contains("sentinel"),
            "aggregate must not carry subject, snippet, or thread label material: {rendered}"
        );
    }

    /// An inbound-only sender is not a sent relationship: nothing qualifies
    /// without at least one outbound message to the address.
    #[test]
    fn inbound_only_senders_are_not_sent_relationships() {
        let db = db_with_accounts();
        let thread = new_thread(&db, "acct-a", "newsletter");
        for uid in 1..=3 {
            inbound(
                &db,
                &thread,
                uid,
                "newsletter@x.test",
                "2026-08-01T00:00:00",
            );
        }

        let page = db.aggregate_sent_relationships("acct-a", NOW, 25).unwrap();

        assert_eq!(page.total, 0);
        assert!(page.items.is_empty());
    }

    /// The same counterparty observed from two accounts stays two separate
    /// account-scoped observations with independent counts.
    #[test]
    fn accounts_remain_separate_observations() {
        let db = db_with_accounts();
        let thread_a = new_thread(&db, "acct-a", "shared");
        outbound(&db, &thread_a, 1, "shared@x.test", "2026-08-01T00:00:00");
        outbound(&db, &thread_a, 2, "shared@x.test", "2026-08-02T00:00:00");
        let thread_b = new_thread(&db, "acct-b", "shared");
        add_message(
            &db,
            &thread_b,
            1,
            "Sent",
            "personal@example.org",
            "shared@x.test",
            None,
            None,
            "2026-08-03T00:00:00",
            true,
        );

        let page_a = db.aggregate_sent_relationships("acct-a", NOW, 25).unwrap();
        let page_b = db.aggregate_sent_relationships("acct-b", NOW, 25).unwrap();

        assert_eq!(page_a.total, 1);
        assert_eq!(page_a.items[0].account_id, "acct-a");
        assert_eq!(page_a.items[0].outbound_count, 2);
        assert_eq!(page_b.total, 1);
        assert_eq!(page_b.items[0].account_id, "acct-b");
        assert_eq!(page_b.items[0].outbound_count, 1);
    }

    /// Signal derivation is fixed and truthful: recency splits outbound-
    /// dominant relationships, meaningful inbound reads as bilateral, and
    /// rows order by outbound volume first.
    #[test]
    fn signals_derive_from_topology_and_recency_and_rows_order_by_volume() {
        let db = db_with_accounts();

        // 20 outbound / 2 inbound — exactly at the one-way ratio — with a
        // recent last message: recent outbound history.
        let many = new_thread(&db, "acct-a", "many");
        for uid in 1..=20 {
            outbound(&db, &many, uid, "many@x.test", "2026-08-20T00:00:00");
        }
        inbound(&db, &many, 8001, "many@x.test", "2026-07-01T00:00:00");
        inbound(&db, &many, 8002, "many@x.test", "2026-07-02T00:00:00");

        // 3 outbound / 2 inbound — below the ratio — reads bilateral however
        // old it is, and the latest inbound date sets last_observed.
        let both = new_thread(&db, "acct-a", "both");
        for uid in 21..=23 {
            outbound(&db, &both, uid, "both@x.test", "2025-01-01T00:00:00");
        }
        inbound(&db, &both, 8003, "both@x.test", "2025-02-01T00:00:00");
        inbound(&db, &both, 8004, "both@x.test", "2025-03-01T00:00:00");

        // 2 outbound, nothing inbound, quiet since 2025: historical one-way.
        let old = new_thread(&db, "acct-a", "old");
        outbound(&db, &old, 24, "old@x.test", "2025-01-01T00:00:00");
        outbound(&db, &old, 25, "old@x.test", "2025-01-02T00:00:00");

        // 1 outbound five days ago: recent outbound history.
        let fresh = new_thread(&db, "acct-a", "fresh");
        outbound(&db, &fresh, 26, "fresh@x.test", "2026-08-25T00:00:00");

        let page = db.aggregate_sent_relationships("acct-a", NOW, 25).unwrap();

        assert_eq!(page.total, 4);
        let order: Vec<&str> = page
            .items
            .iter()
            .map(|rel| rel.counterparty_email.as_str())
            .collect();
        assert_eq!(
            order,
            vec!["many@x.test", "both@x.test", "old@x.test", "fresh@x.test"]
        );
        let by_email = |email: &str| {
            page.items
                .iter()
                .find(|rel| rel.counterparty_email == email)
                .unwrap()
        };
        assert_eq!(
            by_email("many@x.test").signal,
            SentRelationshipSignal::RecentOutboundHistory
        );
        assert_eq!(
            by_email("both@x.test").signal,
            SentRelationshipSignal::BilateralHistory
        );
        assert_eq!(
            by_email("both@x.test").last_observed.as_deref(),
            Some("2025-03-01T00:00:00")
        );
        assert_eq!(
            by_email("old@x.test").signal,
            SentRelationshipSignal::HistoricalOneWay
        );
        assert_eq!(
            by_email("fresh@x.test").signal,
            SentRelationshipSignal::RecentOutboundHistory
        );
    }

    /// The cap sheds the lowest-volume rows and reports the uncapped total,
    /// so a capped page can disclose "showing N of M" truthfully.
    #[test]
    fn capping_reports_true_total_and_keeps_highest_volume_rows() {
        let db = db_with_accounts();
        let mut uid = 0u32;
        for c in 0..6u32 {
            let thread = new_thread(&db, "acct-a", &format!("c{c}"));
            // c0 gets 6 outbound, c5 gets 1.
            for _ in 0..(6 - c) {
                uid += 1;
                outbound(
                    &db,
                    &thread,
                    uid,
                    &format!("c{c}@x.test"),
                    "2026-08-01T00:00:00",
                );
            }
        }

        let page = db.aggregate_sent_relationships("acct-a", NOW, 3).unwrap();

        assert_eq!(page.total, 6);
        assert_eq!(page.items.len(), 3);
        let kept: Vec<&str> = page
            .items
            .iter()
            .map(|rel| rel.counterparty_email.as_str())
            .collect();
        assert_eq!(kept, vec!["c0@x.test", "c1@x.test", "c2@x.test"]);
    }

    /// The aggregate is strictly read-only: repeated calls return identical
    /// pages, the thread cache is untouched, and no address-history reconcile
    /// state appears (proving no index refresh piggybacked on the read).
    #[test]
    fn aggregate_is_read_only_against_the_store() {
        let db = db_with_accounts();
        let thread = new_thread(&db, "acct-a", "trip");
        outbound(&db, &thread, 1, "plans@tripit.com", "2025-11-15T00:00:00");
        inbound(&db, &thread, 2, "plans@tripit.com", "2025-07-01T00:00:00");

        let rows_before: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM thread_messages", [], |r| r.get(0))
            .unwrap();

        let first = db.aggregate_sent_relationships("acct-a", NOW, 25).unwrap();
        let second = db.aggregate_sent_relationships("acct-a", NOW, 25).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.total, 1);
        let rows_after: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM thread_messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows_before, rows_after);
        let reconcile_rows: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM address_history_state", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            reconcile_rows, 0,
            "the aggregate must not reconcile or refresh any derived index"
        );
    }
}
