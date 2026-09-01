// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Bounded, account-scoped relationship facts for outbound Governor attribution.
//!
//! This deliberately reads only local durable state: curated/derived contacts and
//! cached thread headers. It never opens IMAP, reconciles address history, or
//! inspects message subjects, snippets, or bodies. An exhausted bounded scan is
//! *unknown*, not evidence that a recipient or domain is new.

use std::collections::HashSet;

use rusqlite::{OptionalExtension, params};

use crate::address_book::parse_address_list;
use crate::db::Database;
use crate::errors::Result;

/// Maximum distinct recipients examined for one outbound attribution decision.
/// Larger recipient sets are intentionally left unknown rather than turning a
/// Governor gate into an unbounded mailbox walk.
pub const RELATIONSHIP_FACT_RECIPIENT_LIMIT: usize = 8;

/// Thread-header rows examined per recipient. The extra row detects truncation;
/// a missing match after a truncated scan remains unknown.
const RELATIONSHIP_FACT_THREAD_SCAN_LIMIT: usize = 256;

/// Sanitized, tri-state relationship observations suitable for
/// `AttributedSendContext`. This type deliberately carries no addresses, header
/// values, snippets, subjects, or message bodies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RelationshipFacts {
    pub known_contact: Option<bool>,
    pub frequent_contact: Option<bool>,
    pub cold_email: Option<bool>,
    pub unknown_domain: Option<bool>,
}

#[derive(Default)]
struct RecipientObservation {
    known: bool,
    history_complete: bool,
    domain_seen: bool,
    domain_complete: bool,
    recent_messages: usize,
}

impl Database {
    /// Derive relationship facts for an outbound recipient set from this
    /// account's actual contact rows and cached correspondence headers.
    ///
    /// `known_contact` is true only when every recipient is present in contacts
    /// or observed in correspondence. `cold_email` is true only when every
    /// recipient has no contact/history evidence *and* every bounded history scan
    /// completed. Mixed sets resolve both facts false; this prevents contradictory
    /// relationship labels. `unknown_domain` follows the same complete-scan rule.
    ///
    /// The scan is intentionally bounded. If a recipient/domain is not found
    /// before the cap, absence is not asserted and its facts remain `None`.
    pub fn derive_outbound_relationship_facts(
        &self,
        account_id: &str,
        to: &str,
        cc: Option<&str>,
        bcc: Option<&str>,
    ) -> Result<RelationshipFacts> {
        let recipients = recipient_addresses(to, cc, bcc);
        if recipients.is_empty() || recipients.len() > RELATIONSHIP_FACT_RECIPIENT_LIMIT {
            return Ok(RelationshipFacts::default());
        }

        let mut observations = Vec::with_capacity(recipients.len());
        for recipient in recipients {
            observations.push(self.observe_recipient_relationship(account_id, &recipient)?);
        }

        let all_history_complete = observations.iter().all(|o| o.history_complete);
        let any_known = observations.iter().any(|o| o.known);
        let all_known = observations.iter().all(|o| o.known);
        let all_unknown = observations.iter().all(|o| !o.known);
        let all_domains_complete = observations.iter().all(|o| o.domain_complete);
        let any_domain_seen = observations.iter().any(|o| o.domain_seen);
        let all_domains_unseen = observations.iter().all(|o| !o.domain_seen);
        let all_frequent = observations.iter().all(|o| o.recent_messages >= 5);

        Ok(RelationshipFacts {
            known_contact: if all_known {
                Some(true)
            } else if all_history_complete {
                Some(false)
            } else {
                None
            },
            // A positive frequency observation is useful and cannot conflict with
            // `known_contact`; absence is intentionally unknown rather than a
            // claim about incomplete or malformed timestamp coverage.
            frequent_contact: all_frequent.then_some(true),
            cold_email: if all_unknown && all_history_complete {
                Some(true)
            } else if any_known {
                Some(false)
            } else {
                None
            },
            unknown_domain: if all_domains_unseen && all_domains_complete {
                Some(true)
            } else if any_domain_seen {
                Some(false)
            } else {
                None
            },
        })
    }

    fn observe_recipient_relationship(
        &self,
        account_id: &str,
        recipient: &str,
    ) -> Result<RecipientObservation> {
        let contact_exists: bool = self
            .conn()
            .query_row(
                "SELECT 1 FROM contacts
                 WHERE account_id = ?1 AND lower(email) = ?2
                 LIMIT 1",
                params![account_id, recipient],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        let domain = recipient
            .rsplit_once('@')
            .map(|(_, domain)| domain)
            .unwrap_or("");
        let mut observation = RecipientObservation {
            known: contact_exists,
            ..Default::default()
        };

        let mut stmt = self.conn().prepare(
            "SELECT tm.from_address, tm.to_addresses, tm.cc_addresses, tm.bcc_addresses, tm.date
             FROM thread_messages tm
             JOIN threads t ON t.thread_id = tm.thread_id
             WHERE t.account_id = ?1
             ORDER BY tm.id DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![account_id, (RELATIONSHIP_FACT_THREAD_SCAN_LIMIT + 1) as i64],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )?;

        let mut scanned = 0usize;
        let mut truncated = false;
        let recent_floor = chrono::Utc::now() - chrono::Duration::days(30);
        for row in rows {
            if scanned == RELATIONSHIP_FACT_THREAD_SCAN_LIMIT {
                truncated = true;
                break;
            }
            scanned += 1;
            let (from, to, cc, bcc, date) = row?;
            let addresses = [
                from.as_deref(),
                to.as_deref(),
                cc.as_deref(),
                bcc.as_deref(),
            ]
            .into_iter()
            .flatten()
            .flat_map(parse_address_list)
            .collect::<Vec<_>>();
            let recipient_seen = addresses.iter().any(|address| address.email == recipient);
            if recipient_seen {
                observation.known = true;
                if date
                    .as_deref()
                    .and_then(parse_timestamp)
                    .is_some_and(|at| at >= recent_floor)
                {
                    observation.recent_messages += 1;
                }
            }
            if addresses.iter().any(|address| {
                address
                    .email
                    .rsplit_once('@')
                    .is_some_and(|(_, address_domain)| address_domain == domain)
            }) {
                observation.domain_seen = true;
            }
        }

        // We queried one extra row so absence becomes a negative fact only when
        // the bounded scan actually exhausted the account's cached history.
        observation.history_complete = !truncated;
        observation.domain_complete = observation.history_complete;
        Ok(observation)
    }
}

fn recipient_addresses(to: &str, cc: Option<&str>, bcc: Option<&str>) -> Vec<String> {
    let mut recipients = HashSet::new();
    for raw in [Some(to), cc, bcc].into_iter().flatten() {
        for address in parse_address_list(raw) {
            recipients.insert(address.email);
        }
    }
    let mut recipients: Vec<String> = recipients.into_iter().collect();
    recipients.sort_unstable();
    recipients
}

fn parse_timestamp(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|date| date.with_timezone(&chrono::Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S")
                .map(|date| date.and_utc())
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Contact;

    fn db_with_account() -> Database {
        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acc', 'Account', 'me@example.test', 'example.test',
                         'smtp.example.test', 587, 'imap.example.test', 993, 'encrypted')",
                [],
            )
            .unwrap();
        db
    }

    fn add_thread_message(db: &Database, recipient: &str, is_outbound: bool, date: &str) {
        let thread = db
            .create_thread("relationship test", date, date, "acc")
            .expect("create thread");
        db.upsert_thread_message(
            &thread.thread_id,
            1,
            Some("message-id@example.test"),
            None,
            None,
            "INBOX",
            if is_outbound {
                "me@example.test"
            } else {
                recipient
            },
            if is_outbound {
                recipient
            } else {
                "me@example.test"
            },
            None,
            None,
            date,
            "subject",
            is_outbound,
            None,
        )
        .expect("insert thread message");
    }

    #[test]
    fn contact_or_current_correspondence_is_known_host_history() {
        let db = db_with_account();
        db.upsert_contact(&Contact {
            id: "contact".into(),
            account_id: "acc".into(),
            email: "curated@example.net".into(),
            name: None,
            tags: "[]".into(),
            notes: None,
            message_count: 0,
            first_seen: None,
            last_seen: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        })
        .unwrap();
        let contact = db
            .derive_outbound_relationship_facts("acc", "curated@example.net", None, None)
            .unwrap();
        assert_eq!(contact.known_contact, Some(true));
        assert_eq!(contact.cold_email, Some(false));

        add_thread_message(&db, "current@example.net", true, "2026-09-01T00:00:00Z");
        let correspondence = db
            .derive_outbound_relationship_facts("acc", "current@example.net", None, None)
            .unwrap();
        assert_eq!(correspondence.known_contact, Some(true));
        assert_eq!(correspondence.cold_email, Some(false));
        assert_eq!(correspondence.unknown_domain, Some(false));
    }

    #[test]
    fn genuinely_new_contact_is_cold_only_after_complete_history_lookup() {
        let db = db_with_account();
        let facts = db
            .derive_outbound_relationship_facts("acc", "new@example.net", None, None)
            .unwrap();
        assert_eq!(facts.known_contact, Some(false));
        assert_eq!(facts.cold_email, Some(true));
        assert_eq!(facts.unknown_domain, Some(true));

        for index in 0..=RELATIONSHIP_FACT_THREAD_SCAN_LIMIT {
            let thread = db
                .create_thread(
                    &format!("unrelated {index}"),
                    "2026-09-01T00:00:00Z",
                    "2026-09-01T00:00:00Z",
                    "acc",
                )
                .unwrap();
            db.upsert_thread_message(
                &thread.thread_id,
                index as u32 + 1,
                Some(&format!("unrelated-{index}@example.test")),
                None,
                None,
                "INBOX",
                "other@example.test",
                "me@example.test",
                None,
                None,
                "2026-09-01T00:00:00Z",
                "subject",
                false,
                None,
            )
            .unwrap();
        }
        let bounded = db
            .derive_outbound_relationship_facts("acc", "unseen@example.net", None, None)
            .unwrap();
        assert_eq!(bounded.known_contact, None);
        assert_eq!(bounded.cold_email, None);
        assert_eq!(bounded.unknown_domain, None);
    }
}
