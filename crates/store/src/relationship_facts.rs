// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Bounded, account-scoped relationship facts for outbound Governor attribution.
//!
//! This deliberately reads only local durable state: curated/derived contacts and
//! cached thread headers. It never opens IMAP, reconciles address history, or
//! inspects message subjects, snippets, or bodies. An exhausted bounded scan is
//! *unknown*, not evidence that a recipient or domain is new.
//!
//! Every favorable fact here must come from evidence an inbound message cannot
//! mint. Outbound history counts only from the account's detected Sent-role
//! folder: a cached row whose From merely equals the account address anywhere
//! else (a spoofed inbound in INBOX, say) is not the account's mail. A contact
//! vouches only when a person curated it (`history_derived = 0`); rows an agent
//! wrote over MCP, or an inbox import copied, carry
//! [`crate::contacts::AGENT_CURATED`] and never do.

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

/// Rows that are the account's own verified outbound mail: flagged outbound
/// *and* cached from the folder detected as its Sent mailbox. `?1` is the
/// account id; callers append further predicates.
const VERIFIED_OUTBOUND_ROWS: &str = "FROM thread_messages tm
     JOIN threads t ON t.thread_id = tm.thread_id
     JOIN detected_folders df
       ON df.account_id = t.account_id AND df.folder_type = 'sent' AND df.folder_name = tm.folder
     WHERE t.account_id = ?1 AND tm.is_outbound = 1";

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
    /// `known_contact` is true only when every recipient is human-curated or
    /// observed in verified outbound correspondence. Inbound-only mail, a
    /// self-From outside the Sent folder, agent-curated contacts, unverified
    /// header links, and display names never establish a favorable fact.
    /// `cold_email` is true when any recipient has no such evidence and the
    /// bounded history scan completed, so a stranger added beside a known
    /// recipient keeps its first-contact signal; it is false only when every
    /// recipient is known. `unknown_domain` follows the same rule per domain.
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
        let all_known = observations.iter().all(|o| o.known);
        let any_unknown = !all_known;
        let all_domains_complete = observations.iter().all(|o| o.domain_complete);
        let all_domains_seen = observations.iter().all(|o| o.domain_seen);
        let any_domain_unseen = !all_domains_seen;
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
            cold_email: if all_known {
                Some(false)
            } else if any_unknown && all_history_complete {
                Some(true)
            } else {
                None
            },
            unknown_domain: if all_domains_seen {
                Some(false)
            } else if any_domain_unseen && all_domains_complete {
                Some(true)
            } else {
                None
            },
        })
    }

    /// Whether a send answering `in_reply_to` earns reply credit
    /// (`reply_to_thread`, which also rules out `cold_email`).
    ///
    /// The header is caller-supplied, and the parent may be an attacker's own
    /// message, so neither its presence nor its resolving proves a
    /// relationship. Credit needs both: the parent's Message-ID resolves to
    /// exactly one cached thread in this account, and every recipient already
    /// appears on a verified outbound message (see [`VERIFIED_OUTBOUND_ROWS`])
    /// in that thread. Being in the thread through inbound mail never counts,
    /// and a recipient new to the thread (an added Cc) forfeits the credit.
    /// Anything unresolved, ambiguous, or past the scan bound is `false`, and
    /// the send is then scored as a fresh compose.
    pub fn is_verified_reply(
        &self,
        account_id: &str,
        in_reply_to: &str,
        to: &str,
        cc: Option<&str>,
        bcc: Option<&str>,
    ) -> Result<bool> {
        let parent = crate::threads::normalize_message_id(
            in_reply_to.split_whitespace().next().unwrap_or_default(),
        );
        if parent.is_empty() {
            return Ok(false);
        }
        let recipients = recipient_addresses(to, cc, bcc);
        if recipients.is_empty() || recipients.len() > RELATIONSHIP_FACT_RECIPIENT_LIMIT {
            return Ok(false);
        }

        let mut stmt = self.conn().prepare(
            "SELECT DISTINCT tm.thread_id
             FROM thread_messages tm
             JOIN threads t ON t.thread_id = tm.thread_id
             WHERE t.account_id = ?1 AND trim(trim(tm.message_id), '<>') = ?2
             LIMIT 2",
        )?;
        let threads = stmt
            .query_map(params![account_id, parent], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let [thread_id] = threads.as_slice() else {
            return Ok(false);
        };

        let mut stmt = self.conn().prepare(&format!(
            "SELECT tm.from_address, tm.to_addresses, tm.cc_addresses, tm.bcc_addresses
             {VERIFIED_OUTBOUND_ROWS} AND tm.thread_id = ?2
             LIMIT ?3"
        ))?;
        let rows = stmt
            .query_map(
                params![
                    account_id,
                    thread_id,
                    (RELATIONSHIP_FACT_THREAD_SCAN_LIMIT + 1) as i64
                ],
                |row| {
                    Ok([
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ])
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if rows.len() > RELATIONSHIP_FACT_THREAD_SCAN_LIMIT {
            return Ok(false);
        }
        let correspondents: HashSet<String> = rows
            .iter()
            .flatten()
            .flatten()
            .flat_map(|header| parse_address_list(header))
            .map(|address| address.email)
            .collect();
        Ok(recipients
            .iter()
            .all(|recipient| correspondents.contains(recipient)))
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
                 WHERE account_id = ?1 AND lower(email) = ?2 AND COALESCE(history_derived, 0) = 0
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

        let mut stmt = self.conn().prepare(&format!(
            "SELECT tm.from_address, tm.to_addresses, tm.cc_addresses, tm.bcc_addresses, tm.date
             {VERIFIED_OUTBOUND_ROWS}
             ORDER BY tm.id DESC
             LIMIT ?2"
        ))?;
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
        db.set_detected_folder("acc", "sent", "Sent").unwrap();
        db
    }

    /// Cache one message in `thread_id`, or in a new thread. `is_outbound` is
    /// written as given, so a test can reproduce rows an earlier indexer
    /// stored for any folder.
    #[allow(clippy::too_many_arguments)]
    fn store_message(
        db: &Database,
        thread_id: Option<&str>,
        message_id: &str,
        folder: &str,
        from: &str,
        to: &str,
        cc: Option<&str>,
        is_outbound: bool,
    ) -> String {
        let thread_id = match thread_id {
            Some(id) => id.to_string(),
            None => {
                db.create_thread(
                    "relationship test",
                    "2026-09-01T00:00:00Z",
                    "2026-09-01T00:00:00Z",
                    "acc",
                )
                .expect("create thread")
                .thread_id
            }
        };
        let uid = db
            .conn()
            .query_row("SELECT COUNT(*) + 1 FROM thread_messages", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap() as u32;
        db.upsert_thread_message(
            &thread_id,
            uid,
            Some(message_id),
            None,
            None,
            folder,
            from,
            to,
            cc,
            None,
            &chrono::Utc::now().to_rfc3339(),
            "subject",
            is_outbound,
            None,
        )
        .expect("insert thread message");
        thread_id
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
            if is_outbound { "Sent" } else { "INBOX" },
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
    fn curated_contact_or_outbound_correspondence_is_known_host_history() {
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
    fn inbound_only_correspondence_does_not_create_favorable_fact() {
        let db = db_with_account();
        add_thread_message(
            &db,
            "inbound-only@example.net",
            false,
            "2026-09-01T00:00:00Z",
        );
        let facts = db
            .derive_outbound_relationship_facts("acc", "inbound-only@example.net", None, None)
            .unwrap();
        assert_ne!(facts.known_contact, Some(true));
        assert_ne!(facts.frequent_contact, Some(true));
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
                "Sent",
                "other@example.test",
                "me@example.test",
                None,
                None,
                "2026-09-01T00:00:00Z",
                "subject",
                true,
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

    /// Injection path: an inbound message forging `From: <account>` and
    /// addressed to the attacker was indexed as outbound because its From
    /// matched. Outside the Sent-role folder it must never vouch for anyone.
    #[test]
    fn spoofed_self_from_outside_the_sent_folder_never_makes_a_recipient_known() {
        let db = db_with_account();
        for index in 0..5 {
            for folder in ["INBOX", "Junk"] {
                store_message(
                    &db,
                    None,
                    &format!("spoof-{folder}-{index}@evil.test"),
                    folder,
                    "me@example.test",
                    "attacker@evil.test",
                    None,
                    true,
                );
            }
        }
        let facts = db
            .derive_outbound_relationship_facts("acc", "attacker@evil.test", None, None)
            .unwrap();
        assert_eq!(facts.known_contact, Some(false), "{facts:?}");
        assert_ne!(facts.frequent_contact, Some(true), "{facts:?}");
        assert_eq!(facts.cold_email, Some(true), "{facts:?}");
        assert_eq!(facts.unknown_domain, Some(true), "{facts:?}");
    }

    /// The same self-From row in the Sent-role folder is the account's own
    /// outbound mail and does count.
    #[test]
    fn self_from_in_the_sent_folder_is_verified_outbound_history() {
        let db = db_with_account();
        store_message(
            &db,
            None,
            "real@example.test",
            "Sent",
            "me@example.test",
            "friend@example.net",
            None,
            true,
        );
        let facts = db
            .derive_outbound_relationship_facts("acc", "friend@example.net", None, None)
            .unwrap();
        assert_eq!(facts.known_contact, Some(true), "{facts:?}");
        assert_eq!(facts.cold_email, Some(false), "{facts:?}");
        assert_eq!(facts.unknown_domain, Some(false), "{facts:?}");
    }

    /// Injection path: adding a stranger beside a known recipient used to clear
    /// `cold_email` and `unknown_domain` for the whole send. The unknown
    /// recipient keeps its first-contact signals.
    #[test]
    fn a_mixed_recipient_set_keeps_first_contact_signals_for_the_unknown_recipient() {
        let db = db_with_account();
        add_thread_message(&db, "known@example.net", true, "2026-09-01T00:00:00Z");

        let new_domain = db
            .derive_outbound_relationship_facts(
                "acc",
                "known@example.net",
                Some("attacker@evil.test"),
                None,
            )
            .unwrap();
        assert_eq!(new_domain.known_contact, Some(false), "{new_domain:?}");
        assert_eq!(new_domain.cold_email, Some(true), "{new_domain:?}");
        assert_eq!(new_domain.unknown_domain, Some(true), "{new_domain:?}");

        let seen_domain = db
            .derive_outbound_relationship_facts(
                "acc",
                "known@example.net",
                None,
                Some("stranger@example.net"),
            )
            .unwrap();
        assert_eq!(seen_domain.cold_email, Some(true), "{seen_domain:?}");
        assert_eq!(seen_domain.unknown_domain, Some(false), "{seen_domain:?}");
    }

    /// A thread the account took part in: its own message to
    /// `known@example.net` in Sent, then that contact's answer in INBOX.
    fn verified_thread(db: &Database) -> String {
        let thread = store_message(
            db,
            None,
            "<mine-1@example.test>",
            "Sent",
            "me@example.test",
            "known@example.net",
            None,
            true,
        );
        store_message(
            db,
            Some(&thread),
            "<theirs-2@example.net>",
            "INBOX",
            "known@example.net",
            "me@example.test",
            None,
            false,
        );
        thread
    }

    #[test]
    fn a_reply_to_a_thread_the_account_wrote_to_is_verified() {
        let db = db_with_account();
        verified_thread(&db);
        for parent in [
            "<theirs-2@example.net>",
            "theirs-2@example.net",
            "<mine-1@example.test>",
        ] {
            assert!(
                db.is_verified_reply("acc", parent, "known@example.net", None, None)
                    .unwrap(),
                "{parent}"
            );
        }
    }

    #[test]
    fn an_unresolved_in_reply_to_is_not_a_verified_reply() {
        let db = db_with_account();
        verified_thread(&db);
        for parent in ["<forged@nowhere.test>", "", "   "] {
            assert!(
                !db.is_verified_reply("acc", parent, "known@example.net", None, None)
                    .unwrap(),
                "{parent:?}"
            );
        }
        // Another account's thread never counts.
        assert!(
            !db.is_verified_reply(
                "other",
                "<theirs-2@example.net>",
                "known@example.net",
                None,
                None
            )
            .unwrap()
        );
    }

    #[test]
    fn a_reply_to_a_strangers_thread_is_not_verified() {
        let db = db_with_account();
        let thread = store_message(
            &db,
            None,
            "<attack@evil.test>",
            "INBOX",
            "attacker@evil.test",
            "me@example.test",
            None,
            false,
        );
        // Nor does a forged self-From the attacker threaded in with it.
        store_message(
            &db,
            Some(&thread),
            "<spoof@evil.test>",
            "INBOX",
            "me@example.test",
            "attacker@evil.test",
            None,
            true,
        );
        assert!(
            !db.is_verified_reply(
                "acc",
                "<attack@evil.test>",
                "attacker@evil.test",
                None,
                None
            )
            .unwrap()
        );
    }

    #[test]
    fn a_recipient_new_to_the_thread_forfeits_reply_credit() {
        let db = db_with_account();
        let thread = verified_thread(&db);
        // The attacker joined the thread only through inbound mail.
        store_message(
            &db,
            Some(&thread),
            "<poison@evil.test>",
            "INBOX",
            "attacker@evil.test",
            "me@example.test",
            Some("known@example.net"),
            false,
        );
        for (to, cc) in [
            ("known@example.net", Some("attacker@evil.test")),
            ("attacker@evil.test", None),
        ] {
            assert!(
                !db.is_verified_reply("acc", "<theirs-2@example.net>", to, cc, None)
                    .unwrap(),
                "{to} {cc:?}"
            );
        }
    }

    #[test]
    fn a_message_id_cached_in_two_threads_is_ambiguous() {
        let db = db_with_account();
        verified_thread(&db);
        store_message(
            &db,
            None,
            "<theirs-2@example.net>",
            "Sent",
            "me@example.test",
            "known@example.net",
            None,
            true,
        );
        assert!(
            !db.is_verified_reply(
                "acc",
                "<theirs-2@example.net>",
                "known@example.net",
                None,
                None
            )
            .unwrap()
        );
    }
}
