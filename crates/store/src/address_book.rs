// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Recipient address history for compose autocomplete.
//!
//! This is a read/write view over the EXISTING `contacts` table — there is no
//! second address book. [`Database::reconcile_address_history`] folds locally
//! cached mail metadata into contacts, and [`Database::suggest_addresses`]
//! ranks them for the To/Cc/Bcc fields.
//!
//! Three local sources feed the history, all already on disk:
//!   * `thread_messages` joined to `threads` — the deep cache of everyone this
//!     account has corresponded with, inbound senders and outbound recipients
//!     alike. This is the bulk source: a real install holds six figures of
//!     rows here against a few hundred in the dashboard's index. From, To, Cc,
//!     and Bcc are all read; Cc/Bcc are `NULL` on rows the scan cached before
//!     Envelope retained them and fill in as scans revisit those folders.
//!   * `indexed_message_summaries` — the dashboard's rolling INBOX snapshot,
//!     which is where display names come from.
//!   * `drafts` with `status = 'sent'` — recipients you have written to but
//!     whose Sent copy the thread scan has not reached yet. This source is
//!     also folded in the instant a send becomes durable, so someone you just
//!     wrote to is suggestible without waiting for anything; see
//!     [`Database::record_sent_draft_recipients`].
//!
//! None of them is IMAP, so reconciling never opens a socket and typing in a
//! compose field never leaves the machine.
//!
//! ## Why a watermark, and what invalidates it
//!
//! `thread_messages.id` is `INTEGER PRIMARY KEY AUTOINCREMENT`, so it is
//! monotonic and never reused. `address_history_state.last_thread_message_id`
//! records how far up that sequence an account has been folded in;
//! reconciling picks up above the watermark and advances it in the same
//! transaction as the contact writes. A refresh therefore costs the rows that
//! actually arrived, not a rescan of the whole thread cache, and a crash
//! between the two either rolls both back or commits both.
//!
//! The watermark alone would be a lie, because `thread_messages` is not
//! append-only. Two paths change rows at or below it:
//!
//!   * `upsert_thread_message` rewrites a cached row in place when a folder is
//!     re-scanned, so a corrected From/To/Cc/Bcc, date, or direction would
//!     otherwise never be seen again;
//!   * `reset_folder_sync` deletes a folder's rows after a UIDVALIDITY change,
//!     and the rescan re-inserts the same messages under fresh ids — every one
//!     of which is above the watermark and would be counted twice.
//!
//! Both call [`Database::invalidate_address_history`], which sets
//! `address_history_state.dirty`. A dirty account reconciles like a
//! source-version bump: derived counts are dropped and re-folded from the
//! first thread message. The in-place path compares the stored address-bearing
//! fields against the incoming ones first, so an unchanged rescan — the common
//! case — costs nothing.
//!
//! The other two sources are rewritten in place (`upsert_indexed_message_summaries`
//! deletes and re-inserts each folder; a draft's timestamps move as it is
//! edited), so they cannot be watermarked. They are small and bounded, so
//! every completed reconcile recounts them from scratch and merges the total
//! as a high-water mark, which is idempotent by construction.
//!
//! ## What a reconcile may and may not overwrite
//!
//! `history_count` and `history_sent_count` are the derived half of the
//! interaction signal and the only contact columns this module owns.
//! `message_count`, `name`, `tags`, and `notes` belong to `envelope contacts
//! add|import` and manual edits: a reconcile fills a blank name and otherwise
//! leaves them alone, so a rebuild is free to lower or reset the derived count
//! without a stale copy of it surviving in the manual column. Suggestions rank
//! on whichever of the three is higher.
//!
//! ## Why the sent-draft count has its own column
//!
//! A send must not have to wait for a reconcile to make its recipients
//! suggestible, and the write that runs at the send edge must not distort the
//! count once the same message comes back around through the Sent folder.
//! Those two are only compatible if the immediate write and the reconcile
//! account for the message separately, so `history_sent_count` holds the
//! locally recorded sent-draft signal and `history_count` holds everything
//! else. The immediate edge writes only the former, the thread-cache fold
//! writes only the latter, and the suggestion signal takes the larger — so the
//! Sent-folder row arriving later lands on a count that never absorbed the
//! send edge's write, and comes out exactly where it would have without it.
//!
//! Both are recomputed floors, never increments: the immediate edge recounts
//! the same bounded sent-draft window the reconcile reads, so running it twice
//! for one send is indistinguishable from running it once. Once a reconcile
//! completes, `history_sent_count` is subsumed by `history_count` (which
//! recounts the same drafts among its own sources), so the column only
//! decides anything in the window between a send and the next reconcile.
//!
//! ## Who owns a row
//!
//! `contacts.history_derived` says whether a row is this module's. A row a
//! reconcile invented is marked derived; anything `envelope contacts` creates
//! or edits — adding a contact, tagging one, annotating one, including taking
//! over a row that started out derived — is manual from that point on.
//! Observing a manual address in a header does not change that: history writes
//! the derived count and nothing else.
//!
//! The distinction is what lets a rebuild finish the job. When it resets the
//! derived counts and re-folds them, a derived row left at zero is one whose
//! last source is gone — an address that only ever appeared in a header a
//! later scan corrected — and it is deleted rather than left in the dropdown
//! at zero signal. A manual row is never deleted, however little signal it
//! carries: a bare address someone added by hand is indistinguishable from a
//! swept derived row and is still theirs.

use std::collections::HashMap;

use email_address::EmailAddress;
use rusqlite::functions::FunctionFlags;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::db::Database;
use crate::errors::Result;

/// Version of the derivation itself (which sources are read, and how their
/// rows become counts). Bumping it makes the next reconcile drop
/// `history_count` for the account and re-fold from the first thread message,
/// which is how a parser fix reaches history that was already folded in.
///
/// 2: Cc/Bcc joined From/To as thread-cache sources.
pub const ADDRESS_HISTORY_SOURCE_VERSION: i64 = 2;

/// Thread-cache rows folded in per chunk. The first reconcile of an existing
/// install has six figures of rows to walk; chunking keeps any single call —
/// and any single hold of the dashboard's database mutex — bounded, and the
/// watermark makes the progress durable so the next chunk resumes rather than
/// restarts.
pub const ADDRESS_HISTORY_CHUNK_ROWS: usize = 5_000;

/// Most recent sent drafts recounted per reconcile. Sent drafts accumulate
/// without bound; the newest window carries the useful recipient signal and
/// older rows are already in the thread cache once their Sent copy is scanned.
const SENT_DRAFT_WINDOW: usize = 500;

/// Most contact rows a single suggestion query will rank. Candidates are drawn
/// strongest-match-first and only then strongest-signal-first, so the cap sheds
/// the least useful rows rather than an arbitrary slice — an exact or prefix
/// match survives it however quiet the contact is.
const SUGGESTION_SCAN_CAP: usize = 2000;

/// Rank given to a row that does not match at all, or whose stored address is
/// unusable. Weaker than every real tier, so those rows sort last and cannot
/// displace a match at [`SUGGESTION_SCAN_CAP`].
const NO_MATCH_RANK: i64 = 3;

/// Name the ranking function is registered under for the suggestion query.
const MATCH_RANK_FN: &str = "envelope_match_rank";

/// The suggestion read, in one place so the plan assertions in the tests and
/// the scale probe pin the query that actually runs.
///
/// Textual match strength is computed in SQL and ordered on BEFORE the cap:
/// otherwise an account with more matches than the cap could shed a
/// low-frequency exact match in favour of 2,000 high-frequency substring rows.
/// The `LIKE` in the WHERE clause is the cheap "matches at all" filter — every
/// tier [`match_rank`] returns implies containment — and the ranking function
/// then orders what survives.
///
/// `MAX(message_count, history_count, history_sent_count)` is the interaction
/// signal: `message_count` is what an import or a manual edit knows,
/// `history_count` is what the local cache derived, `history_sent_count` is
/// what the send edge recorded before any reconcile saw it, and whichever is
/// largest is the best available.
///
/// A derived row at zero signal is skipped. Between a rebuild's reset and the
/// chunk that re-folds its source, such a row is a contact whose history is
/// mid-recount; once the rebuild finishes it is one whose source is gone and
/// the reconcile has deleted it. Neither is worth offering. A manual row is
/// never skipped — an address someone added by hand is a suggestion at zero
/// signal exactly as it is at fifty.
const SUGGESTION_SQL: &str = "SELECT email, name,
                MAX(message_count, history_count, history_sent_count),
                COALESCE(last_seen, '')
         FROM contacts
         WHERE account_id = ?1
           AND (history_derived = 0
                OR MAX(message_count, history_count, history_sent_count) > 0)
           AND (?2 = ''
                OR lower(email) LIKE ?3 ESCAPE '\\'
                OR lower(COALESCE(name, '')) LIKE ?3 ESCAPE '\\')
         ORDER BY envelope_match_rank(email, name, ?2) ASC,
                  MAX(message_count, history_count, history_sent_count) DESC,
                  COALESCE(last_seen, '') DESC,
                  email ASC
         LIMIT ?4";

/// One ranked autocomplete row.
///
/// Address-book metadata only. Subjects, snippets, and bodies are never
/// carried here — the compose surfaces need an address and a name, and the
/// ranking signal stays server-side.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AddressSuggestion {
    pub email: String,
    pub name: Option<String>,
}

/// What a reconcile pass actually changed. Returned for logging and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AddressHistoryReconcile {
    /// Distinct normalized addresses observed in this pass.
    pub observed: usize,
    pub created: usize,
    pub updated: usize,
    /// Derived-only rows deleted because their last source is gone.
    pub removed: usize,
    /// Thread-cache rows folded in by this pass.
    pub thread_rows: usize,
    /// The pass dropped the derived counts and re-folded from the start.
    pub rebuilt: bool,
    /// More thread-cache rows remain above the watermark: call again.
    pub pending: bool,
}

impl AddressHistoryReconcile {
    fn absorb(&mut self, pass: &AddressHistoryReconcile) {
        self.observed += pass.observed;
        self.created += pass.created;
        self.updated += pass.updated;
        self.removed += pass.removed;
        self.thread_rows += pass.thread_rows;
        self.rebuilt |= pass.rebuilt;
        self.pending = pass.pending;
    }
}

/// A display name and address recovered from one header entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAddress {
    /// Lowercased, bracket-stripped address.
    pub email: String,
    /// Display name as written, when the entry had one.
    pub name: Option<String>,
}

/// Which kind of source an observation came from, which decides how its count
/// merges: thread-cache rows are new-since-the-watermark and add to the
/// derived count, recounted sources replace it only if they are higher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// Rows above the watermark — counted exactly once, ever.
    Thread,
    /// Rows recounted from scratch on every completed reconcile.
    Recount,
    /// A locally recorded sent draft. Recounted like any other recount
    /// source, and tallied a second time on its own so the send edge has a
    /// figure it can write without touching `history_count`.
    SentDraft,
}

/// Accumulated signal for one address before it is written back.
#[derive(Debug, Default, Clone)]
struct Observation {
    /// Interactions from thread rows above the watermark.
    delta: i64,
    /// Absolute interaction total from the recounted sources.
    recount: i64,
    /// Absolute interaction total from locally recorded sent drafts alone —
    /// the [`Source::SentDraft`] subset of `recount`.
    sent: i64,
    name: Option<String>,
    /// Timestamp of the observation `name` came from, so the newest display
    /// name wins regardless of the order the sources are walked in.
    name_at: Option<String>,
    first_seen: Option<String>,
    last_seen: Option<String>,
}

impl Observation {
    fn record(&mut self, source: Source, name: Option<String>, at: Option<String>) {
        match source {
            Source::Thread => self.delta += 1,
            Source::Recount => self.recount += 1,
            Source::SentDraft => {
                self.recount += 1;
                self.sent += 1;
            }
        }

        if let Some(name) = name.filter(|name| !name.trim().is_empty()) {
            let newer = match (&self.name_at, &at) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(current), Some(candidate)) => candidate > current,
            };
            if self.name.is_none() || newer {
                self.name = Some(name);
                self.name_at = at.clone();
            }
        }

        if let Some(at) = at {
            if self.first_seen.as_ref().is_none_or(|first| at < *first) {
                self.first_seen = Some(at.clone());
            }
            if self.last_seen.as_ref().is_none_or(|last| at > *last) {
                self.last_seen = Some(at);
            }
        }
    }

    /// The same accumulated signal with everything but the sent-draft tally
    /// dropped. This is what the send edge merges: it moves
    /// `history_sent_count` and leaves `history_count` — and therefore the
    /// message the Sent-folder scan will fold in later — entirely to the
    /// reconcile.
    fn sent_only(&self) -> Observation {
        Observation {
            delta: 0,
            recount: 0,
            sent: self.sent,
            name: self.name.clone(),
            name_at: self.name_at.clone(),
            first_seen: self.first_seen.clone(),
            last_seen: self.last_seen.clone(),
        }
    }
}

/// How strongly a candidate matched the typed query. Lower sorts first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MatchRank {
    Exact = 0,
    Prefix = 1,
    Substring = 2,
}

/// The stored contact a new observation is merged into, if one exists.
#[derive(Debug)]
struct ExistingContact {
    id: String,
    name: Option<String>,
    /// The derived counts only. `message_count` is not read here: a reconcile
    /// neither reads nor writes the manual counter.
    history_count: i64,
    history_sent_count: i64,
    first_seen: Option<String>,
    last_seen: Option<String>,
}

/// A contact row that matched, with the signal used to order it.
#[derive(Debug)]
struct Candidate {
    email: String,
    name: Option<String>,
    signal: i64,
    last_seen: String,
}

impl Database {
    /// Fold every locally cached address for one account into the contacts
    /// table, chunk by chunk, until the thread cache is caught up.
    ///
    /// Read-only against the network: all three sources are local SQLite
    /// tables. Safe to call repeatedly — the thread cache is read above a
    /// durable watermark and the recounted sources merge as a high-water
    /// mark, so a second call over unchanged data is a no-op.
    pub fn reconcile_address_history(&self, account_id: &str) -> Result<AddressHistoryReconcile> {
        let mut total = AddressHistoryReconcile::default();
        loop {
            let pass =
                self.reconcile_address_history_chunk(account_id, ADDRESS_HISTORY_CHUNK_ROWS)?;
            total.absorb(&pass);
            if !pass.pending {
                return Ok(total);
            }
        }
    }

    /// One bounded reconcile pass: at most `max_thread_rows` thread-cache rows.
    ///
    /// Returns with `pending = true` when more rows remain above the
    /// watermark. Callers that must not hold the database for long (the
    /// dashboard's startup backfill, which shares one mutex with every
    /// request) loop on this and release between passes; the watermark makes
    /// the progress durable either way.
    ///
    /// The recounted sources — the dashboard's INBOX snapshot and sent drafts
    /// — are folded in only on the final pass of a catch-up, since recounting
    /// them per chunk would be wasted work for an identical result.
    pub fn reconcile_address_history_chunk(
        &self,
        account_id: &str,
        max_thread_rows: usize,
    ) -> Result<AddressHistoryReconcile> {
        let max_thread_rows = max_thread_rows.max(1);
        let own_address = self.account_own_address(account_id)?;
        let own_address = own_address.as_deref();

        let tx = self.conn().unchecked_transaction()?;

        let (stored_version, stored_watermark, dirty) = load_state(&tx, account_id)?;
        let rebuilt = stored_version != ADDRESS_HISTORY_SOURCE_VERSION || dirty;
        let watermark = if rebuilt {
            // Either the derivation changed under us, or the thread rows below
            // the watermark did (an in-place header rewrite, or the
            // delete/reinsert a UIDVALIDITY reset performs). Drop the derived
            // half of the signal and re-fold from the first thread message;
            // message_count, names, tags, and notes are left exactly as they
            // are. Both derived columns reset together — leaving the sent-draft
            // count standing would keep a row alive past the disappearance of
            // the last source that justified it.
            tx.execute(
                "UPDATE contacts SET history_count = 0, history_sent_count = 0
                 WHERE account_id = ?1",
                params![account_id],
            )?;
            0
        } else {
            stored_watermark
        };

        let mut observations: HashMap<String, Observation> = HashMap::new();
        let mut highest = watermark;
        let mut thread_rows = 0usize;

        {
            let mut stmt = tx.prepare(
                "SELECT tm.id, tm.from_address, tm.to_addresses, tm.cc_addresses,
                        tm.bcc_addresses, tm.date, tm.is_outbound
                 FROM thread_messages tm
                 JOIN threads t ON t.thread_id = tm.thread_id
                 WHERE tm.id > ?2 AND t.account_id = ?1
                 ORDER BY tm.id
                 LIMIT ?3",
            )?;
            let rows = stmt.query_map(
                params![account_id, watermark, max_thread_rows as i64],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, i64>(6)? != 0,
                    ))
                },
            )?;
            for row in rows {
                let (id, from_address, to_addresses, cc, bcc, date, is_outbound) = row?;
                thread_rows += 1;
                highest = highest.max(id);
                let at = date.as_deref().and_then(normalize_timestamp);
                // On an outbound message the From line is this account (or one
                // of its aliases, which the username check would miss), so only
                // the recipients are history worth suggesting.
                if !is_outbound && let Some(from) = from_address.as_deref() {
                    record(from, &at, own_address, Source::Thread, &mut observations);
                }
                // Cc and Bcc are recipients exactly as the To line is. They are
                // NULL on rows cached before Envelope retained those headers.
                for recipients in [to_addresses.as_deref(), cc.as_deref(), bcc.as_deref()]
                    .into_iter()
                    .flatten()
                {
                    record(
                        recipients,
                        &at,
                        own_address,
                        Source::Thread,
                        &mut observations,
                    );
                }
            }
        }

        // A short pass means the thread cache is caught up; only then is it
        // worth recounting the two small sources.
        let pending = thread_rows == max_thread_rows;
        if !pending {
            {
                let mut stmt = tx.prepare(
                    "SELECT from_addr, to_addr, date
                     FROM indexed_message_summaries
                     WHERE account_id = ?1",
                )?;
                let rows = stmt.query_map(params![account_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })?;
                for row in rows {
                    let (from_addr, to_addr, date) = row?;
                    let at = date.as_deref().and_then(normalize_timestamp);
                    record(
                        &from_addr,
                        &at,
                        own_address,
                        Source::Recount,
                        &mut observations,
                    );
                    record(
                        &to_addr,
                        &at,
                        own_address,
                        Source::Recount,
                        &mut observations,
                    );
                }
            }

            record_sent_drafts(&tx, account_id, own_address, &mut observations)?;
        }

        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let mut summary = AddressHistoryReconcile {
            observed: observations.len(),
            thread_rows,
            rebuilt,
            pending,
            ..Default::default()
        };
        for (email, observed) in &observations {
            if merge_observation(&tx, account_id, email, observed, &now)? {
                summary.created += 1;
            } else {
                summary.updated += 1;
            }
        }

        // A pass that catches up has the final derived count for every address
        // the sources still carry, so a derived row left at zero is one whose
        // last source is gone — a header a rescan corrected, or a message a
        // folder wipe took with it. A derived row is created with at least one
        // interaction and only a rebuild's reset can take it back to zero, so
        // this deletes exactly what the rebuild orphaned, and a pass that is
        // still `pending` deletes nothing because the rows it has not reached
        // are not stale. Manual rows are out of scope by definition: a bare
        // address someone added by hand looks exactly like a swept derived row
        // and is still theirs.
        //
        // The predicate is the suggestion query's zero-signal skip, negated:
        // the two must stay identical, or a row could be offered in the
        // dropdown and swept by the very next pass.
        if !pending {
            summary.removed = tx.execute(
                "DELETE FROM contacts
                 WHERE account_id = ?1 AND history_derived = 1
                   AND MAX(message_count, history_count, history_sent_count) = 0",
                params![account_id],
            )?;
        }

        // Same transaction as the contact writes: a retry after a crash
        // re-reads the same rows rather than counting them a second time.
        // Clearing `dirty` here is safe even mid-catch-up — the rebuild has
        // already zeroed the derived counts and reset the watermark, so the
        // remaining chunks resume rather than restart, and an invalidation
        // raised after this commit lands on a fresh flag.
        tx.execute(
            "INSERT INTO address_history_state
                (account_id, source_version, last_thread_message_id, reconciled_at, dirty)
             VALUES (?1, ?2, ?3, ?4, 0)
             ON CONFLICT(account_id) DO UPDATE SET
                source_version = excluded.source_version,
                last_thread_message_id = excluded.last_thread_message_id,
                reconciled_at = excluded.reconciled_at,
                dirty = 0",
            params![
                account_id,
                ADDRESS_HISTORY_SOURCE_VERSION,
                highest,
                now.as_str()
            ],
        )?;
        tx.commit()?;

        Ok(summary)
    }

    /// Fold one just-sent draft's recipients into the address history, so they
    /// are suggestible the moment the send is durable.
    ///
    /// Called from [`Database::mark_draft_sent`], which is the single
    /// owner-token transition every send path takes after SMTP acceptance — so
    /// the CLI, MCP, the dashboard, and the scheduled sweep all reach this
    /// without a line of their own. Only a row that is already `sent` is read:
    /// a send that was refused, or one whose transition lost the ownership
    /// token, writes nothing.
    ///
    /// Reads and writes local SQLite only, in one transaction. To, Cc, and Bcc
    /// are all recipients; the account's own address is skipped exactly as it
    /// is everywhere else.
    ///
    /// The count written is not an increment. The whole bounded sent-draft
    /// window is recounted — the same window and the same parse the reconcile
    /// uses — and only this draft's addresses are written back, as a floor on
    /// `history_sent_count`. Calling it twice for one send therefore changes
    /// nothing, and because it never touches `history_count`, the Sent-folder
    /// copy the thread scan caches later folds in against a count that has not
    /// already absorbed this message.
    ///
    /// Returns how many distinct addresses were written.
    pub(crate) fn record_sent_draft_recipients(&self, draft_id: &str) -> Result<usize> {
        let recipients: Option<(String, String, Option<String>, Option<String>)> = self
            .conn()
            .query_row(
                "SELECT account_id, to_addr, cc_addr, bcc_addr
                 FROM drafts WHERE id = ?1 AND status = 'sent'",
                params![draft_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((account_id, to_addr, cc_addr, bcc_addr)) = recipients else {
            return Ok(0);
        };

        let own_address = self.account_own_address(&account_id)?;
        let own_address = own_address.as_deref();

        // The addresses this send is responsible for making suggestible.
        // Parsed and filtered exactly as the fold below parses them, so an
        // entry the reconcile would drop is dropped here too.
        let mut wanted: Vec<String> = Vec::new();
        for raw in [
            Some(to_addr.as_str()),
            cc_addr.as_deref(),
            bcc_addr.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            for parsed in parse_address_list(raw) {
                if own_address == Some(parsed.email.as_str()) || wanted.contains(&parsed.email) {
                    continue;
                }
                wanted.push(parsed.email);
            }
        }
        if wanted.is_empty() {
            return Ok(0);
        }

        let tx = self.conn().unchecked_transaction()?;
        let mut window: HashMap<String, Observation> = HashMap::new();
        record_sent_drafts(&tx, &account_id, own_address, &mut window)?;

        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let mut written = 0usize;
        for email in &wanted {
            // Absent only if this draft left the window between the two reads,
            // in which case there is no sent-draft signal left to write.
            let Some(observed) = window.get(email) else {
                continue;
            };
            merge_observation(&tx, &account_id, email, &observed.sent_only(), &now)?;
            written += 1;
        }
        tx.commit()?;

        Ok(written)
    }

    /// Ranked recipient suggestions for one account.
    ///
    /// Reads `contacts` and nothing else: the thread cache is never touched
    /// while someone is typing.
    ///
    /// Ordering is textual strength first (exact, then prefix, then
    /// substring), then interaction signal (frequency, then recency), then the
    /// address itself so equal rows never reorder between identical calls.
    ///
    /// A blank `query` is not an error here: it ranks the whole account by
    /// signal alone, which is the primitive a "recent contacts" affordance
    /// would need. The dashboard endpoint requires a query of its own accord.
    pub fn suggest_addresses(
        &self,
        account_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<AddressSuggestion>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let needle = query.trim().to_lowercase();

        register_match_rank(self.conn())?;
        let mut stmt = self.conn().prepare(SUGGESTION_SQL)?;
        let rows = stmt.query_map(
            params![
                account_id,
                needle,
                format!("%{}%", escape_like(&needle)),
                SUGGESTION_SCAN_CAP as i64
            ],
            |row| {
                Ok(Candidate {
                    email: row.get::<_, String>(0)?,
                    name: row.get::<_, Option<String>>(1)?,
                    signal: row.get(2)?,
                    last_seen: row.get(3)?,
                })
            },
        )?;

        // Dedupe case-insensitively. Rows written by this module are already
        // lowercased, but `envelope contacts add` stores the address as typed,
        // so `Alice@Example.com` and `alice@example.com` can coexist.
        let mut best: HashMap<String, (MatchRank, Candidate)> = HashMap::new();
        for row in rows {
            let candidate = row?;
            // A row can only reach here on a valid stored address; anything
            // malformed was rejected at write time and would not be usable in
            // a compose field.
            let Some(email) = normalize_email(&candidate.email) else {
                continue;
            };
            let Some(rank) = match_rank(&email, candidate.name.as_deref(), &needle) else {
                continue;
            };
            match best.get(&email) {
                Some((existing_rank, existing))
                    if (*existing_rank, std::cmp::Reverse(existing.signal))
                        <= (rank, std::cmp::Reverse(candidate.signal)) => {}
                _ => {
                    best.insert(email, (rank, candidate));
                }
            }
        }

        let mut ranked: Vec<(MatchRank, Candidate, String)> = best
            .into_iter()
            .map(|(email, (rank, candidate))| (rank, candidate, email))
            .collect();
        ranked.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(b.1.signal.cmp(&a.1.signal))
                .then(b.1.last_seen.cmp(&a.1.last_seen))
                .then(a.2.cmp(&b.2))
        });

        Ok(ranked
            .into_iter()
            .take(limit)
            .map(|(_, candidate, email)| AddressSuggestion {
                email,
                name: candidate
                    .name
                    .map(|name| name.trim().to_string())
                    .filter(|name| !name.is_empty()),
            })
            .collect())
    }

    /// `EXPLAIN QUERY PLAN` for the suggestion read, as one line.
    ///
    /// Exposed so the unit tests and the production-scale probe both assert
    /// against the query that actually runs rather than a copy of it: the
    /// design rests on typing never reaching the thread cache, and a plan
    /// assertion against a stale copy of the SQL would prove nothing.
    pub fn suggestion_query_plan(&self, account_id: &str, query: &str) -> Result<String> {
        register_match_rank(self.conn())?;
        let needle = query.trim().to_lowercase();
        let sql = format!("EXPLAIN QUERY PLAN {SUGGESTION_SQL}");
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(
            params![
                account_id,
                needle,
                format!("%{}%", escape_like(&needle)),
                SUGGESTION_SCAN_CAP as i64
            ],
            |row| row.get::<_, String>(3),
        )?;
        let mut steps = Vec::new();
        for row in rows {
            steps.push(row?);
        }
        Ok(steps.join(" | "))
    }

    /// Mark an account's derived address history as no longer trustworthy, so
    /// the next reconcile drops the derived counts and re-folds from the first
    /// thread message.
    ///
    /// Called by the two paths that change `thread_messages` rows at or below
    /// the watermark: an in-place header rewrite, and the folder wipe a
    /// UIDVALIDITY change forces. An account that has never been reconciled has
    /// no state row and needs no flag — its first pass is already a rebuild.
    pub fn invalidate_address_history(&self, account_id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE address_history_state SET dirty = 1 WHERE account_id = ?1",
            params![account_id],
        )?;
        Ok(())
    }

    /// [`Database::invalidate_address_history`] for the account that owns a
    /// thread. A thread with no row (it was just deleted) invalidates nothing.
    pub(crate) fn invalidate_address_history_for_thread(&self, thread_id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE address_history_state SET dirty = 1
             WHERE account_id IN (SELECT account_id FROM threads WHERE thread_id = ?1)",
            params![thread_id],
        )?;
        Ok(())
    }

    /// The account's own normalized address, used to keep self off the
    /// suggestion list.
    fn account_own_address(&self, account_id: &str) -> Result<Option<String>> {
        let username: Option<String> = self
            .conn()
            .query_row(
                "SELECT username FROM accounts WHERE id = ?1",
                params![account_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(username.as_deref().and_then(normalize_email))
    }
}

/// Read the reconciliation boundary for an account: `(source_version,
/// watermark, dirty)`. A missing row means the account has never been
/// reconciled, which reads the same as a rebuild.
fn load_state(tx: &Transaction<'_>, account_id: &str) -> Result<(i64, i64, bool)> {
    let state = tx
        .query_row(
            "SELECT source_version, last_thread_message_id, dirty
             FROM address_history_state
             WHERE account_id = ?1",
            params![account_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)? != 0,
                ))
            },
        )
        .optional()?;
    Ok(state.unwrap_or((-1, 0, false)))
}

/// Register [`match_rank`] as a SQL function, so the suggestion query can
/// order by textual match strength before [`SUGGESTION_SCAN_CAP`] applies —
/// against the same ranking Rust then sorts by, rather than a SQL restatement
/// of it that could drift.
///
/// Registration replaces any previous entry and costs one
/// `sqlite3_create_function` call per query.
fn register_match_rank(conn: &Connection) -> Result<()> {
    conn.create_scalar_function(
        MATCH_RANK_FN,
        3,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |ctx| {
            let email: Option<String> = ctx.get(0)?;
            let name: Option<String> = ctx.get(1)?;
            let needle: String = ctx.get(2)?;
            let rank = email
                .as_deref()
                .and_then(normalize_email)
                .and_then(|email| match_rank(&email, name.as_deref(), &needle));
            Ok(rank.map_or(NO_MATCH_RANK, |rank| rank as i64))
        },
    )?;
    Ok(())
}

/// Fold the account's locally recorded sent drafts into the observation map.
///
/// Shared by the reconcile and by [`Database::record_sent_draft_recipients`],
/// so the figure the send edge writes is by construction the figure the next
/// reconcile will recount — one window, one parse, one definition of which
/// drafts count.
fn record_sent_drafts(
    tx: &Transaction<'_>,
    account_id: &str,
    own_address: Option<&str>,
    seen: &mut HashMap<String, Observation>,
) -> Result<()> {
    let mut stmt = tx.prepare(
        "SELECT to_addr, cc_addr, bcc_addr, COALESCE(sent_at, updated_at)
         FROM drafts
         WHERE account_id = ?1 AND status = 'sent'
         ORDER BY COALESCE(sent_at, updated_at) DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![account_id, SENT_DRAFT_WINDOW as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    for row in rows {
        let (to_addr, cc_addr, bcc_addr, at) = row?;
        let at = at.as_deref().and_then(normalize_timestamp);
        for raw in [
            Some(to_addr.as_str()),
            cc_addr.as_deref(),
            bcc_addr.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            record(raw, &at, own_address, Source::SentDraft, seen);
        }
    }
    Ok(())
}

/// Fold one header address list into the observation map.
fn record(
    raw: &str,
    at: &Option<String>,
    own_address: Option<&str>,
    source: Source,
    seen: &mut HashMap<String, Observation>,
) {
    for parsed in parse_address_list(raw) {
        if own_address == Some(parsed.email.as_str()) {
            // Your own address is on the To line of nearly every inbound
            // message; suggesting it first for every query is noise, not
            // history.
            continue;
        }
        seen.entry(parsed.email.clone())
            .or_default()
            .record(source, parsed.name, at.clone());
    }
}

/// Write one accumulated observation back to `contacts`. Returns true when a
/// new contact row was created.
fn merge_observation(
    tx: &Transaction<'_>,
    account_id: &str,
    email: &str,
    observed: &Observation,
    now: &str,
) -> Result<bool> {
    // Match on `lower(email)` rather than the UNIQUE(account_id, email) key:
    // that constraint is case-sensitive, so a CLI-added `Alice@Example.com`
    // would otherwise gain a lowercase twin.
    let existing: Option<ExistingContact> = tx
        .query_row(
            "SELECT id, name, history_count, history_sent_count, first_seen, last_seen
             FROM contacts
             WHERE account_id = ?1 AND lower(email) = ?2
             ORDER BY history_count DESC, message_count DESC
             LIMIT 1",
            params![account_id, email],
            |row| {
                Ok(ExistingContact {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    history_count: row.get(2)?,
                    history_sent_count: row.get(3)?,
                    first_seen: row.get(4)?,
                    last_seen: row.get(5)?,
                })
            },
        )
        .optional()?;

    match existing {
        Some(current) => {
            // A curated name (set through `envelope contacts add`) outranks
            // whatever a header happened to carry.
            let name = current
                .name
                .filter(|name| !name.trim().is_empty())
                .or_else(|| observed.name.clone());
            // Derived only. `message_count` belongs to `envelope contacts
            // add|import`; copying the derived count into it would make a
            // later rebuild unable to lower the signal, because the stale
            // derived value would survive in the manual column.
            let history_count = (current.history_count + observed.delta).max(observed.recount);
            // A floor, like the recounted half above, and reset only by a
            // rebuild. The send edge merges an observation whose `delta` and
            // `recount` are zero, so it moves this column and leaves
            // `history_count` exactly where it found it.
            let history_sent_count = current.history_sent_count.max(observed.sent);
            let first_seen = min_opt(current.first_seen, observed.first_seen.clone());
            let last_seen = max_opt(current.last_seen, observed.last_seen.clone());

            // `history_derived` is deliberately absent from the SET list, in
            // both directions: history observing an address someone added by
            // hand does not make the row this module's to delete, and a row
            // this module invented stays its own until something curates it.
            tx.execute(
                "UPDATE contacts
                 SET name = ?2, history_count = ?3, history_sent_count = ?4,
                     first_seen = ?5, last_seen = ?6, updated_at = ?7
                 WHERE id = ?1",
                params![
                    current.id,
                    name,
                    history_count,
                    history_sent_count,
                    first_seen,
                    last_seen,
                    now
                ],
            )?;
            Ok(false)
        }
        None => {
            // A contact this module invented has no manual count: nothing has
            // imported or edited it. `message_count` stays 0 until something
            // that owns it says otherwise, and suggestions rank on the MAX.
            // `history_derived` marks it as this module's row, so a later
            // rebuild may take it back out when its last source disappears.
            let history_count = observed.delta.max(observed.recount);
            tx.execute(
                "INSERT INTO contacts
                    (id, account_id, email, name, tags, notes, message_count,
                     history_count, history_sent_count, history_derived,
                     first_seen, last_seen, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, '[]', NULL, 0, ?5, ?6, 1, ?7, ?8, ?9, ?9)",
                params![
                    uuid::Uuid::new_v4().to_string(),
                    account_id,
                    email,
                    observed.name,
                    history_count,
                    observed.sent,
                    observed.first_seen,
                    observed.last_seen,
                    now,
                ],
            )?;
            Ok(true)
        }
    }
}

/// Split an RFC5322 address list into its entries, keeping quoted display
/// names and bracketed addresses intact, then normalize each one.
///
/// Entries that are not usable addresses are dropped: autocomplete must never
/// offer something the compose surfaces would refuse to send.
pub fn parse_address_list(raw: &str) -> Vec<ParsedAddress> {
    split_address_list(raw)
        .iter()
        .filter_map(|entry| parse_address(entry))
        .collect()
}

/// Parse one `Name <addr@host>` entry. `None` when the address is unusable.
pub fn parse_address(entry: &str) -> Option<ParsedAddress> {
    let entry = entry.trim();
    if entry.is_empty() {
        return None;
    }

    let (name, address) = match (entry.find('<'), entry.rfind('>')) {
        (Some(start), Some(end)) if start < end => {
            (Some(entry[..start].to_string()), &entry[start + 1..end])
        }
        _ => (None, entry),
    };

    let email = normalize_email(address)?;
    let name = name
        .map(|name| unquote_display_name(&name))
        .filter(|name| !name.is_empty());

    Some(ParsedAddress { email, name })
}

/// Lowercase and validate a bare address.
///
/// Validation is the send edge's own: `EmailAddress::is_valid_local_part` and
/// `EmailAddress::is_valid_domain` are the checks `lettre::Address` runs, and
/// so does this. Rolling a looser shape here would let the endpoint suggest an
/// address the composer accepts and SMTP then rejects.
///
/// Four deliberate narrowings on top of that, all matching `isValidEmail` in
/// `crates/dashboard/web/src/lib/addresses.ts`:
///
///   * Whitespace is rejected outright — including the Unicode spaces
///     `is_valid_local_part` admits, which are invisible in a chip.
///   * The domain must carry at least two labels, so `root@localhost` is never
///     suggested.
///   * A quoted local part is rejected. This is the one that is easy to get
///     backwards: `lettre::Address::from_str` accepts `"john..doe"@example.com`,
///     but no recipient header is parsed that way. To, Cc, and Bcc all go
///     through `lettre::Mailboxes`, which refuses a quoted local part with
///     `InvalidUser`, so suggesting one would put an address in the dropdown
///     that cannot be sent to at all. See
///     `parse_mailboxes_rejects_what_the_suggestion_and_composer_gates_also_reject`
///     in `crates/email/src/smtp.rs`, which pins what the send edge takes.
///   * Domain labels must be ASCII letter-digit-hyphen. `is_valid_domain`
///     builds a label out of `atext`, so it admits both `exämple.com` and
///     `ex!ample.com`. The first parses at the send edge but reaches the wire
///     unrewritten, and `Connection::send` then refuses the envelope unless the
///     server advertises SMTPUTF8; the second no nameserver resolves. Punycode
///     is the spelling the composer and the send edge both take, and requiring
///     it costs the recipient nothing. A Unicode LOCAL part has no equivalent
///     spelling and stays admitted here and there.
pub fn normalize_email(raw: &str) -> Option<String> {
    let candidate = raw
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    if candidate.is_empty()
        || candidate
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }

    let (local, domain) = candidate.split_once('@')?;
    if domain.contains('@') {
        return None;
    }
    // Leaves exactly the dot-atom form, which is all `Mailboxes` will parse.
    if local.starts_with('"') {
        return None;
    }
    // At least two labels, matching the composer's own rule.
    let (host, tld) = domain.rsplit_once('.')?;
    if host.is_empty() || tld.is_empty() {
        return None;
    }
    if !EmailAddress::is_valid_local_part(local) || !EmailAddress::is_valid_domain(domain) {
        return None;
    }
    // `is_valid_domain` already pinned the label boundaries and the 63/254-byte
    // limits; this is the alphabet it leaves too wide.
    if !domain
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    {
        return None;
    }

    Some(candidate.to_lowercase())
}

/// Split on the separators that actually separate entries.
///
/// The one thing this must never do is a bare `.split(',')`: a comma inside a
/// quoted display name (`"Doe, Jane" <jane@example.test>`) or inside an
/// angle-bracketed address is part of the entry, not a separator. Quoted-pair
/// escapes are honoured so a display name may contain a literal quote, and an
/// RFC 5322 group label (`Team: a@x.test, b@x.test;`) is dropped rather than
/// glued onto the first address.
fn split_address_list(raw: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut in_angles = false;
    let mut escaped = false;

    for c in raw.chars() {
        if escaped {
            current.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_quotes => {
                current.push(c);
                escaped = true;
            }
            '"' => {
                in_quotes = !in_quotes;
                current.push(c);
            }
            '<' if !in_quotes => {
                in_angles = true;
                current.push(c);
            }
            '>' if !in_quotes => {
                in_angles = false;
                current.push(c);
            }
            ':' if !in_quotes && !in_angles => {
                // Everything before the colon was a group label.
                current.clear();
            }
            ',' | ';' if !in_quotes && !in_angles => {
                entries.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    entries.push(current);

    entries
        .into_iter()
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect()
}

/// Strip the quotes around a display name and undo its quoted-pair escapes,
/// keeping the punctuation the quotes were protecting.
fn unquote_display_name(raw: &str) -> String {
    let trimmed = raw.trim();
    let inner = match (trimmed.strip_prefix('"'), trimmed.strip_suffix('"')) {
        (Some(_), Some(_)) if trimmed.len() >= 2 => &trimmed[1..trimmed.len() - 1],
        _ => return trimmed.trim_matches('"').trim().to_string(),
    };

    let mut out = String::with_capacity(inner.len());
    let mut escaped = false;
    for c in inner.chars() {
        match c {
            '\\' if !escaped => escaped = true,
            _ => {
                out.push(c);
                escaped = false;
            }
        }
    }
    out.trim().to_string()
}

/// Normalize a header date to the `%Y-%m-%dT%H:%M:%S` UTC form the contacts
/// table already uses, so `first_seen`/`last_seen` stay lexicographically
/// comparable across sources.
pub(crate) fn normalize_timestamp(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc2822(raw) {
        return Some(
            parsed
                .with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string(),
        );
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(
            parsed
                .with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string(),
        );
    }
    chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|parsed| parsed.format("%Y-%m-%dT%H:%M:%S").to_string())
}

/// Rank how strongly a candidate matches the typed query, or `None` when it
/// does not match at all. A blank query matches everything at the weakest tier
/// so ordering falls through to the interaction signal.
fn match_rank(email: &str, name: Option<&str>, needle: &str) -> Option<MatchRank> {
    if needle.is_empty() {
        return Some(MatchRank::Substring);
    }

    let name = name.map(str::to_lowercase);
    let name = name.as_deref().unwrap_or_default();

    if email == needle || name == needle {
        return Some(MatchRank::Exact);
    }
    if email.starts_with(needle) || name.starts_with(needle) {
        return Some(MatchRank::Prefix);
    }
    // Word-start matches read as prefixes to a human: typing "smith" should
    // reach "Ada Smith", and typing "acme" should reach "…@acme.test".
    if name
        .split(|c: char| c.is_whitespace() || c == '.' || c == '-' || c == '_')
        .any(|word| !word.is_empty() && word.starts_with(needle))
    {
        return Some(MatchRank::Prefix);
    }
    if email
        .split_once('@')
        .is_some_and(|(_, domain)| domain.starts_with(needle))
    {
        return Some(MatchRank::Prefix);
    }
    if email.contains(needle) || name.contains(needle) {
        return Some(MatchRank::Substring);
    }
    None
}

/// Escape LIKE wildcards so a typed `%` or `_` filters literally.
fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn min_opt(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if a <= b { a } else { b }),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

fn max_opt(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if a >= b { a } else { b }),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Contact, IndexedMessageInput};

    fn db_with_accounts() -> Database {
        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acct-a', 'Work', 'me@example.test', 'example.test',
                         'smtp.example.test', 587, 'imap.example.test', 993, 'x'),
                        ('acct-b', 'Other', 'other@example.test', 'example.test',
                         'smtp.example.test', 587, 'imap.example.test', 993, 'x')",
                [],
            )
            .unwrap();
        db
    }

    fn summary(uid: u32, from_addr: &str, to_addr: &str, date: &str) -> IndexedMessageInput {
        IndexedMessageInput {
            uid,
            message_id: Some(format!("<m{uid}@example.test>")),
            from_addr: from_addr.to_string(),
            to_addr: to_addr.to_string(),
            subject: "Quarterly filing".to_string(),
            date: Some(date.to_string()),
            flags: Vec::new(),
            size: 100,
            snippet: Some("body text that must never reach a suggestion".to_string()),
            thread_id: None,
        }
    }

    /// Append one message to the deep thread cache the way an IMAP scan does.
    #[allow(clippy::too_many_arguments)]
    fn thread_message(
        db: &Database,
        account: &str,
        uid: u32,
        message_id: &str,
        from_address: &str,
        to_addresses: &str,
        date: &str,
        is_outbound: bool,
    ) {
        thread_message_with_copies(
            db,
            account,
            uid,
            message_id,
            from_address,
            to_addresses,
            None,
            None,
            date,
            is_outbound,
        );
    }

    /// The same, with the Cc/Bcc a scan retains when the headers are present.
    #[allow(clippy::too_many_arguments)]
    fn thread_message_with_copies(
        db: &Database,
        account: &str,
        uid: u32,
        message_id: &str,
        from_address: &str,
        to_addresses: &str,
        cc_addresses: Option<&str>,
        bcc_addresses: Option<&str>,
        date: &str,
        is_outbound: bool,
    ) {
        let folder = if is_outbound { "Sent" } else { "INBOX" };
        let thread = db
            .create_thread(&format!("subject-{uid}"), date, date, account)
            .unwrap();
        db.upsert_thread_message(
            &thread.thread_id,
            uid,
            Some(message_id),
            None,
            None,
            folder,
            from_address,
            to_addresses,
            cc_addresses,
            bcc_addresses,
            date,
            "Subject",
            is_outbound,
            Some("snippet that must never reach a suggestion"),
        )
        .unwrap();
    }

    fn emails(rows: &[AddressSuggestion]) -> Vec<&str> {
        rows.iter().map(|row| row.email.as_str()).collect()
    }

    fn emails_of(contacts: &[Contact]) -> Vec<&str> {
        contacts.iter().map(|c| c.email.as_str()).collect()
    }

    fn sorted_emails(db: &Database, account: &str) -> Vec<String> {
        let mut found: Vec<String> = db
            .list_contacts(account, None)
            .unwrap()
            .iter()
            .map(|c| c.email.clone())
            .collect();
        found.sort();
        found
    }

    fn contact(db: &Database, account: &str, email: &str) -> Contact {
        db.list_contacts(account, None)
            .unwrap()
            .into_iter()
            .find(|c| c.email == email)
            .unwrap_or_else(|| panic!("no contact {email} for {account}"))
    }

    fn watermark(db: &Database, account: &str) -> i64 {
        db.conn()
            .query_row(
                "SELECT last_thread_message_id FROM address_history_state WHERE account_id = ?1",
                params![account],
                |row| row.get(0),
            )
            .unwrap_or(0)
    }

    /// The derived interaction count, which is the only counter a reconcile
    /// writes. `Contact` carries `message_count` (the manual/imported one), so
    /// this reads the derived column directly.
    fn history_count(db: &Database, account: &str, email: &str) -> i64 {
        db.conn()
            .query_row(
                "SELECT history_count FROM contacts
                 WHERE account_id = ?1 AND lower(email) = ?2",
                params![account, email],
                |row| row.get(0),
            )
            .unwrap_or_else(|e| panic!("no contact {email} for {account}: {e}"))
    }

    /// The sent-draft half of the derived signal, which the send edge writes
    /// and the reconcile recounts. Kept apart from `history_count` so the two
    /// can account for the same message without stacking.
    fn sent_count(db: &Database, account: &str, email: &str) -> i64 {
        db.conn()
            .query_row(
                "SELECT history_sent_count FROM contacts
                 WHERE account_id = ?1 AND lower(email) = ?2",
                params![account, email],
                |row| row.get(0),
            )
            .unwrap_or_else(|e| panic!("no contact {email} for {account}: {e}"))
    }

    /// The signal a suggestion actually ranks on.
    fn signal(db: &Database, account: &str, email: &str) -> i64 {
        db.conn()
            .query_row(
                "SELECT MAX(message_count, history_count, history_sent_count) FROM contacts
                 WHERE account_id = ?1 AND lower(email) = ?2",
                params![account, email],
                |row| row.get(0),
            )
            .unwrap_or_else(|e| panic!("no contact {email} for {account}: {e}"))
    }

    /// Take one draft all the way through a real send: create, claim, and
    /// leave the claim through the owner-token transition every send path
    /// takes after SMTP acceptance. Nothing here opens a socket.
    fn send_draft(
        db: &Database,
        account: &str,
        to: &str,
        cc: Option<&str>,
        bcc: Option<&str>,
    ) -> String {
        let draft = db
            .create_draft(
                account,
                to,
                Some("Subject"),
                Some("body"),
                None,
                None,
                cc,
                bcc,
                Some("human:dashboard"),
            )
            .unwrap();
        let lease = db
            .claim_draft_for_immediate_send(&draft.id, draft.revision)
            .unwrap()
            .expect("claim");
        db.mark_draft_sent(&draft.id, &lease, Some("<sent@example.test>"))
            .unwrap();
        draft.id
    }

    /// Whether the row is still owned by the derivation. A manual row — one
    /// `envelope contacts` created, or took over by editing — reads false and
    /// is never swept by a rebuild.
    fn is_derived(db: &Database, account: &str, email: &str) -> bool {
        db.conn()
            .query_row(
                "SELECT history_derived FROM contacts
                 WHERE account_id = ?1 AND lower(email) = ?2",
                params![account, email],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or_else(|e| panic!("no contact {email} for {account}: {e}"))
            != 0
    }

    fn is_dirty(db: &Database, account: &str) -> bool {
        db.conn()
            .query_row(
                "SELECT dirty FROM address_history_state WHERE account_id = ?1",
                params![account],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            != 0
    }

    // ── Parsing / normalization ───────────────────────────────────────

    #[test]
    fn normalize_email_lowercases_and_strips_brackets() {
        assert_eq!(
            normalize_email("<Alice@Example.COM>").as_deref(),
            Some("alice@example.com")
        );
        assert_eq!(
            normalize_email("  Bob@Example.test  ").as_deref(),
            Some("bob@example.test")
        );
    }

    #[test]
    fn normalize_email_rejects_malformed_addresses() {
        for malformed in [
            "",
            "   ",
            "not-an-email",
            "@example.com",
            "alice@",
            "alice@localhost",
            "alice@@example.com",
            "alice@example.",
            "alice@.com",
            "ali ce@example.com",
            "alice\n@example.com",
            // Rejected by the send edge's own validator, and previously let
            // through: a dot-atom local part has no empty atoms, and a domain
            // label starts and ends alphanumeric.
            "ali..ce@example.com",
            ".alice@example.com",
            "alice.@example.com",
            "alice@exa..mple.com",
            "alice@-example.com",
            "alice@example-.com",
            "alice@example..com",
            "alice@.example.com",
        ] {
            assert!(
                normalize_email(malformed).is_none(),
                "expected {malformed:?} to be rejected"
            );
        }
    }

    /// A quoted local part passes `EmailAddress::is_valid_local_part`, so
    /// deferring to that alone let the address book learn — and then suggest —
    /// an address no recipient header can carry: To/Cc/Bcc are parsed by
    /// `lettre::Mailboxes`, which refuses it with `InvalidUser`. Clicking such
    /// a suggestion produced a chip the composer marked invalid, and pasting
    /// one past the composer would have failed at SMTP. The send edge's half of
    /// this pair is
    /// `parse_mailboxes_rejects_what_the_suggestion_and_composer_gates_also_reject`.
    #[test]
    fn normalize_email_rejects_quoted_local_parts_the_send_edge_cannot_carry() {
        for quoted in [
            "\"john..doe\"@example.com",
            "\"john doe\"@example.com",
            "\"johndoe\"@example.com",
            "\"\"@example.com",
            "\"unterminated@example.com",
            "\"a\\\\\"@example.com",
        ] {
            assert!(
                normalize_email(quoted).is_none(),
                "expected {quoted:?} to be rejected"
            );
            // The half that makes it a bug rather than a preference: the
            // permissive validator says yes to some of these.
            let _ = EmailAddress::is_valid_local_part(quoted.rsplit_once('@').unwrap().0);
        }

        // A header carrying one must not create a contact for it either.
        assert_eq!(
            parse_address_list("\"john..doe\"@example.com, ada@example.test")
                .into_iter()
                .map(|p| p.email)
                .collect::<Vec<_>>(),
            vec!["ada@example.test".to_string()],
            "the unsendable entry is dropped, the rest of the list survives"
        );
    }

    /// Unicode whitespace is admitted by `atext` and invisible in a chip, and
    /// the send edge refuses it. All three layers reject it by name.
    #[test]
    fn normalize_email_rejects_invisible_whitespace() {
        for invisible in ["a\u{a0}b@example.com", "a\u{3000}b@example.com"] {
            assert!(
                normalize_email(invisible).is_none(),
                "expected {invisible:?} to be rejected"
            );
        }
        assert_eq!(
            normalize_email("jos\u{e9}@example.com").as_deref(),
            Some("josé@example.com"),
            "accented addresses are ordinary and stay suggestible"
        );
    }

    /// The dropdown must never offer an address the composer will refuse, and
    /// the composer requires an ASCII domain. `Address::check_domain` parses a
    /// Unicode domain but nothing rewrites it, so the address reaches the wire
    /// in its Unicode spelling and `Connection::send` refuses the envelope
    /// unless the server advertises SMTPUTF8; punycode is the spelling both
    /// layers accept, and it costs the recipient nothing.
    ///
    /// `EmailAddress::is_valid_domain` is looser still — its domain labels are
    /// `atext`, so it admits `ex!ample.com`, which no nameserver will resolve.
    /// Deferring to it alone let the address book learn addresses the composer
    /// then marked invalid on the chip.
    #[test]
    fn normalize_email_requires_an_ascii_domain() {
        for unsuggestable in [
            "ada@exämple.com",
            "ada@例え.com",
            "ada@example.中国",
            "ada@ex!ample.com",
            "ada@ex_ample.com",
        ] {
            assert!(
                normalize_email(unsuggestable).is_none(),
                "expected {unsuggestable:?} to be rejected"
            );
        }

        assert_eq!(
            normalize_email("Ada@XN--exmple-cua.com").as_deref(),
            Some("ada@xn--exmple-cua.com"),
            "the punycode spelling is what both layers accept"
        );
        // A Unicode LOCAL part is a different question: the send edge admits it
        // and there is no ASCII spelling to prefer, so it stays suggestible.
        assert_eq!(
            normalize_email("José@example.com").as_deref(),
            Some("josé@example.com")
        );
    }

    /// Tightening validation must not cost the ordinary addresses this feature
    /// exists to suggest.
    #[test]
    fn normalize_email_keeps_the_addresses_people_actually_use() {
        for good in [
            "ada@example.com",
            "ada.lovelace@example.co.uk",
            "me+filing@example.test",
            "a_b@example.test",
            "clerk-2@court.test",
            "1099@irs.test",
            "<Ada@Example.COM>",
        ] {
            assert!(
                normalize_email(good).is_some(),
                "expected {good:?} to be accepted"
            );
        }
        assert_eq!(
            parse_address("Ada Lovelace <Ada@Example.test>"),
            Some(ParsedAddress {
                email: "ada@example.test".into(),
                name: Some("Ada Lovelace".into()),
            })
        );
    }

    /// The store validates with the same primitives `lettre::Address` uses at
    /// the SMTP edge, so an address the address book offers is one the send
    /// path will parse. (The store is deliberately stricter in two places —
    /// no whitespace, and a domain of at least two labels — which only ever
    /// narrows what is suggested.)
    #[test]
    fn normalize_email_agrees_with_the_send_edge_validator() {
        for candidate in [
            "ada@example.com",
            "ali..ce@example.com",
            "alice@-example.com",
            "alice@example-.com",
            ".alice@example.com",
            "me+filing@example.test",
            "a_b@example.test",
        ] {
            let (local, domain) = candidate.split_once('@').unwrap();
            let send_edge_accepts =
                EmailAddress::is_valid_local_part(local) && EmailAddress::is_valid_domain(domain);
            if normalize_email(candidate).is_some() {
                assert!(
                    send_edge_accepts,
                    "{candidate:?} would be suggested but rejected at send time"
                );
            }
        }
    }

    #[test]
    fn parse_address_list_keeps_display_names_and_quoted_commas() {
        let parsed = parse_address_list(
            "Ada Lovelace <Ada@Example.test>, \"Doe, Jane\" <jane@example.test>, bare@example.test",
        );
        assert_eq!(
            parsed,
            vec![
                ParsedAddress {
                    email: "ada@example.test".into(),
                    name: Some("Ada Lovelace".into()),
                },
                ParsedAddress {
                    email: "jane@example.test".into(),
                    name: Some("Doe, Jane".into()),
                },
                ParsedAddress {
                    email: "bare@example.test".into(),
                    name: None,
                },
            ]
        );
    }

    /// A display name may carry an escaped quote, and a semicolon inside one is
    /// still part of the name. Splitting either apart would invent recipients.
    #[test]
    fn parse_address_list_honours_quoted_pair_escapes() {
        let parsed = parse_address_list(
            "\"Doe, \\\"JD\\\", John; Esq.\" <jd@example.test>, second@example.test",
        );
        assert_eq!(
            parsed,
            vec![
                ParsedAddress {
                    email: "jd@example.test".into(),
                    name: Some("Doe, \"JD\", John; Esq.".into()),
                },
                ParsedAddress {
                    email: "second@example.test".into(),
                    name: None,
                },
            ]
        );
    }

    /// A display name with an odd number of literal quotes is emitted with a
    /// quoted-pair escape. A splitter that toggled quote state on the escaped
    /// quote would stay inside quotes at the next comma and glue the following
    /// recipient onto this one. Matches `parseAddrs`/`formatAddr` in
    /// `crates/dashboard/web/src/lib/addresses.ts`.
    #[test]
    fn parse_address_list_survives_an_odd_literal_quote() {
        let parsed = parse_address_list("\"5\\\" Bolt\" <bolt@vendor.test>, second@example.test");
        assert_eq!(
            parsed,
            vec![
                ParsedAddress {
                    email: "bolt@vendor.test".into(),
                    name: Some("5\" Bolt".into()),
                },
                ParsedAddress {
                    email: "second@example.test".into(),
                    name: None,
                },
            ]
        );
    }

    /// RFC 5322 group syntax: the label is not an address, and the members are.
    #[test]
    fn parse_address_list_drops_group_labels_and_keeps_members() {
        let parsed = parse_address_list("Filing Team: clerk@court.test, judge@court.test;");
        assert_eq!(
            parsed.iter().map(|a| a.email.as_str()).collect::<Vec<_>>(),
            vec!["clerk@court.test", "judge@court.test"]
        );
    }

    #[test]
    fn parse_address_list_drops_malformed_entries_but_keeps_valid_neighbours() {
        let parsed = parse_address_list("good@example.test, garbage, , also-good@example.test");
        assert_eq!(
            parsed.iter().map(|a| a.email.as_str()).collect::<Vec<_>>(),
            vec!["good@example.test", "also-good@example.test"]
        );
    }

    // ── Reconcile: the deep thread cache ──────────────────────────────

    #[test]
    fn reconcile_records_inbound_senders_and_outbound_recipients_from_the_thread_cache() {
        let db = db_with_accounts();
        thread_message(
            &db,
            "acct-a",
            1,
            "<in-1@example.test>",
            "ada@example.test",
            "me@example.test, grace@example.test",
            "2026-05-12T12:00:00Z",
            false,
        );
        thread_message(
            &db,
            "acct-a",
            2,
            "<out-1@example.test>",
            "me@example.test",
            "bob@vendor.test, cc@vendor.test",
            "2026-05-13T12:00:00Z",
            true,
        );

        let summary = db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(summary.thread_rows, 2);
        assert_eq!(summary.created, 4);
        assert!(!summary.pending);

        assert_eq!(
            sorted_emails(&db, "acct-a"),
            vec![
                "ada@example.test",
                "bob@vendor.test",
                "cc@vendor.test",
                "grace@example.test",
            ]
        );
        assert!(
            !sorted_emails(&db, "acct-a").contains(&"me@example.test".to_string()),
            "the account's own address must not be suggested back to it"
        );
        assert_eq!(
            contact(&db, "acct-a", "ada@example.test")
                .last_seen
                .as_deref(),
            Some("2026-05-12T12:00:00")
        );
    }

    /// A Cc recipient is a correspondent exactly as a To recipient is, and the
    /// Bcc on the sender's own copy of an outbound message is the sender's
    /// record of who actually received it. All three enter the shared cache.
    #[test]
    fn reconcile_records_to_cc_and_bcc_from_the_thread_cache() {
        let db = db_with_accounts();
        thread_message_with_copies(
            &db,
            "acct-a",
            1,
            "<out-copies@example.test>",
            "me@example.test",
            "\"Doe, Jane\" <to@court.test>",
            Some("cc@court.test, second-cc@court.test"),
            Some("bcc@court.test"),
            "2026-05-12T12:00:00Z",
            true,
        );

        let pass = db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(pass.thread_rows, 1);
        assert_eq!(
            sorted_emails(&db, "acct-a"),
            vec![
                "bcc@court.test",
                "cc@court.test",
                "second-cc@court.test",
                "to@court.test",
            ]
        );
        assert_eq!(
            contact(&db, "acct-a", "to@court.test").name.as_deref(),
            Some("Doe, Jane")
        );
        // A Bcc recipient is an ordinary suggestion row — it carries no marker
        // saying it was blind-copied.
        let rows = db.suggest_addresses("acct-a", "bcc@court.test", 8).unwrap();
        assert_eq!(
            serde_json::to_string(&rows).unwrap(),
            r#"[{"email":"bcc@court.test","name":null}]"#
        );
    }

    /// Rows cached before Envelope retained Cc/Bcc carry NULL there. Nothing
    /// backfills them; they fill in when a scan revisits the message, and that
    /// rewrite is an address change like any other.
    #[test]
    fn a_scan_that_fills_in_cc_reaches_the_address_book_through_invalidation() {
        let db = db_with_accounts();
        thread_message(
            &db,
            "acct-a",
            1,
            "<pre-cc@example.test>",
            "me@example.test",
            "to@court.test",
            "2026-05-12T12:00:00Z",
            true,
        );
        db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(sorted_emails(&db, "acct-a"), vec!["to@court.test"]);

        thread_message_with_copies(
            &db,
            "acct-a",
            1,
            "<pre-cc@example.test>",
            "me@example.test",
            "to@court.test",
            Some("cc@court.test"),
            None,
            "2026-05-12T12:00:00Z",
            true,
        );
        db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(
            sorted_emails(&db, "acct-a"),
            vec!["cc@court.test", "to@court.test"]
        );
    }

    /// An outbound message's From line is the account itself — including
    /// aliases the username check cannot recognize — so it is never history.
    #[test]
    fn reconcile_ignores_the_from_line_of_outbound_thread_messages() {
        let db = db_with_accounts();
        thread_message(
            &db,
            "acct-a",
            1,
            "<out-alias@example.test>",
            "me+alias@example.test",
            "bob@vendor.test",
            "2026-05-13T12:00:00Z",
            true,
        );

        db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(sorted_emails(&db, "acct-a"), vec!["bob@vendor.test"]);
    }

    #[test]
    fn reconcile_is_account_scoped_across_the_thread_cache() {
        let db = db_with_accounts();
        thread_message(
            &db,
            "acct-a",
            1,
            "<a@example.test>",
            "ada@example.test",
            "me@example.test",
            "2026-05-12T12:00:00Z",
            false,
        );
        thread_message(
            &db,
            "acct-b",
            1,
            "<b@example.test>",
            "elsewhere@example.test",
            "other@example.test",
            "2026-05-12T12:00:00Z",
            false,
        );

        db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(sorted_emails(&db, "acct-a"), vec!["ada@example.test"]);
        assert!(
            db.list_contacts("acct-b", None).unwrap().is_empty(),
            "reconciling one account must not write another account's history"
        );

        db.reconcile_address_history("acct-b").unwrap();
        assert_eq!(
            emails_of(&db.list_contacts("acct-b", None).unwrap()),
            vec!["elsewhere@example.test"]
        );
        assert_eq!(sorted_emails(&db, "acct-a"), vec!["ada@example.test"]);
    }

    /// The watermark is the whole point: a second reconcile over unchanged
    /// data must read no thread rows and change no counts.
    #[test]
    fn reconcile_is_idempotent_and_advances_the_watermark() {
        let db = db_with_accounts();
        for uid in 1..=3u32 {
            thread_message(
                &db,
                "acct-a",
                uid,
                &format!("<m{uid}@example.test>"),
                "ada@example.test",
                "me@example.test",
                "2026-05-12T12:00:00Z",
                false,
            );
        }

        let first = db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(first.thread_rows, 3);
        assert!(first.rebuilt, "the first pass folds in from the start");
        assert_eq!(history_count(&db, "acct-a", "ada@example.test"), 3);
        let mark = watermark(&db, "acct-a");
        assert!(mark > 0);

        let second = db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(second.thread_rows, 0, "no thread row may be read twice");
        assert!(!second.rebuilt);
        assert_eq!(
            history_count(&db, "acct-a", "ada@example.test"),
            3,
            "a repeat reconcile must not inflate the count"
        );
        assert_eq!(watermark(&db, "acct-a"), mark);
    }

    /// Re-indexing an already-cached message updates the row in place; the
    /// interaction it represents has already been counted.
    #[test]
    fn re_indexing_the_same_message_does_not_double_count() {
        let db = db_with_accounts();
        let thread = db
            .create_thread(
                "filing",
                "2026-05-12T12:00:00Z",
                "2026-05-12T12:00:00Z",
                "acct-a",
            )
            .unwrap();
        let upsert = |snippet: &str| {
            db.upsert_thread_message(
                &thread.thread_id,
                7,
                Some("<dup@example.test>"),
                None,
                None,
                "INBOX",
                "ada@example.test",
                "me@example.test",
                None,
                None,
                "2026-05-12T12:00:00Z",
                "Filing",
                false,
                Some(snippet),
            )
            .unwrap()
        };

        let first_row = upsert("v1");
        db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(history_count(&db, "acct-a", "ada@example.test"), 1);

        let second_row = upsert("v2");
        assert_eq!(
            first_row, second_row,
            "the scan upserts, it does not append"
        );
        assert!(
            !is_dirty(&db, "acct-a"),
            "a rescan that changes only the snippet is not an address change"
        );
        let pass = db.reconcile_address_history("acct-a").unwrap();
        assert!(
            !pass.rebuilt,
            "an unchanged rescan must not force a rebuild"
        );
        assert_eq!(pass.thread_rows, 0);
        assert_eq!(
            history_count(&db, "acct-a", "ada@example.test"),
            1,
            "a re-indexed message is the same interaction"
        );
    }

    /// `envelope thread --rebuild` rewrites cached rows in place. The
    /// watermark has already passed those ids, so a corrected recipient would
    /// be invisible forever unless the rewrite invalidates the account.
    #[test]
    fn an_in_place_address_rewrite_invalidates_and_rebuilds() {
        let db = db_with_accounts();
        let thread = db
            .create_thread(
                "filing",
                "2026-05-12T12:00:00Z",
                "2026-05-12T12:00:00Z",
                "acct-a",
            )
            .unwrap();
        let upsert = |to: &str, cc: Option<&str>| {
            db.upsert_thread_message(
                &thread.thread_id,
                7,
                Some("<rewrite@example.test>"),
                None,
                None,
                "INBOX",
                "ada@example.test",
                to,
                cc,
                None,
                "2026-05-12T12:00:00Z",
                "Filing",
                false,
                None,
            )
            .unwrap()
        };

        let row = upsert("me@example.test", None);
        db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(sorted_emails(&db, "acct-a"), vec!["ada@example.test"]);
        assert_eq!(history_count(&db, "acct-a", "ada@example.test"), 1);
        let mark = watermark(&db, "acct-a");
        assert!(mark >= row);

        // A corrected scan finds a recipient the first pass missed, and a Cc
        // the first pass could not store at all.
        upsert("me@example.test, grace@example.test", Some("cc@court.test"));
        assert!(
            is_dirty(&db, "acct-a"),
            "changing an address-bearing field must invalidate the derived history"
        );

        let pass = db.reconcile_address_history("acct-a").unwrap();
        assert!(pass.rebuilt);
        assert_eq!(pass.thread_rows, 1, "a rebuild re-reads from the start");
        assert!(
            !is_dirty(&db, "acct-a"),
            "the rebuild clears the invalidation it just honoured"
        );
        assert_eq!(
            sorted_emails(&db, "acct-a"),
            vec!["ada@example.test", "cc@court.test", "grace@example.test"],
            "the corrected recipients reach the address book"
        );
        assert_eq!(
            history_count(&db, "acct-a", "ada@example.test"),
            1,
            "the rebuild recomputes the count instead of adding to it"
        );
    }

    /// A UIDVALIDITY change deletes a folder's cached rows; the rescan
    /// re-inserts the same messages under fresh ids, every one of them above
    /// the old watermark. Without invalidation each one would be counted twice.
    #[test]
    fn a_uidvalidity_reset_does_not_double_count_the_rescan() {
        let db = db_with_accounts();
        let thread = db
            .create_thread(
                "hearing",
                "2026-05-12T12:00:00Z",
                "2026-05-12T12:00:00Z",
                "acct-a",
            )
            .unwrap();
        let insert = |uid: u32| {
            db.upsert_thread_message(
                &thread.thread_id,
                uid,
                Some(&format!("<uidv-{uid}@example.test>")),
                None,
                None,
                "INBOX",
                "ada@example.test",
                "me@example.test",
                None,
                None,
                "2026-05-12T12:00:00Z",
                "Hearing",
                false,
                None,
            )
            .unwrap()
        };
        for uid in 1..=3u32 {
            insert(uid);
        }
        db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(history_count(&db, "acct-a", "ada@example.test"), 3);

        let deleted = db.reset_folder_sync("acct-a", "INBOX", 9_999).unwrap();
        assert_eq!(deleted, 3);
        assert!(
            is_dirty(&db, "acct-a"),
            "wiping a folder's cached rows invalidates the derived history"
        );

        // The rescan re-inserts the same three messages under new UIDs — and
        // new autoincrement ids well above the stale watermark.
        let thread = db
            .create_thread(
                "hearing",
                "2026-05-12T12:00:00Z",
                "2026-05-12T12:00:00Z",
                "acct-a",
            )
            .unwrap();
        for uid in 5001..=5003u32 {
            db.upsert_thread_message(
                &thread.thread_id,
                uid,
                Some(&format!("<uidv-{}@example.test>", uid - 5000)),
                None,
                None,
                "INBOX",
                "ada@example.test",
                "me@example.test",
                None,
                None,
                "2026-05-12T12:00:00Z",
                "Hearing",
                false,
                None,
            )
            .unwrap();
        }

        let pass = db.reconcile_address_history("acct-a").unwrap();
        assert!(pass.rebuilt);
        assert_eq!(
            history_count(&db, "acct-a", "ada@example.test"),
            3,
            "the same three messages are still three interactions"
        );
    }

    /// Invalidation is per account: one mailbox's UIDVALIDITY churn must not
    /// cost another account a rebuild.
    #[test]
    fn invalidation_is_scoped_to_one_account() {
        let db = db_with_accounts();
        thread_message(
            &db,
            "acct-a",
            1,
            "<a@example.test>",
            "ada@example.test",
            "me@example.test",
            "2026-05-12T12:00:00Z",
            false,
        );
        thread_message(
            &db,
            "acct-b",
            1,
            "<b@example.test>",
            "elsewhere@example.test",
            "other@example.test",
            "2026-05-12T12:00:00Z",
            false,
        );
        db.reconcile_address_history("acct-a").unwrap();
        db.reconcile_address_history("acct-b").unwrap();

        db.reset_folder_sync("acct-a", "INBOX", 4_242).unwrap();
        assert!(is_dirty(&db, "acct-a"));
        assert!(!is_dirty(&db, "acct-b"));

        let other = db.reconcile_address_history("acct-b").unwrap();
        assert!(!other.rebuilt, "acct-b was never invalidated");
        assert_eq!(history_count(&db, "acct-b", "elsewhere@example.test"), 1);
    }

    /// New mail after a backfill costs only the new rows, and lands in the
    /// cache without a rescan.
    #[test]
    fn reconcile_folds_in_new_thread_messages_incrementally() {
        let db = db_with_accounts();
        thread_message(
            &db,
            "acct-a",
            1,
            "<first@example.test>",
            "ada@example.test",
            "me@example.test",
            "2026-05-12T12:00:00Z",
            false,
        );
        db.reconcile_address_history("acct-a").unwrap();
        let mark = watermark(&db, "acct-a");

        thread_message(
            &db,
            "acct-a",
            2,
            "<second@example.test>",
            "grace@example.test",
            "me@example.test",
            "2026-06-01T09:00:00Z",
            false,
        );
        let pass = db.reconcile_address_history("acct-a").unwrap();

        assert_eq!(pass.thread_rows, 1, "only the new row is read");
        assert_eq!(pass.created, 1);
        assert_eq!(
            sorted_emails(&db, "acct-a"),
            vec!["ada@example.test", "grace@example.test"]
        );
        assert!(watermark(&db, "acct-a") > mark);
        assert_eq!(
            history_count(&db, "acct-a", "ada@example.test"),
            1,
            "an untouched contact keeps its count"
        );
    }

    /// Chunking exists so a six-figure thread cache never blocks the caller in
    /// one call. Progress must be durable between chunks, and the result must
    /// be the same as an unchunked pass.
    #[test]
    fn a_chunked_backfill_resumes_and_totals_the_same() {
        let db = db_with_accounts();
        for uid in 1..=5u32 {
            thread_message(
                &db,
                "acct-a",
                uid,
                &format!("<m{uid}@example.test>"),
                "ada@example.test",
                "me@example.test",
                "2026-05-12T12:00:00Z",
                false,
            );
        }

        let mut passes = 0;
        loop {
            let pass = db.reconcile_address_history_chunk("acct-a", 2).unwrap();
            passes += 1;
            assert!(pass.thread_rows <= 2, "a chunk must respect its bound");
            if !pass.pending {
                break;
            }
            assert!(watermark(&db, "acct-a") > 0, "progress must be durable");
        }

        assert!(passes >= 3, "5 rows in chunks of 2 takes several passes");
        assert_eq!(history_count(&db, "acct-a", "ada@example.test"), 5);

        // And the caught-up cache stays caught up.
        let after = db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(after.thread_rows, 0);
        assert_eq!(history_count(&db, "acct-a", "ada@example.test"), 5);
    }

    /// A source-version bump is how a parser fix reaches history that was
    /// already folded in: the derived count is dropped and rebuilt, while the
    /// curated columns survive untouched.
    #[test]
    fn a_source_version_bump_rebuilds_without_double_counting() {
        let db = db_with_accounts();
        for uid in 1..=4u32 {
            thread_message(
                &db,
                "acct-a",
                uid,
                &format!("<m{uid}@example.test>"),
                "ada@example.test",
                "me@example.test",
                "2026-05-12T12:00:00Z",
                false,
            );
        }
        db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(history_count(&db, "acct-a", "ada@example.test"), 4);

        db.conn()
            .execute(
                "UPDATE address_history_state SET source_version = source_version - 1
                 WHERE account_id = 'acct-a'",
                [],
            )
            .unwrap();

        let rebuild = db.reconcile_address_history("acct-a").unwrap();
        assert!(rebuild.rebuilt);
        assert_eq!(rebuild.thread_rows, 4, "a rebuild re-reads from the start");
        assert_eq!(
            history_count(&db, "acct-a", "ada@example.test"),
            4,
            "a rebuild recomputes the count instead of adding to it"
        );
    }

    // ── Reconcile: the recounted sources ──────────────────────────────

    #[test]
    fn reconcile_records_prior_senders_and_prior_recipients() {
        let db = db_with_accounts();
        db.upsert_indexed_message_summaries(
            "acct-a",
            "INBOX",
            1,
            &[summary(
                1,
                "Ada Lovelace <ada@example.test>",
                "me@example.test, Grace Hopper <grace@example.test>",
                "Tue, 12 May 2026 12:00:00 +0000",
            )],
        )
        .unwrap();

        let draft = db
            .create_draft(
                "acct-a",
                "Bob Vendor <bob@vendor.test>",
                Some("Invoice"),
                Some("body"),
                None,
                None,
                Some("cc@vendor.test"),
                Some("bcc@vendor.test"),
                Some("human:dashboard"),
            )
            .unwrap();
        db.update_draft_status(&draft.id, crate::DraftStatus::Sent)
            .unwrap();

        let summary = db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(summary.observed, 5);
        assert_eq!(summary.created, 5);
        assert_eq!(summary.updated, 0);

        assert_eq!(
            sorted_emails(&db, "acct-a"),
            vec![
                "ada@example.test",
                "bcc@vendor.test",
                "bob@vendor.test",
                "cc@vendor.test",
                "grace@example.test",
            ]
        );
        assert!(
            !sorted_emails(&db, "acct-a").contains(&"me@example.test".to_string()),
            "the account's own address must not be suggested back to it"
        );

        let ada = contact(&db, "acct-a", "ada@example.test");
        assert_eq!(ada.name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(ada.last_seen.as_deref(), Some("2026-05-12T12:00:00"));
    }

    /// The dashboard rewrites its INBOX snapshot on every refresh, so the
    /// snapshot is recounted rather than watermarked — which must not turn a
    /// repeat reconcile into a second interaction.
    #[test]
    fn recounted_sources_are_idempotent_across_reconciles() {
        let db = db_with_accounts();
        db.upsert_indexed_message_summaries(
            "acct-a",
            "INBOX",
            1,
            &[
                summary(
                    1,
                    "Ada <ada@example.test>",
                    "me@example.test",
                    "Tue, 12 May 2026 12:00:00 +0000",
                ),
                summary(
                    2,
                    "Ada <ada@example.test>",
                    "me@example.test",
                    "Wed, 13 May 2026 12:00:00 +0000",
                ),
            ],
        )
        .unwrap();

        db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(history_count(&db, "acct-a", "ada@example.test"), 2);

        db.reconcile_address_history("acct-a").unwrap();
        db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(
            history_count(&db, "acct-a", "ada@example.test"),
            2,
            "recounting must not accumulate"
        );

        // The same message re-indexed under the same key is still one message.
        db.upsert_indexed_message_summaries(
            "acct-a",
            "INBOX",
            1,
            &[
                summary(
                    1,
                    "Ada <ada@example.test>",
                    "me@example.test",
                    "Tue, 12 May 2026 12:00:00 +0000",
                ),
                summary(
                    2,
                    "Ada <ada@example.test>",
                    "me@example.test",
                    "Wed, 13 May 2026 12:00:00 +0000",
                ),
            ],
        )
        .unwrap();
        db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(history_count(&db, "acct-a", "ada@example.test"), 2);
    }

    #[test]
    fn reconcile_deduplicates_case_insensitively() {
        let db = db_with_accounts();
        db.upsert_indexed_message_summaries(
            "acct-a",
            "INBOX",
            1,
            &[
                summary(
                    1,
                    "Ada <Ada@Example.test>",
                    "me@example.test",
                    "Tue, 12 May 2026 12:00:00 +0000",
                ),
                summary(
                    2,
                    "ADA@EXAMPLE.TEST",
                    "me@example.test",
                    "Wed, 13 May 2026 12:00:00 +0000",
                ),
            ],
        )
        .unwrap();

        let first = db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(first.observed, 1);
        assert_eq!(first.created, 1);

        let second = db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(second.created, 0);
        assert_eq!(second.updated, 1);

        let stored = db.list_contacts("acct-a", None).unwrap();
        assert_eq!(
            stored.len(),
            1,
            "case variants must collapse to one contact"
        );
        assert_eq!(stored[0].email, "ada@example.test");
        assert_eq!(history_count(&db, "acct-a", "ada@example.test"), 2);
        assert_eq!(
            stored[0].message_count, 0,
            "nothing has imported or edited this contact, so the manual counter stays untouched"
        );
        assert_eq!(stored[0].first_seen.as_deref(), Some("2026-05-12T12:00:00"));
        assert_eq!(stored[0].last_seen.as_deref(), Some("2026-05-13T12:00:00"));
    }

    #[test]
    fn reconcile_merges_into_a_cli_added_contact_without_lowering_its_signal() {
        let db = db_with_accounts();
        db.upsert_contact(&Contact {
            id: "curated".into(),
            account_id: "acct-a".into(),
            email: "Ada@Example.test".into(),
            name: Some("Ada, curated".into()),
            tags: r#"["vendor"]"#.into(),
            notes: Some("Net-30".into()),
            message_count: 99,
            first_seen: Some("2020-01-01T00:00:00".into()),
            last_seen: Some("2020-01-01T00:00:00".into()),
            created_at: "2020-01-01T00:00:00".into(),
            updated_at: "2020-01-01T00:00:00".into(),
        })
        .unwrap();

        thread_message(
            &db,
            "acct-a",
            1,
            "<m1@example.test>",
            "ada@example.test",
            "me@example.test",
            "2026-05-12T12:00:00Z",
            false,
        );
        db.upsert_indexed_message_summaries(
            "acct-a",
            "INBOX",
            1,
            &[summary(
                1,
                "Ada Lovelace <ada@example.test>",
                "me@example.test",
                "Tue, 12 May 2026 12:00:00 +0000",
            )],
        )
        .unwrap();
        db.reconcile_address_history("acct-a").unwrap();

        let stored = db.list_contacts("acct-a", None).unwrap();
        assert_eq!(stored.len(), 1, "no lowercase twin may be created");
        assert_eq!(stored[0].name.as_deref(), Some("Ada, curated"));
        assert_eq!(stored[0].message_count, 99, "count must never be lowered");
        assert_eq!(stored[0].first_seen.as_deref(), Some("2020-01-01T00:00:00"));
        assert_eq!(stored[0].last_seen.as_deref(), Some("2026-05-12T12:00:00"));
        assert_eq!(stored[0].tags, r#"["vendor"]"#, "curated tags survive");
        assert_eq!(stored[0].notes.as_deref(), Some("Net-30"));
        assert_eq!(
            history_count(&db, "acct-a", "ada@example.test"),
            1,
            "the derived count is kept separately from the imported one"
        );
    }

    /// `message_count` is the manual/imported counter and `history_count` the
    /// derived one. Keeping them apart is what lets a rebuild lower or reset
    /// the derived signal: copying it across would leave a stale high-water
    /// mark in a column this module is not allowed to lower.
    #[test]
    fn a_rebuild_lowers_the_derived_count_without_a_stale_copy_surviving() {
        let db = db_with_accounts();
        for uid in 1..=4u32 {
            thread_message(
                &db,
                "acct-a",
                uid,
                &format!("<m{uid}@example.test>"),
                "ada@example.test",
                "me@example.test",
                "2026-05-12T12:00:00Z",
                false,
            );
        }
        db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(history_count(&db, "acct-a", "ada@example.test"), 4);
        assert_eq!(
            contact(&db, "acct-a", "ada@example.test").message_count,
            0,
            "a contact this module invented has no manual count"
        );
        assert_eq!(
            emails(&db.suggest_addresses("acct-a", "ada", 8).unwrap()),
            vec!["ada@example.test"],
            "the derived count alone is enough to rank a contact"
        );

        // Three of the four messages leave the cache — a Sent folder pruned,
        // say — and the derivation is re-run from scratch.
        db.conn()
            .execute(
                "DELETE FROM thread_messages WHERE id > (
                     SELECT MIN(id) FROM thread_messages
                 )",
                [],
            )
            .unwrap();
        db.invalidate_address_history("acct-a").unwrap();

        let pass = db.reconcile_address_history("acct-a").unwrap();
        assert!(pass.rebuilt);
        assert_eq!(
            history_count(&db, "acct-a", "ada@example.test"),
            1,
            "a rebuild may lower the derived count"
        );
        assert_eq!(
            contact(&db, "acct-a", "ada@example.test").message_count,
            0,
            "and no stale copy of the old derived count survives in the manual column"
        );
    }

    // ── Row ownership: derived rows and manual ones ───────────────────

    /// The case this ownership split exists for. A scan corrects a header, the
    /// rebuild re-derives from the corrected rows, and the address the typo
    /// invented has no source left. It was never a contact anyone chose, so it
    /// goes — rather than sitting at zero signal in the dropdown forever.
    #[test]
    fn a_rebuild_removes_a_derived_row_whose_only_source_was_corrected_away() {
        let db = db_with_accounts();
        let thread = db
            .create_thread(
                "filing",
                "2026-05-12T12:00:00Z",
                "2026-05-12T12:00:00Z",
                "acct-a",
            )
            .unwrap();
        let upsert = |to: &str| {
            db.upsert_thread_message(
                &thread.thread_id,
                7,
                Some("<typo@example.test>"),
                None,
                None,
                "INBOX",
                "ada@example.test",
                to,
                None,
                None,
                "2026-05-12T12:00:00Z",
                "Filing",
                false,
                None,
            )
            .unwrap()
        };

        upsert("me@example.test, tpyo@court.test");
        db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(
            sorted_emails(&db, "acct-a"),
            vec!["ada@example.test", "tpyo@court.test"]
        );
        assert!(is_derived(&db, "acct-a", "tpyo@court.test"));

        upsert("me@example.test, clerk@court.test");
        let pass = db.reconcile_address_history("acct-a").unwrap();

        assert!(pass.rebuilt);
        assert_eq!(pass.removed, 1);
        assert_eq!(
            sorted_emails(&db, "acct-a"),
            vec!["ada@example.test", "clerk@court.test"],
            "the corrected-away address must not survive as a zero-signal row"
        );
        assert!(
            db.suggest_addresses("acct-a", "tpyo", 8)
                .unwrap()
                .is_empty(),
            "and it must not be suggested"
        );
        assert_eq!(
            history_count(&db, "acct-a", "ada@example.test"),
            1,
            "the correspondents the corrected header still carries are untouched"
        );
    }

    /// A contact someone added by hand is not the derivation's to delete —
    /// including the barest possible one, an address with no name, no tags, and
    /// no counts at all, which is exactly what a swept derived row looks like.
    #[test]
    fn a_manual_bare_contact_survives_a_rebuild_that_never_observes_it() {
        let db = db_with_accounts();
        seed_contact(
            &db,
            "acct-a",
            "filed@court.test",
            None,
            0,
            "2026-01-01T00:00:00",
        );
        thread_message(
            &db,
            "acct-a",
            1,
            "<m1@example.test>",
            "ada@example.test",
            "me@example.test",
            "2026-05-12T12:00:00Z",
            false,
        );
        db.reconcile_address_history("acct-a").unwrap();

        db.invalidate_address_history("acct-a").unwrap();
        let pass = db.reconcile_address_history("acct-a").unwrap();

        assert!(pass.rebuilt);
        assert_eq!(pass.removed, 0);
        assert_eq!(
            sorted_emails(&db, "acct-a"),
            vec!["ada@example.test", "filed@court.test"]
        );
        assert_eq!(
            emails(&db.suggest_addresses("acct-a", "filed", 8).unwrap()),
            vec!["filed@court.test"],
            "a manual contact with no history is still suggestible"
        );
    }

    /// History observing an address someone had already added by hand does not
    /// take the row over, and the observation is one interaction however many
    /// times it is reconciled.
    #[test]
    fn a_manual_contact_seen_in_history_stays_manual_and_is_counted_once() {
        let db = db_with_accounts();
        seed_contact(
            &db,
            "acct-a",
            "ada@example.test",
            Some("Ada, curated"),
            7,
            "2020-01-01T00:00:00",
        );
        thread_message(
            &db,
            "acct-a",
            1,
            "<m1@example.test>",
            "ada@example.test",
            "me@example.test",
            "2026-05-12T12:00:00Z",
            false,
        );

        db.reconcile_address_history("acct-a").unwrap();
        db.reconcile_address_history("acct-a").unwrap();

        assert_eq!(db.list_contacts("acct-a", None).unwrap().len(), 1);
        assert!(!is_derived(&db, "acct-a", "ada@example.test"));
        assert_eq!(
            history_count(&db, "acct-a", "ada@example.test"),
            1,
            "one message is one interaction, however often it is reconciled"
        );
        assert_eq!(
            contact(&db, "acct-a", "ada@example.test").message_count,
            7,
            "the manual counter is still not the derivation's to write"
        );

        // And when that history disappears, the row someone added stays.
        db.conn()
            .execute("DELETE FROM thread_messages", [])
            .unwrap();
        db.invalidate_address_history("acct-a").unwrap();
        let pass = db.reconcile_address_history("acct-a").unwrap();

        assert!(pass.rebuilt);
        assert_eq!(pass.removed, 0);
        assert_eq!(history_count(&db, "acct-a", "ada@example.test"), 0);
        assert_eq!(
            emails(&db.suggest_addresses("acct-a", "ada", 8).unwrap()),
            vec!["ada@example.test"]
        );
    }

    /// Curating a row the derivation invented — adding it through `envelope
    /// contacts add`, or tagging it — takes ownership of it, so a later rebuild
    /// leaves it alone with its tags and notes intact.
    #[test]
    fn curating_a_derived_contact_makes_it_manual_and_rebuild_safe() {
        let db = db_with_accounts();
        for (uid, sender) in [(1u32, "ada@example.test"), (2, "grace@example.test")] {
            thread_message(
                &db,
                "acct-a",
                uid,
                &format!("<m{uid}@example.test>"),
                sender,
                "me@example.test",
                "2026-05-12T12:00:00Z",
                false,
            );
        }
        db.reconcile_address_history("acct-a").unwrap();
        assert!(is_derived(&db, "acct-a", "ada@example.test"));
        assert!(is_derived(&db, "acct-a", "grace@example.test"));

        db.upsert_contact(&Contact {
            id: "curated".into(),
            account_id: "acct-a".into(),
            email: "ada@example.test".into(),
            name: None,
            tags: r#"["vendor"]"#.into(),
            notes: Some("Net-30".into()),
            message_count: 0,
            first_seen: Some("2026-05-12T12:00:00".into()),
            last_seen: Some("2026-05-12T12:00:00".into()),
            created_at: "2026-05-12T12:00:00".into(),
            updated_at: "2026-05-12T12:00:00".into(),
        })
        .unwrap();
        db.add_contact_tag("acct-a", "grace@example.test", "vip")
            .unwrap();
        assert!(!is_derived(&db, "acct-a", "ada@example.test"));
        assert!(!is_derived(&db, "acct-a", "grace@example.test"));

        // Their history disappears entirely; the curation does not.
        db.conn()
            .execute("DELETE FROM thread_messages", [])
            .unwrap();
        db.invalidate_address_history("acct-a").unwrap();
        let pass = db.reconcile_address_history("acct-a").unwrap();

        assert!(pass.rebuilt);
        assert_eq!(pass.removed, 0);
        assert_eq!(
            sorted_emails(&db, "acct-a"),
            vec!["ada@example.test", "grace@example.test"]
        );
        let ada = contact(&db, "acct-a", "ada@example.test");
        assert_eq!(ada.tags, r#"["vendor"]"#);
        assert_eq!(ada.notes.as_deref(), Some("Net-30"));
        assert_eq!(
            contact(&db, "acct-a", "grace@example.test").tags,
            r#"["vip"]"#
        );
    }

    /// A rebuild zeroes the derived counts before it re-folds them, and a big
    /// enough thread cache spends several chunks in that state. A row whose
    /// count has been reset but not yet recomputed is not a suggestion, and it
    /// is not a deletion either — the pass has simply not reached its source.
    #[test]
    fn a_chunked_rebuild_neither_suggests_nor_deletes_a_row_it_has_not_reached() {
        let db = db_with_accounts();
        for (uid, sender) in [(1u32, "ada@example.test"), (2, "grace@example.test")] {
            thread_message(
                &db,
                "acct-a",
                uid,
                &format!("<m{uid}@example.test>"),
                sender,
                "me@example.test",
                "2026-05-12T12:00:00Z",
                false,
            );
        }
        db.reconcile_address_history("acct-a").unwrap();
        db.invalidate_address_history("acct-a").unwrap();

        let pass = db.reconcile_address_history_chunk("acct-a", 1).unwrap();
        assert!(pass.rebuilt && pass.pending);
        assert_eq!(
            pass.removed, 0,
            "a pending pass deletes nothing: it has not read the sources yet"
        );
        assert_eq!(
            emails(&db.suggest_addresses("acct-a", "example.test", 8).unwrap()),
            vec!["ada@example.test"],
            "the row this pass has not re-folded yet must not be offered at zero signal"
        );

        db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(
            sorted_emails(&db, "acct-a"),
            vec!["ada@example.test", "grace@example.test"],
            "and it comes back when the rebuild reaches it"
        );
        assert_eq!(history_count(&db, "acct-a", "grace@example.test"), 1);
    }

    #[test]
    fn reconcile_ignores_unsent_drafts() {
        let db = db_with_accounts();
        db.create_draft(
            "acct-a",
            "never-sent@example.test",
            Some("Draft"),
            Some("body"),
            None,
            None,
            None,
            None,
            Some("agent"),
        )
        .unwrap();

        let summary = db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(summary.observed, 0);
        assert!(db.list_contacts("acct-a", None).unwrap().is_empty());
    }

    // ── The send edge ─────────────────────────────────────────────────

    /// The point of the whole edge: you write to someone, and the next thing
    /// you compose can suggest them. No restart, no thread scan, no unified
    /// refresh, no reconcile — and To, Cc, and Bcc all count as having been
    /// written to.
    #[test]
    fn a_send_makes_its_recipients_suggestible_before_anything_reconciles() {
        let db = db_with_accounts();
        send_draft(
            &db,
            "acct-a",
            "Bob Vendor <bob@vendor.test>",
            Some("cc@vendor.test"),
            Some("bcc@vendor.test"),
        );

        // Nothing has reconciled: no boundary row exists at all.
        let boundaries: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM address_history_state WHERE account_id = 'acct-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(boundaries, 0, "the send edge must not need a reconcile");

        let found = db.suggest_addresses("acct-a", "vendor.test", 8).unwrap();
        for (header, address) in [
            ("To", "bob@vendor.test"),
            ("Cc", "cc@vendor.test"),
            ("Bcc", "bcc@vendor.test"),
        ] {
            assert!(
                emails(&found).contains(&address),
                "the {header} recipient was not suggestible straight after the send: {:?}",
                emails(&found)
            );
        }

        assert_eq!(
            contact(&db, "acct-a", "bob@vendor.test").name.as_deref(),
            Some("Bob Vendor")
        );
        assert_eq!(
            history_count(&db, "acct-a", "bob@vendor.test"),
            0,
            "the send edge writes its own column and leaves the reconcile's alone"
        );
        assert_eq!(sent_count(&db, "acct-a", "bob@vendor.test"), 1);
        assert!(is_derived(&db, "acct-a", "bob@vendor.test"));
    }

    /// A draft the account is still writing is not history, and neither is a
    /// transition refused for want of the ownership token. History follows
    /// SMTP acceptance and the owner lease, or it does not happen.
    #[test]
    fn nothing_but_an_owned_successful_transition_records_history() {
        let db = db_with_accounts();
        let draft = db
            .create_draft(
                "acct-a",
                "bob@vendor.test",
                Some("Subject"),
                Some("body"),
                None,
                None,
                Some("cc@vendor.test"),
                Some("bcc@vendor.test"),
                Some("human:dashboard"),
            )
            .unwrap();
        assert!(
            db.list_contacts("acct-a", None).unwrap().is_empty(),
            "an unsent draft is not history"
        );

        // Never claimed: there is no lease to hold.
        assert!(
            db.mark_draft_sent(&draft.id, "never-claimed", Some("<m@x>"))
                .is_err()
        );
        assert!(db.list_contacts("acct-a", None).unwrap().is_empty());

        // Claimed by someone else's token.
        let _lease = db
            .claim_draft_for_immediate_send(&draft.id, draft.revision)
            .unwrap()
            .expect("claim");
        assert!(
            db.mark_draft_sent(&draft.id, "not-the-owner", Some("<m@x>"))
                .is_err()
        );
        assert!(
            db.list_contacts("acct-a", None).unwrap().is_empty(),
            "a transition that lost the ownership token records no recipients"
        );
        assert!(
            db.suggest_addresses("acct-a", "vendor.test", 8)
                .unwrap()
                .is_empty()
        );
    }

    /// The message comes back around: the Sent-folder copy is cached by a
    /// later scan and reconciled like any other thread row. The send edge must
    /// not have inflated the count it lands on — one message stays one
    /// interaction, and the result is identical to an install where the send
    /// edge never ran.
    ///
    /// The reconciliation boundary is established first, because that is the
    /// state any install past its first refresh is in — and because a first
    /// reconcile is a rebuild, which would reset the derived counts and hide
    /// the very stacking this is guarding.
    #[test]
    fn the_sent_folder_copy_arriving_later_does_not_double_count() {
        let db = db_with_accounts();
        db.reconcile_address_history("acct-a").unwrap();

        send_draft(&db, "acct-a", "bob@vendor.test", None, None);
        assert_eq!(signal(&db, "acct-a", "bob@vendor.test"), 1);

        // The scan reaches the Sent folder and caches the copy of that same
        // message; the reconcile then folds it in.
        thread_message(
            &db,
            "acct-a",
            1,
            "<sent@example.test>",
            "me@example.test",
            "bob@vendor.test",
            "2026-05-12T12:00:00Z",
            true,
        );
        db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(
            signal(&db, "acct-a", "bob@vendor.test"),
            1,
            "the sent draft and its Sent-folder copy are one message"
        );
        assert_eq!(history_count(&db, "acct-a", "bob@vendor.test"), 1);
        assert_eq!(sent_count(&db, "acct-a", "bob@vendor.test"), 1);

        db.reconcile_address_history("acct-a").unwrap();
        db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(
            signal(&db, "acct-a", "bob@vendor.test"),
            1,
            "and repeat reconciles do not accumulate either"
        );

        // The control: the same message observed only through the thread
        // cache, on an install where nothing was sent locally.
        let control = db_with_accounts();
        control.reconcile_address_history("acct-a").unwrap();
        thread_message(
            &control,
            "acct-a",
            1,
            "<sent@example.test>",
            "me@example.test",
            "bob@vendor.test",
            "2026-05-12T12:00:00Z",
            true,
        );
        control.reconcile_address_history("acct-a").unwrap();
        assert_eq!(
            signal(&db, "acct-a", "bob@vendor.test"),
            signal(&control, "acct-a", "bob@vendor.test"),
            "the send edge must leave the settled count exactly where it found it"
        );
    }

    /// The same, on an install whose first reconcile has not run yet: the
    /// rebuild that first pass performs resets the derived counts, and the
    /// recipient must survive it with the same single interaction rather than
    /// vanishing from the dropdown or coming back doubled.
    #[test]
    fn a_send_before_the_first_reconcile_survives_the_rebuild_intact() {
        let db = db_with_accounts();
        send_draft(&db, "acct-a", "bob@vendor.test", None, None);
        thread_message(
            &db,
            "acct-a",
            1,
            "<sent@example.test>",
            "me@example.test",
            "bob@vendor.test",
            "2026-05-12T12:00:00Z",
            true,
        );

        db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(
            emails(&db.suggest_addresses("acct-a", "bob", 8).unwrap()),
            vec!["bob@vendor.test"]
        );
        assert_eq!(signal(&db, "acct-a", "bob@vendor.test"), 1);
    }

    /// The count is recomputed from the sent-draft window, never incremented,
    /// so running the edge twice for one send is indistinguishable from
    /// running it once — and a genuinely second send still moves it.
    #[test]
    fn the_send_edge_recomputes_rather_than_increments() {
        let db = db_with_accounts();
        let draft_id = send_draft(&db, "acct-a", "bob@vendor.test", None, None);

        db.record_sent_draft_recipients(&draft_id).unwrap();
        db.record_sent_draft_recipients(&draft_id).unwrap();
        assert_eq!(
            sent_count(&db, "acct-a", "bob@vendor.test"),
            1,
            "re-running the edge for one send must not raise the count"
        );

        send_draft(&db, "acct-a", "bob@vendor.test", None, None);
        assert_eq!(
            sent_count(&db, "acct-a", "bob@vendor.test"),
            2,
            "a second send is a second interaction"
        );
    }

    /// Writing to someone does not make their contact record the
    /// derivation's, and does not touch the counter an import owns.
    #[test]
    fn the_send_edge_preserves_manual_provenance() {
        let db = db_with_accounts();
        db.upsert_contact(&Contact {
            id: "curated".into(),
            account_id: "acct-a".into(),
            email: "Bob@Vendor.test".into(),
            name: Some("Bob, curated".into()),
            tags: r#"["vendor"]"#.into(),
            notes: Some("Net-30".into()),
            message_count: 99,
            first_seen: Some("2020-01-01T00:00:00".into()),
            last_seen: Some("2020-01-01T00:00:00".into()),
            created_at: "2020-01-01T00:00:00".into(),
            updated_at: "2020-01-01T00:00:00".into(),
        })
        .unwrap();

        send_draft(&db, "acct-a", "Bob Vendor <bob@vendor.test>", None, None);

        let stored = db.list_contacts("acct-a", None).unwrap();
        assert_eq!(stored.len(), 1, "the send must not add a lowercase twin");
        assert_eq!(stored[0].name.as_deref(), Some("Bob, curated"));
        assert_eq!(stored[0].message_count, 99);
        assert_eq!(stored[0].notes.as_deref(), Some("Net-30"));
        assert!(
            !is_derived(&db, "acct-a", "bob@vendor.test"),
            "writing to a curated contact does not make it the derivation's to delete"
        );
        assert_eq!(sent_count(&db, "acct-a", "bob@vendor.test"), 1);
    }

    /// One account's outbound recipients are not another's suggestions.
    #[test]
    fn a_send_stays_inside_its_own_account() {
        let db = db_with_accounts();
        send_draft(
            &db,
            "acct-a",
            "bob@vendor.test",
            Some("cc@vendor.test"),
            None,
        );

        assert!(
            db.list_contacts("acct-b", None).unwrap().is_empty(),
            "the other account gains nothing from this send"
        );
        assert!(
            db.suggest_addresses("acct-b", "vendor.test", 8)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            sorted_emails(&db, "acct-a"),
            vec!["bob@vendor.test", "cc@vendor.test"]
        );
    }

    /// Sending to yourself is not history worth suggesting back, the same way
    /// it is not on the To line of an inbound message.
    #[test]
    fn a_send_to_the_account_itself_suggests_nothing() {
        let db = db_with_accounts();
        send_draft(&db, "acct-a", "me@example.test", None, None);
        assert!(db.list_contacts("acct-a", None).unwrap().is_empty());
    }

    /// Malformed header entries are dropped, and never at the cost of the
    /// valid addresses beside them.
    #[test]
    fn reconcile_drops_malformed_addresses_from_every_source() {
        let db = db_with_accounts();
        thread_message(
            &db,
            "acct-a",
            1,
            "<m1@example.test>",
            "undisclosed-recipients",
            "ada@example.test, mailer-daemon, @nobody.test, ok@example.test",
            "2026-05-12T12:00:00Z",
            false,
        );
        db.upsert_indexed_message_summaries(
            "acct-a",
            "INBOX",
            1,
            &[summary(
                1,
                "not an address",
                "grace@example.test, broken@",
                "Tue, 12 May 2026 12:00:00 +0000",
            )],
        )
        .unwrap();

        db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(
            sorted_emails(&db, "acct-a"),
            vec!["ada@example.test", "grace@example.test", "ok@example.test"]
        );
    }

    /// A quoted display name carrying a comma is one recipient, in the deep
    /// cache exactly as in the dashboard's snapshot.
    #[test]
    fn reconcile_keeps_quoted_display_names_whole() {
        let db = db_with_accounts();
        thread_message(
            &db,
            "acct-a",
            1,
            "<m1@example.test>",
            "me@example.test",
            "\"Doe, Jane\" <jane@example.test>, \"Roe, John\" <john@example.test>",
            "2026-05-12T12:00:00Z",
            true,
        );

        db.reconcile_address_history("acct-a").unwrap();
        assert_eq!(
            sorted_emails(&db, "acct-a"),
            vec!["jane@example.test", "john@example.test"]
        );
    }

    // ── Suggestions ───────────────────────────────────────────────────

    fn seed_contact(
        db: &Database,
        account: &str,
        email: &str,
        name: Option<&str>,
        count: i64,
        last_seen: &str,
    ) {
        db.upsert_contact(&Contact {
            id: uuid::Uuid::new_v4().to_string(),
            account_id: account.into(),
            email: email.into(),
            name: name.map(str::to_string),
            tags: "[]".into(),
            notes: None,
            message_count: count,
            first_seen: Some("2026-01-01T00:00:00".into()),
            last_seen: Some(last_seen.into()),
            created_at: "2026-01-01T00:00:00".into(),
            updated_at: "2026-01-01T00:00:00".into(),
        })
        .unwrap();
    }

    /// The whole reason the cache exists: a suggestion must be answerable from
    /// `contacts` alone, with the thread cache untouched.
    #[test]
    fn the_suggestion_query_never_reads_the_thread_cache() {
        let db = db_with_accounts();
        let plan = db.suggestion_query_plan("acct-a", "ada").unwrap();
        assert!(
            plan.contains("contacts"),
            "the plan should read contacts: {plan}"
        );
        for table in ["thread_messages", "threads", "indexed_message_summaries"] {
            assert!(
                !plan.contains(table),
                "typing must not reach {table}: {plan}"
            );
        }
    }

    /// The candidate cap is a bound on work, not a filter on relevance. An
    /// account with more matches than the cap must still surface the exact and
    /// prefix hits, however quiet those contacts are next to a wall of
    /// high-frequency substring matches.
    #[test]
    fn an_exact_match_survives_the_candidate_cap() {
        let db = db_with_accounts();

        // Well past SUGGESTION_SCAN_CAP rows that match "ada" only as a
        // substring, each of them far busier than the two that matter.
        let tx = db.conn().unchecked_transaction().unwrap();
        for index in 0..(SUGGESTION_SCAN_CAP + 200) {
            tx.execute(
                "INSERT INTO contacts
                    (id, account_id, email, name, tags, notes, message_count, history_count,
                     first_seen, last_seen, created_at, updated_at)
                 VALUES (?1, 'acct-a', ?2, NULL, '[]', NULL, 500, 0,
                         '2026-01-01T00:00:00', '2026-06-01T00:00:00',
                         '2026-01-01T00:00:00', '2026-01-01T00:00:00')",
                params![
                    format!("bulk-{index:05}"),
                    format!("brigada{index:05}@example.test")
                ],
            )
            .unwrap();
        }
        tx.commit().unwrap();

        // Three quiet, old rows: one exact (the name IS the query), two prefix.
        seed_contact(
            &db,
            "acct-a",
            "quiet@example.test",
            Some("Ada"),
            1,
            "2020-01-01T00:00:00",
        );
        seed_contact(
            &db,
            "acct-a",
            "ada@example.test",
            Some("Ada Lovelace"),
            1,
            "2020-01-01T00:00:00",
        );
        seed_contact(
            &db,
            "acct-a",
            "adalovelace@example.test",
            None,
            1,
            "2020-01-01T00:00:00",
        );

        let rows = db.suggest_addresses("acct-a", "ada", 5).unwrap();
        assert_eq!(
            emails(&rows)[..3],
            [
                "quiet@example.test",
                "ada@example.test",
                "adalovelace@example.test"
            ],
            "exact then prefix, ahead of {} busier substring rows",
            SUGGESTION_SCAN_CAP + 200
        );
    }

    #[test]
    fn suggestions_rank_exact_then_prefix_then_substring() {
        let db = db_with_accounts();
        seed_contact(
            &db,
            "acct-a",
            "ada@example.test",
            Some("Ada Lovelace"),
            1,
            "2026-01-01T00:00:00",
        );
        seed_contact(
            &db,
            "acct-a",
            "adalovelace@example.test",
            Some("Adaline"),
            50,
            "2026-06-01T00:00:00",
        );
        seed_contact(
            &db,
            "acct-a",
            "grace@example.test",
            Some("Grace Ada Hopper"),
            90,
            "2026-06-01T00:00:00",
        );
        seed_contact(
            &db,
            "acct-a",
            "brigada@example.test",
            None,
            900,
            "2026-06-01T00:00:00",
        );

        let rows = db
            .suggest_addresses("acct-a", "ada@example.test", 10)
            .unwrap();
        assert_eq!(rows[0].email, "ada@example.test", "exact address wins");

        let rows = db.suggest_addresses("acct-a", "ada", 10).unwrap();
        assert_eq!(
            emails(&rows),
            vec![
                // Prefix tier, ordered by frequency: name-word prefix counts.
                "grace@example.test",
                "adalovelace@example.test",
                "ada@example.test",
                // Substring only ("brig-ada"), even with the highest count.
                "brigada@example.test",
            ]
        );
    }

    /// The derived counter ranks a contact even when nothing has ever written
    /// `message_count` for it.
    #[test]
    fn suggestions_rank_on_derived_history_when_no_import_count_exists() {
        let db = db_with_accounts();
        seed_contact(
            &db,
            "acct-a",
            "quiet@example.test",
            None,
            0,
            "2026-06-01T00:00:00",
        );
        seed_contact(
            &db,
            "acct-a",
            "busy@example.test",
            None,
            0,
            "2026-01-01T00:00:00",
        );
        db.conn()
            .execute(
                "UPDATE contacts SET history_count = 42 WHERE email = 'busy@example.test'",
                [],
            )
            .unwrap();

        assert_eq!(
            emails(&db.suggest_addresses("acct-a", "example", 10).unwrap()),
            vec!["busy@example.test", "quiet@example.test"]
        );
    }

    #[test]
    fn suggestions_break_ties_by_frequency_then_recency_then_address() {
        let db = db_with_accounts();
        seed_contact(
            &db,
            "acct-a",
            "rare@vendor.test",
            None,
            1,
            "2026-06-01T00:00:00",
        );
        seed_contact(
            &db,
            "acct-a",
            "often@vendor.test",
            None,
            40,
            "2026-01-01T00:00:00",
        );
        seed_contact(
            &db,
            "acct-a",
            "stale@vendor.test",
            None,
            40,
            "2025-01-01T00:00:00",
        );

        let rows = db.suggest_addresses("acct-a", "vendor", 10).unwrap();
        assert_eq!(
            emails(&rows),
            vec!["often@vendor.test", "stale@vendor.test", "rare@vendor.test"]
        );

        // Identical signal must produce identical ordering on every call.
        seed_contact(
            &db,
            "acct-a",
            "b@vendor.test",
            None,
            40,
            "2026-01-01T00:00:00",
        );
        seed_contact(
            &db,
            "acct-a",
            "a@vendor.test",
            None,
            40,
            "2026-01-01T00:00:00",
        );
        let first = db.suggest_addresses("acct-a", "vendor", 10).unwrap();
        let second = db.suggest_addresses("acct-a", "vendor", 10).unwrap();
        assert_eq!(first, second);
        let ordered = emails(&first);
        assert!(
            ordered.iter().position(|e| *e == "a@vendor.test")
                < ordered.iter().position(|e| *e == "b@vendor.test"),
            "equal rows sort by address: {ordered:?}"
        );
    }

    #[test]
    fn suggestions_match_display_names_and_domains() {
        let db = db_with_accounts();
        seed_contact(
            &db,
            "acct-a",
            "gh@example.test",
            Some("Grace Hopper"),
            5,
            "2026-06-01T00:00:00",
        );
        seed_contact(
            &db,
            "acct-a",
            "billing@acme.test",
            None,
            5,
            "2026-06-01T00:00:00",
        );

        assert_eq!(
            emails(&db.suggest_addresses("acct-a", "hopper", 10).unwrap()),
            vec!["gh@example.test"]
        );
        assert_eq!(
            emails(&db.suggest_addresses("acct-a", "acme", 10).unwrap()),
            vec!["billing@acme.test"]
        );
    }

    #[test]
    fn suggestions_are_account_scoped() {
        let db = db_with_accounts();
        seed_contact(
            &db,
            "acct-a",
            "mine@example.test",
            None,
            5,
            "2026-06-01T00:00:00",
        );
        seed_contact(
            &db,
            "acct-b",
            "theirs@example.test",
            None,
            5,
            "2026-06-01T00:00:00",
        );

        assert_eq!(
            emails(&db.suggest_addresses("acct-a", "example", 10).unwrap()),
            vec!["mine@example.test"]
        );
        assert_eq!(
            emails(&db.suggest_addresses("acct-b", "example", 10).unwrap()),
            vec!["theirs@example.test"]
        );
    }

    #[test]
    fn suggestions_deduplicate_case_variants_keeping_the_stronger_row() {
        let db = db_with_accounts();
        seed_contact(
            &db,
            "acct-a",
            "Ada@Example.test",
            Some("Ada Lovelace"),
            40,
            "2026-06-01T00:00:00",
        );
        seed_contact(
            &db,
            "acct-a",
            "ada@example.test",
            None,
            2,
            "2026-01-01T00:00:00",
        );

        let rows = db.suggest_addresses("acct-a", "ada", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].email, "ada@example.test",
            "always emitted lowercased"
        );
        assert_eq!(rows[0].name.as_deref(), Some("Ada Lovelace"));
    }

    #[test]
    fn suggestions_honour_the_limit() {
        let db = db_with_accounts();
        for index in 0..25 {
            seed_contact(
                &db,
                "acct-a",
                &format!("person{index:02}@example.test"),
                None,
                i64::from(index),
                "2026-06-01T00:00:00",
            );
        }

        assert_eq!(
            db.suggest_addresses("acct-a", "person", 8).unwrap().len(),
            8
        );
        assert_eq!(
            db.suggest_addresses("acct-a", "person", 1).unwrap().len(),
            1
        );
        assert!(
            db.suggest_addresses("acct-a", "person", 0)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn suggestions_never_offer_a_malformed_stored_address() {
        let db = db_with_accounts();
        // `envelope contacts add` does not validate, so garbage can land in the
        // table. It must not reach a compose field.
        seed_contact(
            &db,
            "acct-a",
            "not-an-email",
            Some("Broken"),
            500,
            "2026-06-01T00:00:00",
        );
        seed_contact(
            &db,
            "acct-a",
            "ok@example.test",
            None,
            1,
            "2026-01-01T00:00:00",
        );

        let rows = db.suggest_addresses("acct-a", "", 10).unwrap();
        assert_eq!(emails(&rows), vec!["ok@example.test"]);
    }

    #[test]
    fn blank_query_ranks_the_account_by_signal_alone() {
        let db = db_with_accounts();
        seed_contact(
            &db,
            "acct-a",
            "quiet@example.test",
            None,
            1,
            "2026-01-01T00:00:00",
        );
        seed_contact(
            &db,
            "acct-a",
            "loud@example.test",
            None,
            30,
            "2026-01-01T00:00:00",
        );

        assert_eq!(
            emails(&db.suggest_addresses("acct-a", "  ", 10).unwrap()),
            vec!["loud@example.test", "quiet@example.test"]
        );
    }

    #[test]
    fn like_wildcards_in_the_query_are_matched_literally() {
        let db = db_with_accounts();
        seed_contact(
            &db,
            "acct-a",
            "a_b@example.test",
            None,
            1,
            "2026-01-01T00:00:00",
        );
        seed_contact(
            &db,
            "acct-a",
            "axb@example.test",
            None,
            90,
            "2026-01-01T00:00:00",
        );

        let rows = db.suggest_addresses("acct-a", "a_b", 10).unwrap();
        assert_eq!(
            emails(&rows),
            vec!["a_b@example.test"],
            "`_` must not act as a single-character wildcard"
        );
    }

    #[test]
    fn suggestions_carry_no_message_content() {
        let db = db_with_accounts();
        thread_message(
            &db,
            "acct-a",
            1,
            "<m1@example.test>",
            "ada@example.test",
            "me@example.test",
            "2026-05-12T12:00:00Z",
            false,
        );
        db.upsert_indexed_message_summaries(
            "acct-a",
            "INBOX",
            1,
            &[summary(
                1,
                "Ada <ada@example.test>",
                "me@example.test",
                "Tue, 12 May 2026 12:00:00 +0000",
            )],
        )
        .unwrap();
        db.reconcile_address_history("acct-a").unwrap();

        let rows = db.suggest_addresses("acct-a", "ada", 10).unwrap();
        let serialized = serde_json::to_string(&rows).unwrap();
        assert!(!serialized.contains("Quarterly filing"));
        assert!(!serialized.contains("body text"));
        assert!(!serialized.contains("snippet"));
        assert_eq!(serialized, r#"[{"email":"ada@example.test","name":"Ada"}]"#);
    }
}
