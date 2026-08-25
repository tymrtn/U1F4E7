// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::db::Database;
use crate::errors::{Result, StoreError};
use crate::models::{Draft, DraftStatus};
use rusqlite::params;
use uuid::Uuid;

/// A held provider-sync lease: the opaque owner token plus the status to
/// restore on release.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncClaim {
    pub token: String,
    pub prior_status: DraftStatus,
}

impl Database {
    pub fn create_draft(
        &self,
        account_id: &str,
        to_addr: &str,
        subject: Option<&str>,
        text_content: Option<&str>,
        html_content: Option<&str>,
        in_reply_to: Option<&str>,
        cc_addr: Option<&str>,
        bcc_addr: Option<&str>,
        created_by: Option<&str>,
    ) -> Result<Draft> {
        let id = Uuid::new_v4().to_string();

        self.conn().execute(
            "INSERT INTO drafts (id, account_id, to_addr, subject, text_content, html_content,
             in_reply_to, cc_addr, bcc_addr, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                id,
                account_id,
                to_addr,
                subject,
                text_content,
                html_content,
                in_reply_to,
                cc_addr,
                bcc_addr,
                created_by
            ],
        )?;

        self.get_draft(&id)?
            .ok_or_else(|| StoreError::DraftNotFound(id))
    }

    pub fn get_draft(&self, id: &str) -> Result<Option<Draft>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, status, to_addr, cc_addr, bcc_addr, reply_to, subject,
                    text_content, html_content, in_reply_to, metadata, attachments, message_id,
                    send_after, snoozed_until, created_at, updated_at, sent_at, created_by,
                    imap_uid, revision
             FROM drafts WHERE id = ?1",
        )?;

        let draft = stmt
            .query_row(params![id], |row| {
                let status_str: String = row.get(2)?;
                let metadata_str: Option<String> = row.get(11)?;
                let attachments_str: String = row.get(12)?;
                let imap_uid_i64: Option<i64> = row.get(20)?;
                Ok(Draft {
                    id: row.get(0)?,
                    account_id: row.get(1)?,
                    status: status_str.parse().unwrap_or(DraftStatus::Draft),
                    to_addr: row.get(3)?,
                    cc_addr: row.get(4)?,
                    bcc_addr: row.get(5)?,
                    reply_to: row.get(6)?,
                    subject: row.get(7)?,
                    text_content: row.get(8)?,
                    html_content: row.get(9)?,
                    in_reply_to: row.get(10)?,
                    metadata: metadata_str.and_then(|s| serde_json::from_str(&s).ok()),
                    attachments: serde_json::from_str(&attachments_str).unwrap_or_default(),
                    message_id: row.get(13)?,
                    send_after: row.get(14)?,
                    snoozed_until: row.get(15)?,
                    created_at: row.get(16)?,
                    updated_at: row.get(17)?,
                    sent_at: row.get(18)?,
                    created_by: row.get(19)?,
                    imap_uid: imap_uid_i64.map(|v| v as u32),
                    revision: row.get(21)?,
                })
            })
            .optional()?;

        Ok(draft)
    }

    /// Find the active local draft that corresponds to an IMAP Drafts-folder UID.
    ///
    /// Dashboard links often arrive as `/messages/<imap_uid>?folder=<Drafts>`
    /// because IMAP clients identify the server-side draft by UID. Envelope's
    /// review surface, however, is the local draft row. This lookup lets the
    /// dashboard prioritize the reviewable local draft instead of showing a raw
    /// read-only message/composer detour.
    pub fn get_draft_by_imap_uid(&self, account_id: &str, imap_uid: u32) -> Result<Option<Draft>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, status, to_addr, cc_addr, bcc_addr, reply_to, subject,
                    text_content, html_content, in_reply_to, metadata, attachments, message_id,
                    send_after, snoozed_until, created_at, updated_at, sent_at, created_by,
                    imap_uid, revision
             FROM drafts
             WHERE account_id = ?1
               AND imap_uid = ?2
               AND status IN ('draft', 'pending_review', 'blocked')
             ORDER BY updated_at DESC
             LIMIT 1",
        )?;

        let draft = stmt
            .query_row(params![account_id, imap_uid as i64], Self::map_draft)
            .optional()?;
        Ok(draft)
    }

    pub fn list_drafts(
        &self,
        account_id: &str,
        status: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Draft>> {
        let sql = if status.is_some() {
            "SELECT id, account_id, status, to_addr, cc_addr, bcc_addr, reply_to, subject,
                    text_content, html_content, in_reply_to, metadata, attachments, message_id,
                    send_after, snoozed_until, created_at, updated_at, sent_at, created_by,
                    imap_uid, revision
             FROM drafts WHERE account_id = ?1 AND status = ?2
             ORDER BY updated_at DESC LIMIT ?3 OFFSET ?4"
        } else {
            "SELECT id, account_id, status, to_addr, cc_addr, bcc_addr, reply_to, subject,
                    text_content, html_content, in_reply_to, metadata, attachments, message_id,
                    send_after, snoozed_until, created_at, updated_at, sent_at, created_by,
                    imap_uid, revision
             FROM drafts WHERE account_id = ?1
             ORDER BY updated_at DESC LIMIT ?3 OFFSET ?4"
        };

        let mut stmt = self.conn().prepare(sql)?;
        let rows = if let Some(s) = status {
            stmt.query_map(params![account_id, s, limit, offset], Self::map_draft)?
        } else {
            stmt.query_map(params![account_id, "", limit, offset], Self::map_draft)?
        };

        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn update_draft_status(&self, id: &str, status: DraftStatus) -> Result<()> {
        // The editable predicate lives INSIDE the statement: a pre-read check
        // alone races with the sweep's `sending` claim (or a concurrent
        // terminal transition) landing between read and write — this UPDATE
        // must never yank a claimed/terminal row back to an editable status.
        let rows = self.conn().execute(
            "UPDATE drafts SET status = ?1, updated_at = datetime('now')
             WHERE id = ?2 AND status IN ('draft', 'pending_review', 'blocked')",
            params![status.as_str(), id],
        )?;
        if rows == 0 {
            return Err(self.classify_guarded_update_miss(id));
        }
        Ok(())
    }

    /// Explain why a status/revision-guarded draft UPDATE matched zero rows,
    /// in precedence order: the draft does not exist → [`StoreError::DraftNotFound`];
    /// it is in a non-editable state (`sending`/`sent`/`discarded`) →
    /// [`StoreError::DraftNotEditable`]; otherwise — the caller's expected
    /// revision is stale, or the row raced through states between the missed
    /// UPDATE and this read (e.g. claim → miss → release) —
    /// [`StoreError::DraftModifiedConcurrently`].
    fn classify_guarded_update_miss(&self, id: &str) -> StoreError {
        match self.get_draft(id) {
            Ok(None) => StoreError::DraftNotFound(id.to_string()),
            Ok(Some(current)) if !current.status.is_editable() => {
                StoreError::DraftNotEditable(current.status.as_str().to_string())
            }
            Ok(Some(_)) => StoreError::DraftModifiedConcurrently(id.to_string()),
            Err(e) => e,
        }
    }

    /// Explain why a queue CAS ([`Self::queue_draft_for_send`]) matched zero
    /// rows. Identical precedence to [`Self::classify_guarded_update_miss`],
    /// except a `blocked` row is reported as [`StoreError::DraftNotEditable`]
    /// rather than a concurrent-edit conflict: `blocked` is
    /// [`DraftStatus::is_editable`] (a human may edit it) but NOT
    /// [`DraftStatus::is_queueable`], so a denied row that a caller tried to
    /// re-schedule gets a truthful "not queueable" refusal, never a resurrection.
    fn classify_queue_miss(&self, id: &str) -> StoreError {
        match self.get_draft(id) {
            Ok(None) => StoreError::DraftNotFound(id.to_string()),
            Ok(Some(current)) if !current.status.is_queueable() => {
                StoreError::DraftNotEditable(current.status.as_str().to_string())
            }
            Ok(Some(_)) => StoreError::DraftModifiedConcurrently(id.to_string()),
            Err(e) => e,
        }
    }

    /// Edit a draft's recipients, subject, and body.
    ///
    /// `to_addr`/`subject` are per-field: `None` keeps the stored value.
    /// `cc_addr`/`bcc_addr` are written as given (pass the existing value to
    /// keep it).
    ///
    /// The two body fields move together as the draft's body representation
    /// set. Supplying either `text_content` or `html_content` writes the
    /// supplied pair exactly and CLEARS the omitted alternate; supplying
    /// neither preserves both. A draft holding an edited text body beside a
    /// pre-edit HTML body is transmitted as `multipart/alternative`, where
    /// receiving clients prefer the HTML — so preserving the bodies per-field
    /// would deliver the content the editor was used to change.
    pub fn update_draft_content(
        &self,
        id: &str,
        to_addr: Option<&str>,
        cc_addr: Option<&str>,
        bcc_addr: Option<&str>,
        subject: Option<&str>,
        text_content: Option<&str>,
        html_content: Option<&str>,
    ) -> Result<Draft> {
        self.update_draft_content_inner(
            id,
            None,
            to_addr,
            cc_addr,
            bcc_addr,
            subject,
            text_content,
            html_content,
        )
    }

    /// Revision-guarded content edit for human surfaces: identical to
    /// [`Self::update_draft_content`] but the UPDATE is compare-and-set on
    /// `expected_revision` — the revision the human was viewing. A concurrent
    /// edit returns [`StoreError::DraftModifiedConcurrently`] instead of
    /// silently overwriting content the human never saw.
    #[allow(clippy::too_many_arguments)]
    pub fn update_draft_content_for_revision(
        &self,
        id: &str,
        expected_revision: i64,
        to_addr: Option<&str>,
        cc_addr: Option<&str>,
        bcc_addr: Option<&str>,
        subject: Option<&str>,
        text_content: Option<&str>,
        html_content: Option<&str>,
    ) -> Result<Draft> {
        self.update_draft_content_inner(
            id,
            Some(expected_revision),
            to_addr,
            cc_addr,
            bcc_addr,
            subject,
            text_content,
            html_content,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn update_draft_content_inner(
        &self,
        id: &str,
        expected_revision: Option<i64>,
        to_addr: Option<&str>,
        cc_addr: Option<&str>,
        bcc_addr: Option<&str>,
        subject: Option<&str>,
        text_content: Option<&str>,
        html_content: Option<&str>,
    ) -> Result<Draft> {
        // `?9` gates BOTH body columns as one unit: a supplied body replaces
        // the pair exactly, and a recipient/subject-only edit (neither
        // supplied) leaves the pair alone. Coalescing the columns
        // independently left a text-only edit sitting beside the old HTML, and
        // that dual-body row goes out as `multipart/alternative` — receiving
        // clients prefer the HTML alternative, so the recipient read the
        // pre-edit draft. Matches `apply_synced_draft_edit`, where the CLI
        // edit path already writes the body pair as a replacement.
        let body_edit = text_content.is_some() || html_content.is_some();
        // One atomic statement: the content change, the revision bump, the
        // approval invalidation, the editable-status guard, and (when bound,
        // ?8) the stale-view revision guard all land together or not at all —
        // no failure or interleaving (including the sweep's `sending` claim
        // arriving between a pre-read and the write) can mutate a claimed or
        // terminal row, or leave changed content with the old approval.
        let rows = self.conn().execute(
            "UPDATE drafts SET
                to_addr = COALESCE(?1, to_addr),
                cc_addr = ?2,
                bcc_addr = ?3,
                subject = COALESCE(?4, subject),
                text_content = CASE WHEN ?9 THEN ?5 ELSE text_content END,
                html_content = CASE WHEN ?9 THEN ?6 ELSE html_content END,
                metadata = json_remove(metadata, '$.human_approval', '$.human_send'),
                revision = revision + 1,
                updated_at = datetime('now')
             WHERE id = ?7
               AND status IN ('draft', 'pending_review', 'blocked')
               AND (?8 IS NULL OR revision = ?8)",
            params![
                to_addr,
                cc_addr,
                bcc_addr,
                subject,
                text_content,
                html_content,
                id,
                expected_revision,
                body_edit
            ],
        )?;
        if rows == 0 {
            return Err(self.classify_guarded_update_miss(id));
        }

        self.get_draft(id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.to_string()))
    }

    /// Record a completed SMTP transmission: `sending` → `sent`.
    ///
    /// This is the ONLY exit from a `sending` claim after SMTP acceptance,
    /// and it requires the owner lease token acquired at claim time — an
    /// actor that did not acquire the claim (or lost it) cannot mark a draft
    /// sent, which closes the cross-path double-send (scheduled sweep vs
    /// CLI/MCP immediate send). The token is cleared on the terminal state.
    ///
    /// Being the one edge every send path crosses, this is also where the
    /// draft's recipients enter the compose autocomplete's address history —
    /// after SMTP acceptance, never before, and never on a transition the
    /// caller did not own. See [`Database::record_sent_draft_recipients`].
    pub fn mark_draft_sent(&self, id: &str, token: &str, message_id: Option<&str>) -> Result<()> {
        // On the terminal `sent` state, `send_after` and the Drafts-folder
        // `imap_uid` are both cleared:
        //  - `send_after`: a transmitted draft is no longer scheduled/due, so no
        //    surface can infer it is still queued (real evidence: a scheduled
        //    allowed send left `send_after` at the expired timestamp even after
        //    the row flipped to `sent`). Immediate sends carry no `send_after`,
        //    so clearing it there is a harmless no-op.
        //  - `imap_uid` is the IMAP *Drafts*-folder UID; once sent, the provider
        //    Drafts copy is being cleaned up (the send paths take the cleanup
        //    identity from the pre-transition in-memory snapshot, not this row),
        //    so the stored UID is stale. Clearing it keeps `imap_uid`
        //    Drafts-folder-only and prevents Sent proof from ever being conflated
        //    with a Drafts UID. Sent-folder proof lives in `metadata.sent_copy`.
        let rows = self.conn().execute(
            "UPDATE drafts SET status = 'sent', message_id = ?1, operation_token = NULL,
             send_after = NULL, imap_uid = NULL,
             sent_at = datetime('now'), updated_at = datetime('now')
             WHERE id = ?2 AND status = 'sending' AND operation_token = ?3",
            params![message_id, id, token],
        )?;
        if rows == 0 {
            return Err(match self.get_draft(id)? {
                None => StoreError::DraftNotFound(id.to_string()),
                Some(current) => StoreError::DraftNotEditable(format!(
                    "{} — only the holder of the `sending` lease can mark this draft sent",
                    current.status.as_str()
                )),
            });
        }

        // The send is durable from the statement above; everything past it is
        // cache maintenance. A suggestion cache that could not be written is a
        // stale dropdown until the next reconcile — reporting it as a send
        // failure would be far worse, because the callers answer that by
        // parking the draft as `delivery_uncertain` and telling an operator to
        // go verify delivery of a message that was, in fact, delivered.
        if let Err(e) = self.record_sent_draft_recipients(id) {
            tracing::warn!(
                draft_id = %id,
                "draft was sent, but its recipients could not be folded into the \
                 address history: {e} — they will appear in compose autocomplete \
                 after the next reconcile"
            );
        }
        Ok(())
    }

    pub fn discard_draft(&self, id: &str) -> Result<bool> {
        // `delivery_uncertain` is discardable: discard is the explicit
        // operator reconciliation exit for that terminal-recovery state (it
        // can never cause a re-send). Claimed (`sending`/`syncing`) and
        // terminal rows remain non-discardable.
        let rows = self.conn().execute(
            "UPDATE drafts SET status = 'discarded', updated_at = datetime('now')
             WHERE id = ?1
               AND status IN ('draft', 'pending_review', 'blocked', 'delivery_uncertain')",
            params![id],
        )?;
        Ok(rows > 0)
    }

    /// Replace the draft's `attachments` JSON array.
    ///
    /// For scheduled sends, attachment bytes are snapshotted at schedule time
    /// (base64-encoded inside each entry) so a later send sweep does not depend
    /// on the original files still existing. Each entry is expected to carry at
    /// least `filename`, `content_type`, and `size`; scheduled-send entries also
    /// carry `data_base64`. Never log or echo the `data_base64` field.
    pub fn update_draft_attachments(
        &self,
        id: &str,
        attachments: &[serde_json::Value],
    ) -> Result<()> {
        let serialized = serde_json::to_string(attachments)?;
        // Changing what will be attached changes what was approved and what was
        // sent: one atomic statement bumps the revision and drops both the
        // approval attestation and the human send authorization together with the
        // attachment change — and refuses claimed/terminal rows
        // (`sending`/`sent`/`discarded`) in the same predicate, so a claimed
        // transmission snapshot can never mutate under the sweep.
        let rows = self.conn().execute(
            "UPDATE drafts SET attachments = ?1,
                metadata = json_remove(metadata, '$.human_approval', '$.human_send'),
                revision = revision + 1,
                updated_at = datetime('now')
             WHERE id = ?2 AND status IN ('draft', 'pending_review', 'blocked')",
            params![serialized, id],
        )?;
        if rows == 0 {
            return Err(self.classify_guarded_update_miss(id));
        }
        Ok(())
    }

    /// Replace the draft's `attachments` JSON array, bound to the revision the
    /// caller was shown.
    ///
    /// The revision-guarded sibling of [`Self::update_draft_attachments`], for
    /// interactive surfaces where the operator is looking at a rendered list of
    /// attachments: adding or removing one is an edit to what will be sent, so
    /// it has to lose a race with a concurrent change rather than rebuild the
    /// array from a stale view. Same atomic statement, same editable-status
    /// guard, same approval/send-authorization invalidation — plus `revision = ?3`.
    pub fn update_draft_attachments_for_revision(
        &self,
        id: &str,
        expected_revision: i64,
        attachments: &[serde_json::Value],
    ) -> Result<Draft> {
        let serialized = serde_json::to_string(attachments)?;
        let rows = self.conn().execute(
            "UPDATE drafts SET attachments = ?1,
                metadata = json_remove(metadata, '$.human_approval', '$.human_send'),
                revision = revision + 1,
                updated_at = datetime('now')
             WHERE id = ?2
               AND status IN ('draft', 'pending_review', 'blocked')
               AND revision = ?3",
            params![serialized, id, expected_revision],
        )?;
        if rows == 0 {
            return Err(self.classify_guarded_update_miss(id));
        }

        self.get_draft(id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.to_string()))
    }

    /// Set the `send_after` timestamp on a draft (for scheduled sending).
    pub fn update_draft_send_after(&self, id: &str, send_after: &str) -> Result<()> {
        // Editable-status guard in the same statement: rescheduling a claimed
        // (`sending`) or terminal row is refused atomically.
        let rows = self.conn().execute(
            "UPDATE drafts SET send_after = ?1, updated_at = datetime('now')
             WHERE id = ?2 AND status IN ('draft', 'pending_review', 'blocked')",
            params![send_after, id],
        )?;
        if rows == 0 {
            return Err(self.classify_guarded_update_miss(id));
        }
        Ok(())
    }

    /// Take a queued draft back out of the outbox WITHOUT discarding it.
    ///
    /// Clears `send_after` so [`Self::list_drafts_due_for_send`] can never
    /// select the row again, and leaves it in `draft` status so the composer
    /// unlocks and the operator can edit it and re-queue it later. This is the
    /// non-destructive counterpart to [`Self::discard_draft`]: a hold loses the
    /// schedule, never the message.
    ///
    /// One guarded UPDATE. The `status = 'draft'` guard is the race boundary
    /// against the send sweep — once [`Self::claim_draft_for_sending`] has
    /// flipped the row to `sending`, this matches nothing and reports
    /// [`StoreError::DraftNotEditable`], so a transmission already in flight is
    /// never yanked back. `send_after IS NOT NULL` keeps the verb honest: a
    /// draft that was not queued reports [`StoreError::DraftNotScheduled`]
    /// instead of a silent no-op success.
    ///
    /// The human-approval attestation and the human send authorization are both
    /// stripped alongside the schedule. They authorized *this* send and the
    /// operator just withdrew it; a later re-queue re-attests through
    /// [`Self::queue_draft_with_human_send`], and an agent re-queue in between
    /// therefore finds nothing to inherit. The revision is deliberately NOT
    /// bumped — no content changed, so an editor holding `expected_revision`
    /// stays valid and can save without a spurious 409.
    ///
    /// Returns the held row.
    pub fn hold_scheduled_draft(&self, id: &str) -> Result<Draft> {
        let rows = self.conn().execute(
            "UPDATE drafts SET
                send_after = NULL,
                metadata = json_remove(
                    COALESCE(metadata, '{}'), '$.human_approval', '$.human_send'),
                updated_at = datetime('now')
             WHERE id = ?1 AND status = 'draft' AND send_after IS NOT NULL",
            params![id],
        )?;
        if rows == 0 {
            return Err(self.classify_hold_miss(id));
        }
        self.get_draft(id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.to_string()))
    }

    /// Explain why [`Self::hold_scheduled_draft`] matched zero rows, in
    /// precedence order: missing row → [`StoreError::DraftNotFound`]; a row the
    /// sweep claimed or that reached a terminal/parked state →
    /// [`StoreError::DraftNotEditable`] (the status names which); otherwise the
    /// row is a plain unqueued draft → [`StoreError::DraftNotScheduled`].
    fn classify_hold_miss(&self, id: &str) -> StoreError {
        match self.get_draft(id) {
            Ok(None) => StoreError::DraftNotFound(id.to_string()),
            Ok(Some(current)) if current.status != DraftStatus::Draft => {
                StoreError::DraftNotEditable(current.status.as_str().to_string())
            }
            Ok(Some(_)) => StoreError::DraftNotScheduled(id.to_string()),
            Err(e) => e,
        }
    }

    /// Atomically claim a due scheduled draft for transmission.
    ///
    /// One CAS UPDATE: only a `draft`-status row at exactly `expected_revision`
    /// whose `send_after` is due transitions to the durable `sending` claim
    /// state. Returns `false` when another sweeper already claimed it, a
    /// concurrent edit bumped the revision, the draft was blocked/discarded/
    /// approved away, or it is no longer due. While claimed, the row is
    /// invisible to [`Self::list_drafts_due_for_send`] (`status = 'draft'`
    /// only), so a crash or a later local DB failure can at worst strand it as
    /// `sending` — visible, inert, never editable, and never re-selected for a
    /// duplicate transmission.
    /// Returns the opaque owner lease token on success: mark-sent, release,
    /// and park all require id + this token, so a non-owner (another sweeper,
    /// an immediate send, a stale actor) can neither finalize nor release the
    /// claim.
    pub fn claim_draft_for_sending(
        &self,
        id: &str,
        expected_revision: i64,
    ) -> Result<Option<String>> {
        let token = Uuid::new_v4().to_string();
        let rows = self.conn().execute(
            "UPDATE drafts SET status = 'sending', operation_token = ?3,
                updated_at = datetime('now')
             WHERE id = ?1 AND revision = ?2 AND status = 'draft'
               AND send_after IS NOT NULL
               AND datetime(send_after) <= datetime('now')",
            params![id, expected_revision, token],
        )?;
        Ok((rows == 1).then_some(token))
    }

    /// Atomically claim a draft for an immediate (CLI/MCP) send.
    ///
    /// Same durable `sending` claim the scheduled sweep uses, without the
    /// due-`send_after` requirement: only a `draft`-status row at exactly
    /// `expected_revision` can be claimed, so an immediate send loses against
    /// an in-flight sweep claim, a provider sync, a concurrent edit (stale
    /// revision), or any non-`draft` state — instead of double-sending or
    /// transmitting a stale snapshot.
    /// Returns the opaque owner lease token on success (see
    /// [`Self::claim_draft_for_sending`]).
    pub fn claim_draft_for_immediate_send(
        &self,
        id: &str,
        expected_revision: i64,
    ) -> Result<Option<String>> {
        let token = Uuid::new_v4().to_string();
        let rows = self.conn().execute(
            "UPDATE drafts SET status = 'sending', operation_token = ?3,
                updated_at = datetime('now')
             WHERE id = ?1 AND revision = ?2 AND status = 'draft'",
            params![id, expected_revision, token],
        )?;
        Ok((rows == 1).then_some(token))
    }

    /// Atomically claim a draft for a provider-mailbox sync (`modify_draft`
    /// replacing the server-side copy). Returns the status the draft held
    /// before the claim (to restore on release), or `None` when the claim is
    /// lost: revision changed, or the draft is not in an editable state
    /// (already `sending`/`syncing`/terminal). While `syncing`, the send
    /// sweep cannot claim the row (it claims only `status='draft'`), other
    /// actors cannot mutate it, and a crash leaves it inert.
    pub fn claim_draft_for_sync(
        &self,
        id: &str,
        expected_revision: i64,
    ) -> Result<Option<SyncClaim>> {
        let tx = self.conn().unchecked_transaction()?;
        let Some(current) = self.get_draft(id)? else {
            return Err(StoreError::DraftNotFound(id.to_string()));
        };
        if !current.status.is_editable() || current.revision != expected_revision {
            return Ok(None);
        }
        let token = Uuid::new_v4().to_string();
        let rows = self.conn().execute(
            "UPDATE drafts SET status = 'syncing', operation_token = ?3,
                updated_at = datetime('now')
             WHERE id = ?1 AND revision = ?2
               AND status IN ('draft', 'pending_review', 'blocked')",
            params![id, expected_revision, token],
        )?;
        if rows == 0 {
            return Ok(None);
        }
        tx.commit()?;
        Ok(Some(SyncClaim {
            token,
            prior_status: current.status,
        }))
    }

    /// True while `token` still owns the `syncing` lease on this draft.
    /// Holders recheck this immediately before every destructive provider
    /// side effect (old-copy delete, replacement APPEND).
    pub fn holds_sync_claim(&self, id: &str, token: &str) -> Result<bool> {
        let count: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM drafts
             WHERE id = ?1 AND status = 'syncing' AND operation_token = ?2",
            params![id, token],
            |row| row.get(0),
        )?;
        Ok(count == 1)
    }

    /// Apply a modify's full local edit — content, recipients, attachments,
    /// and metadata — as ONE atomic statement under the held sync lease.
    /// Nothing lands unless `token` still owns the `syncing` claim, and the
    /// statement bumps the revision and strips any approval attestation, so
    /// no partially updated draft is ever observable or claimable.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_synced_draft_edit(
        &self,
        id: &str,
        token: &str,
        to_addr: &str,
        cc_addr: Option<&str>,
        bcc_addr: Option<&str>,
        subject: &str,
        text_content: &str,
        html_content: Option<&str>,
        attachments: &[serde_json::Value],
        metadata: &serde_json::Value,
    ) -> Result<Draft> {
        let mut sanitized = metadata.clone();
        if let Some(obj) = sanitized.as_object_mut() {
            obj.remove("human_approval");
            obj.remove("human_send");
        }
        let serialized_meta = serde_json::to_string(&sanitized)?;
        let serialized_attachments = serde_json::to_string(attachments)?;
        let rows = self.conn().execute(
            "UPDATE drafts SET
                to_addr = ?1, cc_addr = ?2, bcc_addr = ?3, subject = ?4,
                text_content = ?5, html_content = ?6, attachments = ?7,
                metadata = ?8, revision = revision + 1, updated_at = datetime('now')
             WHERE id = ?9 AND status = 'syncing' AND operation_token = ?10",
            params![
                to_addr,
                cc_addr,
                bcc_addr,
                subject,
                text_content,
                html_content,
                serialized_attachments,
                serialized_meta,
                id,
                token
            ],
        )?;
        if rows == 0 {
            return Err(self.classify_guarded_update_miss(id));
        }
        self.get_draft(id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.to_string()))
    }

    /// Finalize provider-sync bookkeeping (replacement UID, Message-ID,
    /// storage metadata) — permitted ONLY to the lease holder.
    pub fn finalize_synced_draft_bookkeeping(
        &self,
        id: &str,
        token: &str,
        imap_uid: Option<u32>,
        storage_metadata: &serde_json::Value,
    ) -> Result<()> {
        let current = self
            .get_draft(id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.to_string()))?;
        let mut metadata = match current.metadata {
            Some(m) if m.is_object() => m,
            _ => serde_json::json!({}),
        };
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert("storage".to_string(), storage_metadata.clone());
        }
        let serialized = serde_json::to_string(&metadata)?;
        let rows = self.conn().execute(
            "UPDATE drafts SET
                imap_uid = COALESCE(?1, imap_uid),
                metadata = ?2,
                updated_at = datetime('now')
             WHERE id = ?3 AND status = 'syncing' AND operation_token = ?4",
            params![imap_uid.map(|u| u as i64), serialized, id, token],
        )?;
        if rows == 0 {
            return Err(self.classify_guarded_update_miss(id));
        }
        Ok(())
    }

    /// Release a provider-sync claim back to `to` (normally the prior status
    /// returned by [`Self::claim_draft_for_sync`]). Requires the owner lease
    /// token and clears it; a non-owner (or an unclaimed row) matches nothing
    /// and changes nothing.
    pub fn release_syncing_draft(&self, id: &str, token: &str, to: DraftStatus) -> Result<bool> {
        let rows = self.conn().execute(
            "UPDATE drafts SET status = ?1, operation_token = NULL,
                updated_at = datetime('now')
             WHERE id = ?2 AND status = 'syncing' AND operation_token = ?3",
            params![to.as_str(), id, token],
        )?;
        Ok(rows == 1)
    }

    /// Park a `sending` claim as `delivery_uncertain`: SMTP was ACCEPTED but
    /// the local sent-state persistence failed. One atomic statement under
    /// the owner lease clears `send_after` (nothing can select it as due),
    /// clears the token, and enters the terminal-recovery state — which is
    /// non-editable, non-approvable, non-queueable, and non-claimable, so no
    /// approval or sweep can ever re-send delivered mail. Recovery is an
    /// explicit operator reconciliation (verify delivery, then discard).
    pub fn park_delivery_uncertain(&self, id: &str, token: &str) -> Result<bool> {
        let rows = self.conn().execute(
            "UPDATE drafts SET status = 'delivery_uncertain', send_after = NULL,
                operation_token = NULL, updated_at = datetime('now')
             WHERE id = ?1 AND status = 'sending' AND operation_token = ?2",
            params![id, token],
        )?;
        Ok(rows == 1)
    }

    /// Persist a bot's validated attribution declaration onto a queued/scheduled
    /// draft, merged under the `attribution` metadata key.
    ///
    /// The block is bound to the row's authoritative `revision` (stamped from the
    /// column in the same statement), so any later material edit — which bumps the
    /// revision — makes it stale and the scheduled-send sweep treats it as no
    /// declaration at all. All other metadata keys (reply threading, contextual
    /// state, storage, human approval) are preserved via `json_set` rather than a
    /// blob replace, and the revision is NOT bumped so the declaration stays
    /// current for the sweep. The `attribution` value is caller-supplied
    /// (`PersistedDeclaration::to_value`) and contains no score/weight/threshold.
    ///
    /// Editable-status guard: a claimed (`sending`/`syncing`) or terminal row
    /// refuses atomically. The attribution wire key mirrors the transport crate's
    /// `ATTRIBUTION_METADATA_KEY`.
    pub fn set_draft_attribution(&self, id: &str, attribution: &serde_json::Value) -> Result<()> {
        let serialized = serde_json::to_string(attribution)?;
        let rows = self.conn().execute(
            "UPDATE drafts SET
                metadata = json_set(
                    COALESCE(metadata, '{}'),
                    '$.attribution',
                    json_set(json(?1), '$.revision', revision)
                ),
                updated_at = datetime('now')
             WHERE id = ?2
               AND status IN ('draft', 'pending_review', 'blocked')",
            params![serialized, id],
        )?;
        if rows == 0 {
            return Err(self.classify_guarded_update_miss(id));
        }
        Ok(())
    }

    /// Atomically queue a draft for scheduled sending in ONE compare-and-set:
    /// validate the row is still at `expected_revision` and in a queueable
    /// (editable) status, persist the already-validated attribution declaration
    /// bound to that revision, set the `send_after` schedule, and transition the
    /// row to the due `draft` status — all together or not at all.
    ///
    /// This is the single atomic replacement for the previous
    /// `update_draft_send_after` + `set_draft_attribution` sequence, and it closes
    /// three races that sequence allowed:
    ///
    /// - A concurrent material edit after the caller validated its declaration
    ///   bumps the revision; the CAS then matches zero rows and refuses, so a
    ///   stale declaration can never be stamped onto newer content.
    /// - Scheduling and attribution persistence are one statement, so a failure
    ///   can never strand a `send_after` with no declaration (or vice versa).
    /// - A `pending_review` row is transitioned to `draft`, so a valid re-queue
    ///   is actually visible to [`Self::list_drafts_due_for_send`] (which selects
    ///   `status='draft'`) instead of being reported "scheduled" while remaining
    ///   invisible to the sweep. A `blocked` (Governor-denied) row is deliberately
    ///   NOT queueable (see [`DraftStatus::is_queueable`]): it refuses so a denied
    ///   send can never be resurrected into a due draft.
    ///
    /// The revision is deliberately NOT bumped: the persisted declaration must
    /// stay current for the sweep, which claims at this same revision. A stale
    /// `expected_revision`, a claimed/terminal/blocked row, or a missing draft
    /// returns the classified miss (`DraftModifiedConcurrently` /
    /// `DraftNotEditable` / `DraftNotFound`) and nothing is scheduled or
    /// persisted. The `attribution` value is caller-supplied
    /// (`PersistedDeclaration::to_value`) and carries no score/weight/threshold.
    ///
    /// This is the AGENT queue transition (CLI `draft send`, MCP `send_draft`),
    /// so the same statement strips any `human_send` authorization left by an
    /// earlier dashboard **Human-only Send**. The pending send now belongs to the
    /// agent — it is the agent's declaration the sweep will judge, and the human
    /// never authorized *this* transmission. Without the strip, a human's click
    /// (or a click the agent could provoke) would carry an agent-queued send past
    /// the Governor gate. The `human_approval` review attestation is deliberately
    /// left alone: a human really did approve this revision, and that fact is a
    /// declared attribute for scoring, never a bypass.
    pub fn queue_draft_for_send(
        &self,
        id: &str,
        expected_revision: i64,
        send_after: &str,
        attribution: &serde_json::Value,
    ) -> Result<()> {
        let serialized = serde_json::to_string(attribution)?;
        let rows = self.conn().execute(
            "UPDATE drafts SET
                status = 'draft',
                send_after = ?1,
                metadata = json_remove(
                    json_set(
                        COALESCE(metadata, '{}'),
                        '$.attribution',
                        json_set(json(?2), '$.revision', revision)
                    ),
                    '$.human_send'
                ),
                updated_at = datetime('now')
             WHERE id = ?3 AND revision = ?4
               AND status IN ('draft', 'pending_review')",
            params![send_after, serialized, id, expected_revision],
        )?;
        if rows == 0 {
            return Err(self.classify_queue_miss(id));
        }
        Ok(())
    }

    /// Record a failed SMTP-time attribution attempt and leave the draft due for a
    /// later sweep to retry. Under the owner `sending` lease: back to `draft`,
    /// clear the token, merge the advanced attempt state under `attribution`
    /// (revision re-stamped from the column, which is unchanged since the claim),
    /// and DO NOT touch `send_after` (the draft stays due) or bump the revision.
    /// A non-owner or non-`sending` row matches nothing and returns `false`.
    pub fn defer_attribution_retry(
        &self,
        id: &str,
        token: &str,
        attribution: &serde_json::Value,
    ) -> Result<bool> {
        let serialized = serde_json::to_string(attribution)?;
        let rows = self.conn().execute(
            "UPDATE drafts SET
                status = 'draft', operation_token = NULL,
                metadata = json_set(
                    COALESCE(metadata, '{}'),
                    '$.attribution',
                    json_set(json(?1), '$.revision', revision)
                ),
                updated_at = datetime('now')
             WHERE id = ?2 AND status = 'sending' AND operation_token = ?3",
            params![serialized, id, token],
        )?;
        Ok(rows == 1)
    }

    /// Park a draft that exhausted its attribution correction attempts.
    ///
    /// Under the owner `sending` lease: move to `pending_review`, clear
    /// `send_after` (disable automatic transmission scheduling — the due query
    /// never selects it again, so there is no retry storm), clear the token, and
    /// merge the terminal attempt state (with `park_reason`) under `attribution`.
    /// The revision is NOT bumped, so the pending-review draft carries an honest
    /// record of why it parked. A non-owner or non-`sending` row changes nothing.
    pub fn park_attribution_exhausted(
        &self,
        id: &str,
        token: &str,
        attribution: &serde_json::Value,
    ) -> Result<bool> {
        let serialized = serde_json::to_string(attribution)?;
        let rows = self.conn().execute(
            "UPDATE drafts SET
                status = 'pending_review', send_after = NULL, operation_token = NULL,
                metadata = json_set(
                    COALESCE(metadata, '{}'),
                    '$.attribution',
                    json_set(json(?1), '$.revision', revision)
                ),
                updated_at = datetime('now')
             WHERE id = ?2 AND status = 'sending' AND operation_token = ?3",
            params![serialized, id, token],
        )?;
        Ok(rows == 1)
    }

    /// Park a `sending` claim as `pending_review` after a durable Governor
    /// **review** verdict (the scheduled send routed review, not allow).
    ///
    /// Under the owner `sending` lease, one atomic statement: move to
    /// `pending_review`, clear `send_after` (so no surface — dashboard, CLI, or
    /// the due query — can present the parked draft as still queued or show a
    /// stale countdown), and clear the token. The reviewable body/attachment
    /// snapshot, metadata (including the persisted declaration), and revision are
    /// preserved untouched, so the human decision path (approve / edit / discard /
    /// send) still works. Nothing is transmitted and no Sent copy is written. A
    /// non-owner or non-`sending` row changes nothing and returns `false`.
    ///
    /// Distinct from [`Self::release_sending_draft`] into `pending_review`, which
    /// left `send_after` intact — the exact defect that made a parked-for-review
    /// draft read as "Queued for sending".
    pub fn park_for_review(&self, id: &str, token: &str) -> Result<bool> {
        self.park_for_review_with_block(id, token, &Self::default_send_block())
    }

    /// Same lease park as [`Self::park_for_review`], and persist a user-facing
    /// `metadata.send_block` so no surface can show a silent stop. The payload
    /// is caller-supplied (code/title/explanation/action only — never scores,
    /// recipients, or body).
    pub fn park_for_review_with_block(
        &self,
        id: &str,
        token: &str,
        block: &serde_json::Value,
    ) -> Result<bool> {
        let serialized = serde_json::to_string(block)?;
        let rows = self.conn().execute(
            "UPDATE drafts SET status = 'pending_review', send_after = NULL,
                operation_token = NULL,
                metadata = json_set(COALESCE(metadata, '{}'), '$.send_block', json(?3)),
                updated_at = datetime('now')
             WHERE id = ?1 AND status = 'sending' AND operation_token = ?2",
            params![id, token, serialized],
        )?;
        Ok(rows == 1)
    }

    /// Operator-facing stop record when a caller parks without a richer reason.
    /// A silent `pending_review` badge is not allowed.
    pub fn default_send_block() -> serde_json::Value {
        serde_json::json!({
            "code": "send_stopped",
            "title": "This send was stopped",
            "explanation": "Envelope paused this message before it left. Nothing was transmitted.",
            "action": "send"
        })
    }

    /// Record the resolved Sent-folder copy proof on an already-`sent` draft.
    ///
    /// Best-effort bookkeeping run after [`Self::mark_draft_sent`]. The Sent-copy
    /// proof is stored ONLY under the dedicated, folder-qualified
    /// `metadata.sent_copy` object — `folder`, `uid` (explicit JSON `null` when
    /// unresolved, never a retained stale value), `lookup_status`, and the stable
    /// `copy_source` label (`provider` / `client_appended` / `unresolved` /
    /// `not_attempted`). It never writes the Drafts-folder `imap_uid` column, so a
    /// Sent UID is never presented as a Drafts UID. Arguments are primitives (not
    /// a transport `SentMailProof`) so there is no store→email dependency cycle;
    /// the caller passes the truthful `copy_source` from the source-aware
    /// resolver, so a client-appended copy is never recorded as provider proof.
    /// Guarded on the terminal `sent` status; a non-`sent`/missing row returns
    /// `false`.
    pub fn record_sent_copy_proof(
        &self,
        id: &str,
        folder: Option<&str>,
        uid: Option<u32>,
        lookup_status: &str,
        copy_source: &str,
    ) -> Result<bool> {
        let rows = self.conn().execute(
            "UPDATE drafts SET
                metadata = json_set(
                    COALESCE(metadata, '{}'),
                    '$.sent_copy',
                    json_object(
                        'folder', ?1,
                        'uid', ?2,
                        'lookup_status', ?3,
                        'copy_source', ?4
                    )
                ),
                updated_at = datetime('now')
             WHERE id = ?5 AND status = 'sent'",
            params![folder, uid, lookup_status, copy_source, id],
        )?;
        Ok(rows == 1)
    }

    /// Release a `sending` claim into `to`: `draft` to retry on a later sweep
    /// (transient failure), or `pending_review`/`blocked` to park it for
    /// explicit human action. Guarded on the current status being `sending`,
    /// so it can never clobber a terminal or operator-set state; releasing a
    /// row that is not claimed returns `false` and changes nothing. Successful
    /// sends leave the claim through [`Self::mark_draft_sent`] instead — a
    /// transmitted draft must never be released back to due.
    pub fn release_sending_draft(&self, id: &str, token: &str, to: DraftStatus) -> Result<bool> {
        let rows = self.conn().execute(
            "UPDATE drafts SET status = ?1, operation_token = NULL,
                updated_at = datetime('now')
             WHERE id = ?2 AND status = 'sending' AND operation_token = ?3",
            params![to.as_str(), id, token],
        )?;
        Ok(rows == 1)
    }

    /// Query drafts that are due for scheduled sending.
    pub fn list_drafts_due_for_send(&self) -> Result<Vec<Draft>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, status, to_addr, cc_addr, bcc_addr, reply_to, subject,
                    text_content, html_content, in_reply_to, metadata, attachments, message_id,
                    send_after, snoozed_until, created_at, updated_at, sent_at, created_by,
                    imap_uid, revision
             FROM drafts
             WHERE status = 'draft'
               AND send_after IS NOT NULL
               AND datetime(send_after) <= datetime('now')
             ORDER BY send_after ASC",
        )?;

        let rows = stmt.query_map([], Self::map_draft)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Store the IMAP UID assigned by the server after APPEND to the Drafts folder.
    pub fn update_draft_imap_uid(&self, id: &str, imap_uid: u32) -> Result<()> {
        // IMAP bookkeeping is written on editable rows only (post-APPEND sync
        // of a fresh draft) — never on a claimed (`sending`/`syncing`) or
        // terminal row. The sync-claim holder finalizes through the
        // token-checked `finalize_synced_draft_bookkeeping` instead.
        let rows = self.conn().execute(
            "UPDATE drafts SET imap_uid = ?1, updated_at = datetime('now')
             WHERE id = ?2
               AND status IN ('draft', 'pending_review', 'blocked')",
            params![imap_uid as i64, id],
        )?;
        if rows == 0 {
            return Err(self.classify_guarded_update_miss(id));
        }
        Ok(())
    }

    /// Count drafts with actionable/pending status: `draft` or `pending_review`.
    /// Pass `None` to count across all accounts.
    pub fn count_active_drafts(&self, account_id: Option<&str>) -> Result<u64> {
        let count: i64 = if let Some(aid) = account_id {
            self.conn().query_row(
                "SELECT COUNT(*) FROM drafts
                 WHERE account_id = ?1 AND status IN ('draft', 'pending_review')",
                params![aid],
                |row| row.get(0),
            )?
        } else {
            self.conn().query_row(
                "SELECT COUNT(*) FROM drafts WHERE status IN ('draft', 'pending_review')",
                [],
                |row| row.get(0),
            )?
        };
        Ok(count as u64)
    }

    /// Replace the draft's `metadata` JSON blob.
    ///
    /// Used to persist contextual reply/forward state (draft_kind, source
    /// folder/uid/message_id, references, quote/forward block, signature state,
    /// preview metadata) so the full draft can be reconstructed and sent later
    /// without the original message in context.
    ///
    /// Any `human_approval` or `human_send` key in the incoming value is
    /// stripped: a metadata write is a draft-revision change (the revision
    /// counter is bumped in the same statement), and both the approval and the
    /// human send authorization are bound to the revision the human acted on.
    /// Callers (including agent/MCP paths doing read-modify-write) can neither
    /// inject either record nor carry one forward through this path — only
    /// [`Self::record_draft_human_approval`] and
    /// [`Self::queue_draft_with_human_send`] write those keys.
    pub fn set_draft_metadata(&self, id: &str, metadata: &serde_json::Value) -> Result<()> {
        let mut sanitized = metadata.clone();
        if let Some(obj) = sanitized.as_object_mut() {
            obj.remove("human_approval");
            obj.remove("human_send");
        }
        let serialized = serde_json::to_string(&sanitized)?;
        // Status guard in the same statement: metadata (threading, contextual
        // state) is part of what gets transmitted, so a claimed
        // (`sending`/`syncing`) or terminal row must refuse it atomically —
        // the sync-claim holder writes through the token-checked
        // `apply_synced_draft_edit`/`finalize_synced_draft_bookkeeping`
        // variants instead.
        let rows = self.conn().execute(
            "UPDATE drafts SET metadata = ?1, revision = revision + 1,
                updated_at = datetime('now')
             WHERE id = ?2
               AND status IN ('draft', 'pending_review', 'blocked')",
            params![serialized, id],
        )?;
        if rows == 0 {
            return Err(self.classify_guarded_update_miss(id));
        }
        Ok(())
    }

    /// Durably record a sanitized human-approval attestation, compare-and-set
    /// against the exact draft revision the human acted on.
    ///
    /// `expected_revision` is the [`Draft::revision`] the caller read when the
    /// human reviewed the draft. The attestation (surface label, RFC 3339 UTC
    /// timestamp, and that revision — never an address, token, or secret) is
    /// written with `WHERE revision = expected_revision`, so a concurrent
    /// content-relevant edit — before or during this call — makes the write
    /// match zero rows and returns [`StoreError::DraftModifiedConcurrently`]
    /// instead of letting the new content inherit the approval. Only human
    /// surfaces may call this; agent/MCP paths must never write the
    /// attestation.
    ///
    /// Idempotent per revision: a draft already carrying a valid attestation
    /// for `expected_revision` is left untouched (repeat approvals do not
    /// rewrite the stamp). A fresh stamp lands only when a content edit
    /// invalidated the previous one — under a fresh `expected_revision`.
    pub fn record_draft_human_approval(
        &self,
        id: &str,
        expected_revision: i64,
        approved_by: &str,
        approved_at: &str,
    ) -> Result<()> {
        let draft = self
            .get_draft(id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.to_string()))?;
        if draft.revision != expected_revision {
            return Err(StoreError::DraftModifiedConcurrently(id.to_string()));
        }
        if draft.human_approved() {
            return Ok(());
        }
        let mut metadata = match draft.metadata {
            Some(m) if m.is_object() => m,
            _ => serde_json::json!({}),
        };
        metadata["human_approval"] = serde_json::json!({
            "approved_by": approved_by,
            "approved_at": approved_at,
            "revision": expected_revision,
        });
        let serialized = serde_json::to_string(&metadata)?;
        // The CAS clause is the atomic guard: if any content-relevant mutation
        // bumped the revision — or the sweep claimed the row / it reached a
        // terminal state — between the read above and this write, zero rows
        // match and no stale approval is persisted.
        let rows = self.conn().execute(
            "UPDATE drafts SET metadata = ?1, updated_at = datetime('now')
             WHERE id = ?2 AND revision = ?3
               AND status IN ('draft', 'pending_review', 'blocked')",
            params![serialized, id, expected_revision],
        )?;
        if rows == 0 {
            return Err(self.classify_guarded_update_miss(id));
        }
        Ok(())
    }

    /// Record the revision-bound human **send** authorization, compare-and-set
    /// against the revision the human acted on.
    ///
    /// Private by design, and called only from
    /// [`Self::queue_draft_with_human_send`] inside that method's transaction:
    /// the authorization exists only as part of a queue transition a human
    /// surface performed, never as free-standing state some later path could
    /// pick up. `queued_by` is the human surface label (`human:*`), `queued_at`
    /// an RFC 3339 UTC timestamp — never an address, token, or secret.
    ///
    /// The CAS clause is the guard: a concurrent content-relevant edit, a sweep
    /// claim, or a terminal transition between the caller's read and this write
    /// matches zero rows, so no send is ever authorized against content the
    /// human did not see.
    fn record_draft_human_send(
        &self,
        id: &str,
        expected_revision: i64,
        queued_by: &str,
        queued_at: &str,
    ) -> Result<()> {
        let draft = self
            .get_draft(id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.to_string()))?;
        if draft.revision != expected_revision {
            return Err(StoreError::DraftModifiedConcurrently(id.to_string()));
        }
        let mut metadata = match draft.metadata {
            Some(m) if m.is_object() => m,
            _ => serde_json::json!({}),
        };
        metadata["human_send"] = serde_json::json!({
            "queued_by": queued_by,
            "queued_at": queued_at,
            "revision": expected_revision,
        });
        let serialized = serde_json::to_string(&metadata)?;
        let rows = self.conn().execute(
            "UPDATE drafts SET metadata = ?1, updated_at = datetime('now')
             WHERE id = ?2 AND revision = ?3
               AND status IN ('draft', 'pending_review', 'blocked')",
            params![serialized, id, expected_revision],
        )?;
        if rows == 0 {
            return Err(self.classify_guarded_update_miss(id));
        }
        Ok(())
    }

    /// Atomically approve a draft revision from a human surface: promote it to
    /// sweep-eligible `draft` status and record the revision-bound attestation
    /// in one transaction. `expected_revision` is the revision the human
    /// **viewed** (carried by the browser request, never re-read server-side)
    /// — if the content changed since, the whole operation (including the
    /// status flip) rolls back with [`StoreError::DraftModifiedConcurrently`].
    ///
    /// Review only: it sets no `send_after` and writes no `human_send`
    /// authorization, so approving a draft neither sends it nor lets a later
    /// agent send skip the Governor gate. Sending from a human surface is
    /// [`Self::queue_draft_with_human_send`].
    pub fn approve_draft_revision(
        &self,
        id: &str,
        expected_revision: i64,
        approved_by: &str,
        approved_at: &str,
    ) -> Result<Draft> {
        let tx = self.conn().unchecked_transaction()?;
        self.update_draft_status(id, DraftStatus::Draft)?;
        self.record_draft_human_approval(id, expected_revision, approved_by, approved_at)?;
        tx.commit()?;
        self.get_draft(id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.to_string()))
    }

    /// Atomically queue a **human-sent** draft into the outbox: validate the
    /// draft is queueable (`draft` stays, `pending_review` is promoted;
    /// sending/blocked/discarded/sent refuse — those are in-flight, explicit
    /// operator decisions, or terminal states), set `send_after`, record the
    /// review attestation, and record the human send authorization — all bound
    /// to `expected_revision`, the revision the human **viewed**, and all in one
    /// transaction. If any step fails (e.g. a concurrent content edit), nothing
    /// persists: no partially queued state, and no authorization for a send that
    /// was never queued.
    ///
    /// This is the ONLY writer of the `human_send` authorization
    /// ([`Draft::human_send_surface`]), and it writes it only *as* the queue
    /// transition. That is what makes the authorization mean "this human queued
    /// this exact revision from this surface" rather than the much weaker "some
    /// human approved this content at some point" — the distinction the sweep's
    /// Human-only Send gate exception depends on. Callers must pass a
    /// `human:`-prefixed surface for their own send action; an agent-facing
    /// re-queue belongs on [`Self::queue_draft_for_send`], which strips the
    /// authorization instead.
    ///
    /// Returns the exact atomically-queued, authorized [`Draft`] row (read inside
    /// the same transaction before commit) so the caller never has to reload —
    /// a post-commit reload could fail or race and let a handler report success
    /// off pre-attestation state. `blocked` is refused ([`DraftStatus::is_queueable`]
    /// is false): a Governor-denied row is never resurrected by human queueing.
    pub fn queue_draft_with_human_send(
        &self,
        id: &str,
        expected_revision: i64,
        send_after: &str,
        approved_by: &str,
        approved_at: &str,
    ) -> Result<Draft> {
        let tx = self.conn().unchecked_transaction()?;
        let current = self
            .get_draft(id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.to_string()))?;
        match current.status {
            DraftStatus::Draft => {}
            DraftStatus::PendingReview => {
                self.update_draft_status(id, DraftStatus::Draft)?;
            }
            DraftStatus::Sending
            | DraftStatus::Syncing
            | DraftStatus::Blocked
            | DraftStatus::DeliveryUncertain
            | DraftStatus::Discarded
            | DraftStatus::Sent => {
                return Err(StoreError::DraftNotEditable(
                    current.status.as_str().to_string(),
                ));
            }
        }
        self.update_draft_send_after(id, send_after)?;
        self.record_draft_human_approval(id, expected_revision, approved_by, approved_at)?;
        self.record_draft_human_send(id, expected_revision, approved_by, approved_at)?;
        // Read the attested row inside the transaction, then commit. The caller
        // receives exactly what was queued — no reload, no fallback.
        let attested = self
            .get_draft(id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.to_string()))?;
        tx.commit()?;
        Ok(attested)
    }

    /// Store the RFC822 Message-ID for a draft (set during IMAP APPEND).
    pub fn mark_draft_message_id(&self, id: &str, message_id: &str) -> Result<()> {
        // Same ownership rule as `update_draft_imap_uid`: the Message-ID is
        // the provider-cleanup identity, so it may only change on editable
        // rows — claimed and terminal rows refuse.
        let rows = self.conn().execute(
            "UPDATE drafts SET message_id = ?1, updated_at = datetime('now')
             WHERE id = ?2
               AND status IN ('draft', 'pending_review', 'blocked')",
            params![message_id, id],
        )?;
        if rows == 0 {
            return Err(self.classify_guarded_update_miss(id));
        }
        Ok(())
    }

    pub(crate) fn map_draft(row: &rusqlite::Row<'_>) -> rusqlite::Result<Draft> {
        let status_str: String = row.get(2)?;
        let metadata_str: Option<String> = row.get(11)?;
        let attachments_str: String = row.get(12)?;
        let imap_uid_i64: Option<i64> = row.get(20)?;
        Ok(Draft {
            id: row.get(0)?,
            account_id: row.get(1)?,
            status: status_str.parse().unwrap_or(DraftStatus::Draft),
            to_addr: row.get(3)?,
            cc_addr: row.get(4)?,
            bcc_addr: row.get(5)?,
            reply_to: row.get(6)?,
            subject: row.get(7)?,
            text_content: row.get(8)?,
            html_content: row.get(9)?,
            in_reply_to: row.get(10)?,
            metadata: metadata_str.and_then(|s| serde_json::from_str(&s).ok()),
            attachments: serde_json::from_str(&attachments_str).unwrap_or_default(),
            message_id: row.get(13)?,
            send_after: row.get(14)?,
            snoozed_until: row.get(15)?,
            created_at: row.get(16)?,
            updated_at: row.get(17)?,
            sent_at: row.get(18)?,
            created_by: row.get(19)?,
            imap_uid: imap_uid_i64.map(|v| v as u32),
            revision: row.get(21)?,
        })
    }
}

trait OptionalExt<T> {
    fn optional(self) -> std::result::Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalExt<T> for std::result::Result<T, rusqlite::Error> {
    fn optional(self) -> std::result::Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(val) => Ok(Some(val)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn setup() -> Database {
        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acc1', 'Test', 'test@test.com', 'test.com', 'smtp.test.com', 587,
                         'imap.test.com', 993, 'encrypted')",
                [],
            )
            .unwrap();
        db
    }

    fn attribution_of(db: &Database, id: &str) -> serde_json::Value {
        db.get_draft(id)
            .unwrap()
            .unwrap()
            .metadata
            .unwrap()
            .get("attribution")
            .cloned()
            .unwrap()
    }

    #[test]
    fn set_draft_attribution_merges_without_clobber_and_stamps_revision() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        // Pre-existing reply/threading metadata that must survive.
        db.set_draft_metadata(
            &draft.id,
            &serde_json::json!({"draft_kind": "reply", "in_reply_to": "<parent@x>"}),
        )
        .unwrap();
        let rev_before = db.get_draft(&draft.id).unwrap().unwrap().revision;

        db.set_draft_attribution(
            &draft.id,
            &serde_json::json!({
                "protocol": "envelope.attribution.v1",
                "origin": "bot",
                "declared_attrs": ["financial_content"],
                "revision": 999,       // must be overridden by the column value
                "attempts": 0,
            }),
        )
        .unwrap();

        let reloaded = db.get_draft(&draft.id).unwrap().unwrap();
        // Persisting attribution must NOT bump the revision (declaration stays current).
        assert_eq!(reloaded.revision, rev_before);
        let meta = reloaded.metadata.unwrap();
        // Sibling keys preserved.
        assert_eq!(meta["draft_kind"], "reply");
        assert_eq!(meta["in_reply_to"], "<parent@x>");
        // Attribution merged, revision stamped from the column (not the 999 passed).
        assert_eq!(
            meta["attribution"]["declared_attrs"][0],
            "financial_content"
        );
        assert_eq!(meta["attribution"]["revision"], rev_before);
    }

    fn bot_attribution(declared: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "protocol": "envelope.attribution.v1",
            "origin": "bot",
            "declared_attrs": declared,
            "attempts": 0,
        })
    }

    // ── Block 2: one atomic CAS for declaration + schedule + due status ──────

    #[test]
    fn queue_draft_for_send_binds_declaration_schedule_and_due_status_atomically() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("S"),
                Some("B"),
                None,
                None,
                None,
                None,
                Some("mcp"),
            )
            .unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        let past = "2000-01-01T00:00:00Z";
        db.queue_draft_for_send(
            &draft.id,
            rev,
            past,
            &bot_attribution(&["financial_content"]),
        )
        .unwrap();

        let reloaded = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(
            reloaded.status,
            DraftStatus::Draft,
            "queued row is due-status"
        );
        assert_eq!(reloaded.send_after.as_deref(), Some(past));
        assert_eq!(
            reloaded.revision, rev,
            "queueing must not bump the revision"
        );
        let attr = reloaded.metadata.unwrap()["attribution"].clone();
        assert_eq!(attr["declared_attrs"][0], "financial_content");
        assert_eq!(attr["revision"], rev, "declaration bound to this revision");
        // Visible to the sweep's due query.
        let due = db.list_drafts_due_for_send().unwrap();
        assert!(due.iter().any(|d| d.id == draft.id));
    }

    #[test]
    fn queue_draft_for_send_refuses_stale_revision_with_no_partial_write() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("S"),
                Some("B"),
                None,
                None,
                None,
                None,
                Some("mcp"),
            )
            .unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        // Caller's expected revision is stale (off by one).
        let err = db
            .queue_draft_for_send(
                &draft.id,
                rev + 1,
                "2000-01-01T00:00:00Z",
                &bot_attribution(&["financial_content"]),
            )
            .unwrap_err();
        assert!(
            matches!(err, StoreError::DraftModifiedConcurrently(_)),
            "stale revision must conflict, got {err:?}"
        );
        // Nothing partial: no schedule, no attribution.
        let reloaded = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(reloaded.send_after.is_none(), "no send_after was written");
        assert!(
            reloaded
                .metadata
                .and_then(|m| m.get("attribution").cloned())
                .is_none(),
            "no attribution was written"
        );
    }

    #[test]
    fn queue_draft_for_send_no_clobber_when_content_edited_after_validation() {
        // The declaration was validated against revision R. A concurrent material
        // edit bumps to R+1 before queueing; queueing at R must refuse rather than
        // bind the stale declaration to the edited content.
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("S"),
                Some("B"),
                None,
                None,
                None,
                None,
                Some("mcp"),
            )
            .unwrap();
        let validated_rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        db.update_draft_content(
            &draft.id,
            Some("edited@test.com"),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let err = db
            .queue_draft_for_send(
                &draft.id,
                validated_rev,
                "2000-01-01T00:00:00Z",
                &bot_attribution(&["financial_content"]),
            )
            .unwrap_err();
        assert!(matches!(err, StoreError::DraftModifiedConcurrently(_)));
        let reloaded = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(reloaded.send_after.is_none());
        assert!(
            reloaded
                .metadata
                .and_then(|m| m.get("attribution").cloned())
                .is_none()
        );
    }

    #[test]
    fn queue_draft_for_send_transitions_pending_review_to_due() {
        // A re-queued draft that was parked in pending_review must become due
        // (status='draft') so the sweep can see it — never "scheduled" yet invisible.
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("S"),
                Some("B"),
                None,
                None,
                None,
                None,
                Some("mcp"),
            )
            .unwrap();
        db.update_draft_status(&draft.id, DraftStatus::PendingReview)
            .unwrap();
        // Before: a pending_review row is invisible to the due query even with a
        // past send_after — proving the transition is required.
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        db.queue_draft_for_send(
            &draft.id,
            rev,
            "2000-01-01T00:00:00Z",
            &bot_attribution(&["informational"]),
        )
        .unwrap();
        let reloaded = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(reloaded.status, DraftStatus::Draft);
        let due = db.list_drafts_due_for_send().unwrap();
        assert!(
            due.iter().any(|d| d.id == draft.id),
            "re-queued draft is now due"
        );
    }

    #[test]
    fn queue_draft_for_send_refuses_claimed_row() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("S"),
                Some("B"),
                None,
                None,
                None,
                None,
                Some("mcp"),
            )
            .unwrap();
        db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00Z")
            .unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        let _token = db.claim_draft_for_sending(&draft.id, rev).unwrap().unwrap();
        // The row is now `sending`; queueing must refuse (not editable).
        let err = db
            .queue_draft_for_send(
                &draft.id,
                rev,
                "2000-01-01T00:00:00Z",
                &bot_attribution(&["informational"]),
            )
            .unwrap_err();
        assert!(
            matches!(err, StoreError::DraftNotEditable(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn queue_draft_for_send_refuses_blocked_row_with_no_partial_write() {
        // A `blocked` row is Governor-denied. It is `is_editable` (a human may
        // still edit it) but NOT `is_queueable`: a re-queue must refuse rather
        // than resurrect a denied send into a due `draft`. The refusal is a
        // truthful DraftNotEditable and nothing is scheduled or persisted.
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("S"),
                Some("B"),
                None,
                None,
                None,
                None,
                Some("mcp"),
            )
            .unwrap();
        db.update_draft_status(&draft.id, DraftStatus::Blocked)
            .unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        let err = db
            .queue_draft_for_send(
                &draft.id,
                rev,
                "2000-01-01T00:00:00Z",
                &bot_attribution(&["informational"]),
            )
            .unwrap_err();
        assert!(
            matches!(err, StoreError::DraftNotEditable(_)),
            "blocked must refuse as not-editable, got {err:?}"
        );
        // Still blocked, still no schedule, still no attribution: no resurrection.
        let reloaded = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(reloaded.status, DraftStatus::Blocked);
        assert!(reloaded.send_after.is_none(), "no send_after was written");
        assert!(
            reloaded
                .metadata
                .and_then(|m| m.get("attribution").cloned())
                .is_none(),
            "no attribution was written onto a blocked row"
        );
        // The sweep's due query never sees it.
        let due = db.list_drafts_due_for_send().unwrap();
        assert!(!due.iter().any(|d| d.id == draft.id));
    }

    #[test]
    fn material_revision_makes_persisted_attribution_stale() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("S"),
                Some("B"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.set_draft_attribution(
            &draft.id,
            &serde_json::json!({"protocol": "envelope.attribution.v1", "declared_attrs": ["informational"], "attempts": 0}),
        )
        .unwrap();
        let stamped = attribution_of(&db, &draft.id)["revision"].as_i64().unwrap();

        // A material edit (recipients) bumps the row revision; the persisted
        // declaration's stamped revision no longer matches — it is stale.
        db.update_draft_content(
            &draft.id,
            Some("new@test.com"),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let after = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(after.revision > stamped, "material edit bumps revision");
        // The block survives an edit that only strips human_approval, but its
        // stamped revision is now stale relative to the row.
        let block_rev = after.metadata.unwrap()["attribution"]["revision"]
            .as_i64()
            .unwrap();
        assert_eq!(block_rev, stamped);
        assert_ne!(block_rev, after.revision, "declaration is stale after edit");
    }

    #[test]
    fn defer_attribution_retry_returns_to_due_under_lease() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("S"),
                Some("B"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        let past = "2000-01-01T00:00:00Z";
        db.update_draft_send_after(&draft.id, past).unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        let token = db.claim_draft_for_sending(&draft.id, rev).unwrap().unwrap();

        // Wrong token changes nothing.
        assert!(
            !db.defer_attribution_retry(
                &draft.id,
                "wrong-token",
                &serde_json::json!({"attempts": 1})
            )
            .unwrap()
        );

        assert!(
            db.defer_attribution_retry(
                &draft.id,
                &token,
                &serde_json::json!({"protocol": "envelope.attribution.v1", "declared_attrs": [], "attempts": 1}),
            )
            .unwrap()
        );
        let after = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(after.status, DraftStatus::Draft, "returned to due");
        assert_eq!(
            after.send_after.as_deref(),
            Some(past),
            "still due, not cleared"
        );
        assert_eq!(after.revision, rev, "attempt bump does not change revision");
        assert_eq!(after.metadata.unwrap()["attribution"]["attempts"], 1);
        // Still claimable by a later sweep at the same revision.
        assert!(
            db.claim_draft_for_sending(&draft.id, rev)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn park_attribution_exhausted_clears_scheduling_under_lease() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("S"),
                Some("B"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00Z")
            .unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        let token = db.claim_draft_for_sending(&draft.id, rev).unwrap().unwrap();

        assert!(
            !db.park_attribution_exhausted(&draft.id, "wrong", &serde_json::json!({"attempts": 3}))
                .unwrap()
        );
        assert!(
            db.park_attribution_exhausted(
                &draft.id,
                &token,
                &serde_json::json!({"attempts": 3, "park_reason": "attribution_exhausted"}),
            )
            .unwrap()
        );
        let after = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(after.status, DraftStatus::PendingReview);
        assert_eq!(after.send_after, None, "scheduling disabled");
        assert_eq!(
            after.metadata.unwrap()["attribution"]["park_reason"],
            "attribution_exhausted"
        );
        // Never selected as due again — no retry storm.
        assert!(db.list_drafts_due_for_send().unwrap().is_empty());
    }

    #[test]
    fn mark_draft_sent_clears_send_after_so_a_sent_draft_is_not_still_scheduled() {
        // Real evidence: a scheduled allowed send left `send_after` at the expired
        // timestamp after the row flipped to `sent`, so downstream surfaces could
        // still infer it was queued. mark_draft_sent must neutralize it.
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("S"),
                Some("B"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00Z")
            .unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        let token = db.claim_draft_for_sending(&draft.id, rev).unwrap().unwrap();

        db.mark_draft_sent(&draft.id, &token, Some("<mid@host>"))
            .unwrap();

        let after = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(after.status, DraftStatus::Sent);
        assert!(after.sent_at.is_some(), "sent_at must be set");
        assert_eq!(after.message_id.as_deref(), Some("<mid@host>"));
        assert_eq!(
            after.send_after, None,
            "a sent draft must no longer carry a scheduled send_after"
        );
        assert!(db.list_drafts_due_for_send().unwrap().is_empty());
    }

    #[test]
    fn park_for_review_parks_pending_review_and_clears_send_after_under_lease() {
        // Real evidence: a scheduled send that routed `review` was released to
        // `pending_review` but kept its expired `send_after`, so the dashboard
        // rendered it "Queued for sending" with a stale countdown. park_for_review
        // must clear send_after atomically under the owner lease.
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("S"),
                Some("B"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00Z")
            .unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        let token = db.claim_draft_for_sending(&draft.id, rev).unwrap().unwrap();

        // A non-owner cannot park it.
        assert!(!db.park_for_review(&draft.id, "wrong-token").unwrap());
        // The owner parks it for review.
        assert!(db.park_for_review(&draft.id, &token).unwrap());

        let after = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(after.status, DraftStatus::PendingReview);
        assert_eq!(
            after.send_after, None,
            "a review-parked draft must not look queued/due"
        );
        assert!(after.sent_at.is_none(), "review parking never sends");
        let block = after.metadata.as_ref().unwrap()["send_block"].clone();
        assert_eq!(block["code"], "send_stopped");
        assert_eq!(block["title"], "This send was stopped");
        assert!(
            block["explanation"]
                .as_str()
                .unwrap()
                .contains("Nothing was transmitted"),
            "{block}"
        );
        // Never selected as due again.
        assert!(db.list_drafts_due_for_send().unwrap().is_empty());
    }

    #[test]
    fn mark_draft_sent_clears_the_stale_drafts_imap_uid() {
        // The `imap_uid` column is the IMAP *Drafts*-folder UID. Once the draft is
        // sent it is stale (the provider Drafts copy is cleaned up from the
        // pre-transition snapshot), so mark_draft_sent must clear it. This keeps
        // the field Drafts-folder-only and prevents Sent proof being conflated
        // with a Drafts UID.
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("S"),
                Some("B"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_imap_uid(&draft.id, 4242).unwrap();
        assert_eq!(
            db.get_draft(&draft.id).unwrap().unwrap().imap_uid,
            Some(4242)
        );

        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        let token = db
            .claim_draft_for_immediate_send(&draft.id, rev)
            .unwrap()
            .unwrap();
        db.mark_draft_sent(&draft.id, &token, Some("<mid@host>"))
            .unwrap();

        let after = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(after.status, DraftStatus::Sent);
        assert_eq!(
            after.imap_uid, None,
            "the stale Drafts-folder UID must clear on sent"
        );
    }

    #[test]
    fn record_sent_copy_proof_stores_folder_qualified_metadata_never_the_drafts_uid() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("S"),
                Some("B"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        // A pre-existing Drafts UID that mark_draft_sent will clear.
        db.update_draft_imap_uid(&draft.id, 4242).unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        let token = db
            .claim_draft_for_immediate_send(&draft.id, rev)
            .unwrap()
            .unwrap();
        db.mark_draft_sent(&draft.id, &token, Some("<mid@host>"))
            .unwrap();

        // A resolved provider copy: folder-qualified in metadata.sent_copy, and
        // the Drafts `imap_uid` column stays cleared (never repurposed).
        assert!(
            db.record_sent_copy_proof(&draft.id, Some("Sent"), Some(77), "found", "provider")
                .unwrap()
        );
        let after = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(
            after.imap_uid, None,
            "Sent UID must never be written to the Drafts imap_uid column"
        );
        let sent_copy = &after.metadata.unwrap()["sent_copy"];
        assert_eq!(sent_copy["folder"], "Sent");
        assert_eq!(sent_copy["uid"], 77);
        assert_eq!(sent_copy["lookup_status"], "found");
        assert_eq!(sent_copy["copy_source"], "provider");
    }

    #[test]
    fn record_sent_copy_proof_unresolved_stores_explicit_null_uid() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("S"),
                Some("B"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_imap_uid(&draft.id, 4242).unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        let token = db
            .claim_draft_for_immediate_send(&draft.id, rev)
            .unwrap()
            .unwrap();
        db.mark_draft_sent(&draft.id, &token, Some("<mid@host>"))
            .unwrap();

        // Unresolved proof: uid is recorded as explicit JSON null, NOT retained
        // from the stale Drafts UID.
        assert!(
            db.record_sent_copy_proof(&draft.id, None, None, "not_found", "unresolved")
                .unwrap()
        );
        let after = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(after.imap_uid, None);
        let sent_copy = &after.metadata.unwrap()["sent_copy"];
        assert!(
            sent_copy.get("uid").is_some() && sent_copy["uid"].is_null(),
            "unresolved Sent UID must be explicit JSON null, got {sent_copy:?}"
        );
        assert_eq!(sent_copy["copy_source"], "unresolved");
        assert_eq!(sent_copy["lookup_status"], "not_found");
    }

    #[test]
    fn create_and_get_draft() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        assert_eq!(draft.to_addr, "to@test.com");
        assert_eq!(draft.status, DraftStatus::Draft);
        assert_eq!(draft.imap_uid, None);

        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(fetched.id, draft.id);
        assert_eq!(fetched.imap_uid, None);
    }

    #[test]
    fn record_human_approval_persists_and_preserves_metadata() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        // Contextual reply state written before approval must survive the merge.
        db.set_draft_metadata(
            &draft.id,
            &serde_json::json!({
                "in_reply_to": "<parent@example.net>",
                "references": ["<root@example.net>", "<parent@example.net>"],
            }),
        )
        .unwrap();

        // Agent-created state alone never derives as approved.
        let before = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(!before.human_approved());

        db.record_draft_human_approval(
            &draft.id,
            before.revision,
            "human:dashboard",
            "2026-07-10T09:00:00Z",
        )
        .unwrap();

        let after = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(after.human_approved());
        let meta = after.metadata.as_ref().unwrap();
        assert_eq!(
            meta["human_approval"]["approved_by"], "human:dashboard",
            "attestation is a surface label, never an address or secret"
        );
        assert_eq!(
            meta["human_approval"]["approved_at"],
            "2026-07-10T09:00:00Z"
        );
        assert_eq!(
            meta["in_reply_to"], "<parent@example.net>",
            "threading metadata must survive the approval merge"
        );
        assert_eq!(meta["references"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn record_human_approval_errors_on_unknown_draft() {
        let db = setup();
        assert!(
            db.record_draft_human_approval("nope", 0, "human:dashboard", "2026-07-10T09:00:00Z")
                .is_err()
        );
    }

    #[test]
    fn human_approved_is_fail_closed_on_malformed_attestations() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("S"),
                None,
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();

        // A non-human surface label must not derive as approved even if an
        // attestation-shaped object appears in metadata.
        db.set_draft_metadata(
            &draft.id,
            &serde_json::json!({
                "human_approval": {"approved_by": "agent:mcp", "approved_at": "2026-07-10T09:00:00Z"}
            }),
        )
        .unwrap();
        assert!(!db.get_draft(&draft.id).unwrap().unwrap().human_approved());

        // Missing timestamp fails closed too.
        db.set_draft_metadata(
            &draft.id,
            &serde_json::json!({"human_approval": {"approved_by": "human:dashboard"}}),
        )
        .unwrap();
        assert!(!db.get_draft(&draft.id).unwrap().unwrap().human_approved());

        // A non-RFC 3339 timestamp fails closed: naive (no offset) and garbage.
        // Written via raw SQL (bypassing the stripping/bumping public paths)
        // with the CURRENT revision embedded, so the timestamp is the only
        // invalid part under test.
        let revision = db.get_draft(&draft.id).unwrap().unwrap().revision;
        for bad_at in ["2026-07-10T09:00:00", "yesterday", "", "1752130800"] {
            let crafted = serde_json::json!({
                "human_approval": {
                    "approved_by": "human:dashboard",
                    "approved_at": bad_at,
                    "revision": revision,
                }
            })
            .to_string();
            db.conn()
                .execute(
                    "UPDATE drafts SET metadata = ?1 WHERE id = ?2",
                    rusqlite::params![crafted, draft.id],
                )
                .unwrap();
            assert!(
                !db.get_draft(&draft.id).unwrap().unwrap().human_approved(),
                "approved_at {bad_at:?} must fail closed"
            );
        }

        // A stale approved revision fails closed even with a valid timestamp
        // and human surface label.
        let crafted = serde_json::json!({
            "human_approval": {
                "approved_by": "human:dashboard",
                "approved_at": "2026-07-10T09:00:00Z",
                "revision": revision - 1,
            }
        })
        .to_string();
        db.conn()
            .execute(
                "UPDATE drafts SET metadata = ?1 WHERE id = ?2",
                rusqlite::params![crafted, draft.id],
            )
            .unwrap();
        assert!(
            !db.get_draft(&draft.id).unwrap().unwrap().human_approved(),
            "an attestation for a previous revision must fail closed"
        );
    }

    /// Approval is bound to the draft revision: any content-relevant edit
    /// (content/recipients, attachments, metadata rewrite) must invalidate the
    /// attestation so an agent/MCP edit cannot retain `tyler_approved`.
    #[test]
    fn content_relevant_edits_invalidate_human_approval() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();

        let approve = |db: &Database| {
            let current = db.get_draft(&draft.id).unwrap().unwrap();
            db.record_draft_human_approval(
                &draft.id,
                current.revision,
                "human:dashboard",
                "2026-07-10T09:00:00Z",
            )
            .unwrap();
            assert!(db.get_draft(&draft.id).unwrap().unwrap().human_approved());
        };

        // Content/recipient edit clears the attestation.
        approve(&db);
        db.update_draft_content(
            &draft.id,
            Some("attacker@evil.example"),
            None,
            None,
            None,
            Some("changed body"),
            None,
        )
        .unwrap();
        assert!(
            !db.get_draft(&draft.id).unwrap().unwrap().human_approved(),
            "content edit after approval must invalidate the attestation"
        );

        // Attachment change clears it.
        approve(&db);
        db.update_draft_attachments(
            &draft.id,
            &[serde_json::json!({
                "filename": "new.pdf", "content_type": "application/pdf", "size": 3,
                "data_base64": "Zm9v",
            })],
        )
        .unwrap();
        assert!(
            !db.get_draft(&draft.id).unwrap().unwrap().human_approved(),
            "attachment edit after approval must invalidate the attestation"
        );

        // Whole-metadata rewrite (threading etc.) clears it — even a
        // read-modify-write that carries the attestation forward.
        approve(&db);
        let carried = db.get_draft(&draft.id).unwrap().unwrap().metadata.unwrap();
        assert!(carried.get("human_approval").is_some());
        db.set_draft_metadata(&draft.id, &carried).unwrap();
        assert!(
            !db.get_draft(&draft.id).unwrap().unwrap().human_approved(),
            "metadata rewrite must strip a carried-forward attestation"
        );

        // Direct injection through the public metadata path is stripped too.
        db.set_draft_metadata(
            &draft.id,
            &serde_json::json!({
                "human_approval": {
                    "approved_by": "human:dashboard",
                    "approved_at": "2026-07-10T09:00:00Z"
                }
            }),
        )
        .unwrap();
        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(!fetched.human_approved());
        assert!(
            fetched
                .metadata
                .map(|m| m.get("human_approval").is_none())
                .unwrap_or(true),
            "set_draft_metadata must never persist an injected attestation"
        );
    }

    /// An agent-authored draft, the shape the dashboard send routes act on.
    fn agent_draft(db: &Database) -> Draft {
        db.create_draft(
            "acc1",
            "to@test.com",
            Some("Subject"),
            Some("Body"),
            None,
            None,
            None,
            None,
            Some("agent"),
        )
        .unwrap()
    }

    fn queued_by_human(db: &Database, id: &str) -> Draft {
        let revision = db.get_draft(id).unwrap().unwrap().revision;
        db.queue_draft_with_human_send(
            id,
            revision,
            "2030-01-01T00:00:00Z",
            "human:dashboard",
            "2026-07-10T09:00:00Z",
        )
        .unwrap()
    }

    /// The persisted declaration an agent binds when it queues
    /// (`PersistedDeclaration::new_bot(...).to_value()` in the CLI/MCP send
    /// paths); shaped by hand here so the store crate stays dependency-free.
    fn agent_declaration(revision: i64) -> serde_json::Value {
        serde_json::json!({
            "protocol": "envelope.attribution.v1",
            "declared_attrs": ["recipient_requested"],
            "revision": revision,
        })
    }

    #[test]
    fn human_send_records_a_revision_bound_authorization_with_the_queue() {
        // The authorization exists only as part of the queue transition a human
        // performed: same call, same revision, same transaction.
        let db = setup();
        let draft = agent_draft(&db);
        let before = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(before.human_send_surface(), None);

        let queued = queued_by_human(&db, &draft.id);

        assert_eq!(queued.human_send_surface(), Some("human:dashboard"));
        assert!(
            queued.human_approved(),
            "the human who sent it also reviewed it"
        );
        assert_eq!(queued.send_after.as_deref(), Some("2030-01-01T00:00:00Z"));
        let meta = queued.metadata.as_ref().unwrap();
        assert_eq!(meta["human_send"]["queued_by"], "human:dashboard");
        assert_eq!(meta["human_send"]["queued_at"], "2026-07-10T09:00:00Z");
        assert_eq!(meta["human_send"]["revision"], before.revision);
    }

    #[test]
    fn approving_a_draft_never_authorizes_a_send() {
        // Approve is a review decision. It records the attestation and leaves the
        // draft unqueued and unauthorized, so no later path can read it as a
        // human's send.
        let db = setup();
        let draft = agent_draft(&db);
        let revision = db.get_draft(&draft.id).unwrap().unwrap().revision;

        let approved = db
            .approve_draft_revision(
                &draft.id,
                revision,
                "human:dashboard",
                "2026-07-10T09:00:00Z",
            )
            .unwrap();

        assert!(approved.human_approved());
        assert_eq!(
            approved.human_send_surface(),
            None,
            "approval is not a send authorization"
        );
        assert!(approved.send_after.is_none(), "approval queues nothing");
    }

    #[test]
    fn an_agent_requeue_supersedes_the_human_send_authorization() {
        // The agent queue transition (CLI `draft send`, MCP `send_draft`) takes
        // ownership of the pending send: the human authorized a different
        // transmission, and this one is the agent's to declare for.
        let db = setup();
        let draft = agent_draft(&db);
        let queued = queued_by_human(&db, &draft.id);
        assert!(queued.human_send_surface().is_some());

        db.queue_draft_for_send(
            &draft.id,
            queued.revision,
            "2030-02-02T00:00:00Z",
            &agent_declaration(queued.revision),
        )
        .unwrap();

        let requeued = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(
            requeued.human_send_surface(),
            None,
            "an agent-queued send is never a human send"
        );
        assert!(
            requeued.human_approved(),
            "the review attestation survives — it is a declared attribute for \
             scoring, and the human really did approve this revision"
        );
        assert_eq!(requeued.send_after.as_deref(), Some("2030-02-02T00:00:00Z"));
    }

    #[test]
    fn withdrawing_or_changing_the_send_invalidates_the_authorization() {
        // Everything that changes what would be transmitted, or takes the
        // transmission back, withdraws the human's authorization for it.
        let db = setup();
        let draft = agent_draft(&db);

        // Hold takes the message out of the outbox.
        queued_by_human(&db, &draft.id);
        let held = db.hold_scheduled_draft(&draft.id).unwrap();
        assert_eq!(held.human_send_surface(), None, "Hold withdraws the send");
        assert!(held.send_after.is_none());

        // A content edit.
        queued_by_human(&db, &draft.id);
        db.update_draft_content(&draft.id, None, None, None, None, Some("changed"), None)
            .unwrap();
        assert_eq!(
            db.get_draft(&draft.id)
                .unwrap()
                .unwrap()
                .human_send_surface(),
            None,
            "an edit is not the version the human sent"
        );

        // An attachment change.
        queued_by_human(&db, &draft.id);
        db.update_draft_attachments(
            &draft.id,
            &[serde_json::json!({
                "filename": "new.pdf", "content_type": "application/pdf", "size": 3,
                "data_base64": "Zm9v",
            })],
        )
        .unwrap();
        assert_eq!(
            db.get_draft(&draft.id)
                .unwrap()
                .unwrap()
                .human_send_surface(),
            None,
            "changing the attachments changes what was sent"
        );

        // A revision-guarded attachment change from an interactive surface.
        let queued = queued_by_human(&db, &draft.id);
        db.update_draft_attachments_for_revision(&draft.id, queued.revision, &[])
            .unwrap();
        assert_eq!(
            db.get_draft(&draft.id)
                .unwrap()
                .unwrap()
                .human_send_surface(),
            None
        );

        // A metadata rewrite, including a read-modify-write that tries to carry
        // the authorization forward.
        let queued = queued_by_human(&db, &draft.id);
        let carried = queued.metadata.clone().unwrap();
        assert!(carried.get("human_send").is_some());
        db.set_draft_metadata(&draft.id, &carried).unwrap();
        let after = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(after.human_send_surface(), None);
        assert!(
            after
                .metadata
                .as_ref()
                .is_none_or(|m| m.get("human_send").is_none()),
            "set_draft_metadata must never persist a carried-forward authorization"
        );

        // Direct injection through the public metadata path is stripped too.
        db.set_draft_metadata(
            &draft.id,
            &serde_json::json!({
                "human_send": {
                    "queued_by": "human:dashboard",
                    "queued_at": "2026-07-10T09:00:00Z",
                }
            }),
        )
        .unwrap();
        assert_eq!(
            db.get_draft(&draft.id)
                .unwrap()
                .unwrap()
                .human_send_surface(),
            None,
            "an agent cannot inject a send authorization"
        );
    }

    #[test]
    fn human_send_surface_is_fail_closed_on_malformed_authorizations() {
        let db = setup();
        let draft = agent_draft(&db);
        let revision = db.get_draft(&draft.id).unwrap().unwrap().revision;

        // Written via raw SQL (bypassing the stripping public paths) so each
        // crafted authorization is tested with everything else valid.
        let craft = |authorization: serde_json::Value| {
            db.conn()
                .execute(
                    "UPDATE drafts SET metadata = ?1 WHERE id = ?2",
                    rusqlite::params![
                        serde_json::json!({ "human_send": authorization }).to_string(),
                        draft.id
                    ],
                )
                .unwrap();
            db.get_draft(&draft.id)
                .unwrap()
                .unwrap()
                .human_send_surface()
                .is_some()
        };

        for (why, authorization) in [
            (
                "a non-human surface label",
                serde_json::json!({
                    "queued_by": "agent:mcp",
                    "queued_at": "2026-07-10T09:00:00Z",
                    "revision": revision,
                }),
            ),
            (
                "a missing timestamp",
                serde_json::json!({ "queued_by": "human:dashboard", "revision": revision }),
            ),
            (
                "a naive (offset-free) timestamp",
                serde_json::json!({
                    "queued_by": "human:dashboard",
                    "queued_at": "2026-07-10T09:00:00",
                    "revision": revision,
                }),
            ),
            (
                "a stale revision",
                serde_json::json!({
                    "queued_by": "human:dashboard",
                    "queued_at": "2026-07-10T09:00:00Z",
                    "revision": revision - 1,
                }),
            ),
            (
                "no revision binding at all",
                serde_json::json!({
                    "queued_by": "human:dashboard",
                    "queued_at": "2026-07-10T09:00:00Z",
                }),
            ),
        ] {
            assert!(!craft(authorization), "{why} must fail closed");
        }
    }

    #[test]
    fn human_send_refuses_a_revision_the_human_did_not_see() {
        // The CAS is the whole guarantee: a draft edited since the operator
        // loaded it is not queued and not authorized.
        let db = setup();
        let draft = agent_draft(&db);
        let viewed = db.get_draft(&draft.id).unwrap().unwrap();
        db.update_draft_content(&draft.id, None, None, None, None, Some("edited"), None)
            .unwrap();

        let err = db
            .queue_draft_with_human_send(
                &draft.id,
                viewed.revision,
                "2030-01-01T00:00:00Z",
                "human:dashboard",
                "2026-07-10T09:00:00Z",
            )
            .unwrap_err();

        assert!(matches!(err, StoreError::DraftModifiedConcurrently(_)));
        let after = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(after.human_send_surface(), None);
        assert!(
            after.send_after.is_none(),
            "nothing partially queued survives the refusal"
        );
    }

    /// Create a draft carrying BOTH body forms, as an agent-composed reply
    /// with an HTML alternative does.
    fn dual_body_draft(db: &Database) -> Draft {
        db.create_draft(
            "acc1",
            "to@test.com",
            Some("Subject"),
            Some("OLD text body"),
            Some("<p>OLD html body</p>"),
            None,
            None,
            None,
            Some("agent"),
        )
        .unwrap()
    }

    /// Regression: a body edit replaces the draft's body representation SET.
    ///
    /// The dashboard's plain-text editor POSTs `text_content` alone for a
    /// draft that carries both a text and an HTML body. Coalescing each body
    /// column independently kept the omitted HTML, so the row stayed dual-body
    /// and the send snapshot went out as `multipart/alternative` — where
    /// receiving clients prefer the HTML alternative and render the UNEDITED
    /// draft. The edited body must be the only body that persists, and the only
    /// body the due-send snapshot carries.
    #[test]
    fn text_edit_clears_the_stale_html_alternative() {
        let db = setup();
        let draft = dual_body_draft(&db);
        let viewed = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(viewed.text_content.as_deref(), Some("OLD text body"));
        assert_eq!(viewed.html_content.as_deref(), Some("<p>OLD html body</p>"));

        // Exactly the edit the dashboard text editor performs: the edited text,
        // no HTML field, guarded on the revision the human viewed.
        let edited = db
            .update_draft_content_for_revision(
                &draft.id,
                viewed.revision,
                None,
                viewed.cc_addr.as_deref(),
                viewed.bcc_addr.as_deref(),
                None,
                Some("NEW text body"),
                None,
            )
            .unwrap();

        assert_eq!(edited.text_content.as_deref(), Some("NEW text body"));
        assert_eq!(
            edited.html_content, None,
            "the omitted HTML alternative must not survive a text-body edit — a \
             dual-body row sends multipart/alternative and clients render the \
             stale HTML"
        );
        assert_eq!(edited.revision, viewed.revision + 1);

        // The due-send snapshot — the row the scheduled sweep reloads and hands
        // to SMTP — carries the edited body only.
        db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00Z")
            .unwrap();
        let due = db.list_drafts_due_for_send().unwrap();
        let queued = due
            .iter()
            .find(|d| d.id == draft.id)
            .expect("edited draft should be due");
        assert_eq!(queued.text_content.as_deref(), Some("NEW text body"));
        assert_eq!(
            queued.html_content, None,
            "the transmission snapshot must not be dual-body after a text edit"
        );
    }

    /// The mirror case: an HTML-body edit clears the omitted text alternative,
    /// so the recipient's plain-text client cannot render the pre-edit body.
    #[test]
    fn html_edit_clears_the_stale_text_alternative() {
        let db = setup();
        let draft = dual_body_draft(&db);
        let viewed = db.get_draft(&draft.id).unwrap().unwrap();

        let edited = db
            .update_draft_content_for_revision(
                &draft.id,
                viewed.revision,
                None,
                viewed.cc_addr.as_deref(),
                viewed.bcc_addr.as_deref(),
                None,
                None,
                Some("<p>NEW html body</p>"),
            )
            .unwrap();

        assert_eq!(edited.html_content.as_deref(), Some("<p>NEW html body</p>"));
        assert_eq!(
            edited.text_content, None,
            "the omitted text alternative must not survive an HTML-body edit"
        );
    }

    /// A recipient- or subject-only edit supplies NO body: both existing body
    /// forms are preserved untouched. Clearing a body the editor never showed
    /// the human would silently drop content.
    #[test]
    fn recipient_or_subject_only_edit_preserves_both_body_forms() {
        let db = setup();
        let draft = dual_body_draft(&db);
        let viewed = db.get_draft(&draft.id).unwrap().unwrap();

        let edited = db
            .update_draft_content_for_revision(
                &draft.id,
                viewed.revision,
                Some("someone-else@test.com"),
                viewed.cc_addr.as_deref(),
                viewed.bcc_addr.as_deref(),
                Some("New subject"),
                None,
                None,
            )
            .unwrap();

        assert_eq!(edited.to_addr, "someone-else@test.com");
        assert_eq!(edited.subject.as_deref(), Some("New subject"));
        assert_eq!(edited.text_content.as_deref(), Some("OLD text body"));
        assert_eq!(
            edited.html_content.as_deref(),
            Some("<p>OLD html body</p>"),
            "an edit that supplies no body must preserve both existing bodies"
        );
    }

    /// Repeat approvals of the same revision are idempotent (the original
    /// stamp is preserved); a fresh stamp only lands after an edit cleared it.
    #[test]
    fn repeat_approval_preserves_stamp_until_an_edit_invalidates() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();

        let approved_at = |db: &Database| {
            db.get_draft(&draft.id).unwrap().unwrap().metadata.unwrap()["human_approval"]
                ["approved_at"]
                .as_str()
                .unwrap()
                .to_string()
        };

        let rev0 = db.get_draft(&draft.id).unwrap().unwrap().revision;
        db.record_draft_human_approval(&draft.id, rev0, "human:dashboard", "2026-07-10T09:00:00Z")
            .unwrap();
        db.record_draft_human_approval(&draft.id, rev0, "human:dashboard", "2026-07-10T10:30:00Z")
            .unwrap();
        assert_eq!(
            approved_at(&db),
            "2026-07-10T09:00:00Z",
            "re-approving an unchanged revision must not rewrite the stamp"
        );

        // Edit invalidates; the next approval stamps the new revision fresh.
        db.update_draft_content(&draft.id, None, None, None, Some("New subject"), None, None)
            .unwrap();
        let edited = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(!edited.human_approved());
        db.record_draft_human_approval(
            &draft.id,
            edited.revision,
            "human:dashboard",
            "2026-07-10T11:00:00Z",
        )
        .unwrap();
        assert_eq!(approved_at(&db), "2026-07-10T11:00:00Z");
    }

    /// Compare-and-set: an approval bound to the revision the human read must
    /// be refused when the draft was edited concurrently (after the read,
    /// before the write) — the new content never inherits the approval.
    #[test]
    fn approval_cas_rejects_concurrently_edited_revision() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();

        // The human's view of the draft.
        let human_view = db.get_draft(&draft.id).unwrap().unwrap();

        // A concurrent agent edit lands between the human's read and the
        // approval write.
        db.update_draft_content(
            &draft.id,
            Some("attacker@evil.example"),
            None,
            None,
            None,
            Some("swapped body"),
            None,
        )
        .unwrap();

        let err = db
            .record_draft_human_approval(
                &draft.id,
                human_view.revision,
                "human:dashboard",
                "2026-07-10T09:00:00Z",
            )
            .unwrap_err();
        assert!(
            matches!(err, StoreError::DraftModifiedConcurrently(_)),
            "expected DraftModifiedConcurrently, got {err:?}"
        );
        assert!(
            !db.get_draft(&draft.id).unwrap().unwrap().human_approved(),
            "the edited content must not inherit the approval"
        );
    }

    /// The atomic queue primitive is all-or-nothing: when the final approval
    /// CAS fails (concurrent edit), the status promotion and `send_after` are
    /// rolled back too — no partially queued, unapproved state persists.
    #[test]
    fn queue_with_human_approval_rolls_back_on_concurrent_edit() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_status(&draft.id, DraftStatus::PendingReview)
            .unwrap();

        // Human reviews this revision…
        let human_view = db.get_draft(&draft.id).unwrap().unwrap();
        // …and an agent edits it before the queue lands.
        db.update_draft_content(&draft.id, None, None, None, None, Some("swapped"), None)
            .unwrap();

        let err = db
            .queue_draft_with_human_send(
                &draft.id,
                human_view.revision,
                "2026-07-10T09:02:00Z",
                "human:dashboard",
                "2026-07-10T09:00:00Z",
            )
            .unwrap_err();
        assert!(matches!(err, StoreError::DraftModifiedConcurrently(_)));

        let after = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(
            after.status,
            DraftStatus::PendingReview,
            "status promotion must roll back with the failed approval"
        );
        assert!(
            after.send_after.is_none(),
            "send_after must roll back with the failed approval"
        );
        assert!(!after.human_approved());
    }

    /// Happy path for the atomic queue primitive: promotion, schedule, and
    /// revision-bound attestation land together.
    #[test]
    fn queue_with_human_approval_promotes_schedules_and_approves() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_status(&draft.id, DraftStatus::PendingReview)
            .unwrap();
        let human_view = db.get_draft(&draft.id).unwrap().unwrap();

        db.queue_draft_with_human_send(
            &draft.id,
            human_view.revision,
            "2026-07-10T09:02:00Z",
            "human:dashboard",
            "2026-07-10T09:00:00Z",
        )
        .unwrap();

        let queued = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(queued.status, DraftStatus::Draft);
        assert_eq!(queued.send_after.as_deref(), Some("2026-07-10T09:02:00Z"));
        assert!(queued.human_approved());

        // Blocked/discarded drafts still refuse to queue.
        db.update_draft_status(&draft.id, DraftStatus::Blocked)
            .unwrap();
        let blocked = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(matches!(
            db.queue_draft_with_human_send(
                &blocked.id,
                blocked.revision,
                "2026-07-10T09:05:00Z",
                "human:dashboard",
                "2026-07-10T09:04:00Z",
            )
            .unwrap_err(),
            StoreError::DraftNotEditable(_)
        ));
    }

    /// The atomic approve primitive also rolls the status flip back when the
    /// approval CAS detects a concurrent edit.
    #[test]
    fn approve_draft_revision_rolls_back_status_on_concurrent_edit() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_status(&draft.id, DraftStatus::PendingReview)
            .unwrap();

        let human_view = db.get_draft(&draft.id).unwrap().unwrap();
        db.update_draft_content(&draft.id, None, None, None, None, Some("swapped"), None)
            .unwrap();

        let err = db
            .approve_draft_revision(
                &draft.id,
                human_view.revision,
                "human:dashboard",
                "2026-07-10T09:00:00Z",
            )
            .unwrap_err();
        assert!(matches!(err, StoreError::DraftModifiedConcurrently(_)));
        let after = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(after.status, DraftStatus::PendingReview);
        assert!(!after.human_approved());
    }

    /// Deterministic concurrent-claim behavior: exactly one claim wins for a
    /// given id+revision+status; the winner removes the row from the due
    /// query before any transmission work begins.
    #[test]
    fn claim_is_exclusive_and_removes_draft_from_due() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Queued"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00Z")
            .unwrap();
        let due = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(
            db.list_drafts_due_for_send()
                .unwrap()
                .iter()
                .any(|d| d.id == draft.id)
        );

        // First sweeper wins the claim…
        assert!(
            db.claim_draft_for_sending(&draft.id, due.revision)
                .unwrap()
                .is_some()
        );
        // …a second sweeper (same snapshot) must lose.
        assert!(
            db.claim_draft_for_sending(&draft.id, due.revision)
                .unwrap()
                .is_none()
        );

        let claimed = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(claimed.status, DraftStatus::Sending);
        assert!(
            !db.list_drafts_due_for_send()
                .unwrap()
                .iter()
                .any(|d| d.id == draft.id),
            "a claimed draft must be invisible to the due query"
        );
        // While claimed: not editable, not discardable, not re-queueable.
        assert!(!claimed.status.is_editable());
        assert!(!db.discard_draft(&draft.id).unwrap());
        assert!(matches!(
            db.queue_draft_with_human_send(
                &draft.id,
                claimed.revision,
                "2026-07-10T09:02:00Z",
                "human:dashboard",
                "2026-07-10T09:00:00Z",
            )
            .unwrap_err(),
            StoreError::DraftNotEditable(_)
        ));
    }

    /// A stale-revision snapshot (concurrent edit between the due scan and the
    /// claim) must not be claimable — the sweeper re-scans and sees the new
    /// revision next cycle instead of transmitting a stale snapshot.
    #[test]
    fn claim_rejects_stale_revision_and_non_draft_status() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Queued"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00Z")
            .unwrap();
        let scanned = db.get_draft(&draft.id).unwrap().unwrap();

        // Concurrent edit bumps the revision after the due scan.
        db.update_draft_content(&draft.id, None, None, None, None, Some("edited"), None)
            .unwrap();
        assert!(
            db.claim_draft_for_sending(&draft.id, scanned.revision)
                .unwrap()
                .is_none(),
            "stale revision must not be claimable"
        );
        assert_eq!(
            db.get_draft(&draft.id).unwrap().unwrap().status,
            DraftStatus::Draft,
            "failed claim must not change status"
        );

        // Operator blocks it: not claimable even at the current revision.
        let current = db.get_draft(&draft.id).unwrap().unwrap();
        db.update_draft_status(&draft.id, DraftStatus::Blocked)
            .unwrap();
        assert!(
            db.claim_draft_for_sending(&draft.id, current.revision)
                .unwrap()
                .is_none()
        );

        // A future send_after is not claimable either.
        db.update_draft_status(&draft.id, DraftStatus::Draft)
            .unwrap();
        db.update_draft_send_after(&draft.id, "2999-01-01T00:00:00Z")
            .unwrap();
        let future = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(
            db.claim_draft_for_sending(&draft.id, future.revision)
                .unwrap()
                .is_none()
        );
    }

    /// A claimed (`sending`) row is immutable to every content/recipient/
    /// attachment/metadata/status/schedule mutation primitive — the editable
    /// predicate lives inside each UPDATE statement, so even an interleaving
    /// where the claim lands between a caller's pre-read and its write cannot
    /// mutate the authoritative transmission snapshot. Every refusal leaves
    /// revision, content, and approval byte-identical.
    #[test]
    fn claimed_draft_is_atomically_immutable_to_every_mutation_primitive() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Queued"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.set_draft_metadata(&draft.id, &serde_json::json!({"in_reply_to": "<p@x>"}))
            .unwrap();
        db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00Z")
            .unwrap();
        let before = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(
            db.claim_draft_for_sending(&draft.id, before.revision)
                .unwrap()
                .is_some()
        );
        let claimed = db.get_draft(&draft.id).unwrap().unwrap();

        // Content edit — both the revision-guarded human path (with the
        // CURRENT revision, so only the status predicate can refuse it) and
        // the unconditional agent path.
        assert!(matches!(
            db.update_draft_content_for_revision(
                &draft.id,
                claimed.revision,
                Some("attacker@evil.example"),
                None,
                None,
                None,
                Some("swapped"),
                None,
            )
            .unwrap_err(),
            StoreError::DraftNotEditable(_)
        ));
        assert!(matches!(
            db.update_draft_content(&draft.id, None, None, None, None, Some("swapped"), None)
                .unwrap_err(),
            StoreError::DraftNotEditable(_)
        ));

        // Attachments, metadata, schedule, status flip, approval, queue.
        assert!(matches!(
            db.update_draft_attachments(
                &draft.id,
                &[serde_json::json!({"filename": "x", "content_type": "t", "size": 1, "data_base64": "eA=="})],
            )
            .unwrap_err(),
            StoreError::DraftNotEditable(_)
        ));
        assert!(matches!(
            db.set_draft_metadata(&draft.id, &serde_json::json!({"in_reply_to": "<q@x>"}))
                .unwrap_err(),
            StoreError::DraftNotEditable(_)
        ));
        assert!(matches!(
            db.update_draft_send_after(&draft.id, "2999-01-01T00:00:00Z")
                .unwrap_err(),
            StoreError::DraftNotEditable(_)
        ));
        assert!(matches!(
            db.update_draft_status(&draft.id, DraftStatus::Draft)
                .unwrap_err(),
            StoreError::DraftNotEditable(_)
        ));
        assert!(matches!(
            db.record_draft_human_approval(
                &draft.id,
                claimed.revision,
                "human:dashboard",
                "2026-07-10T09:00:00Z",
            )
            .unwrap_err(),
            StoreError::DraftNotEditable(_)
        ));
        assert!(matches!(
            db.queue_draft_with_human_send(
                &draft.id,
                claimed.revision,
                "2026-07-10T09:02:00Z",
                "human:dashboard",
                "2026-07-10T09:00:00Z",
            )
            .unwrap_err(),
            StoreError::DraftNotEditable(_)
        ));

        // IMAP bookkeeping is part of the snapshot's cleanup identity and is
        // equally immutable under a `sending` claim.
        assert!(matches!(
            db.update_draft_imap_uid(&draft.id, 999).unwrap_err(),
            StoreError::DraftNotEditable(_)
        ));
        assert!(matches!(
            db.mark_draft_message_id(&draft.id, "<other@x>")
                .unwrap_err(),
            StoreError::DraftNotEditable(_)
        ));

        // The authoritative transmission snapshot is byte-identical.
        let after = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(after.status, DraftStatus::Sending);
        assert_eq!(after.revision, claimed.revision);
        assert_eq!(after.to_addr, claimed.to_addr);
        assert_eq!(after.text_content, claimed.text_content);
        assert_eq!(after.metadata, claimed.metadata);
        assert_eq!(after.attachments, claimed.attachments);
        assert_eq!(after.send_after, claimed.send_after);
        assert!(!after.human_approved());
        assert!(
            !db.list_drafts_due_for_send()
                .unwrap()
                .iter()
                .any(|d| d.id == draft.id)
        );
    }

    /// Cross-path exclusion: the scheduled sweep and an immediate CLI/MCP
    /// send compete for the SAME durable `sending` claim — exactly one wins,
    /// in either order.
    #[test]
    fn scheduled_and_immediate_send_claims_are_mutually_exclusive() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Queued"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00Z")
            .unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;

        // Sweep claims first → immediate send must lose.
        let sweep_lease = db
            .claim_draft_for_sending(&draft.id, rev)
            .unwrap()
            .expect("sweep claim");
        assert!(
            db.claim_draft_for_immediate_send(&draft.id, rev)
                .unwrap()
                .is_none()
        );
        assert!(
            db.release_sending_draft(&draft.id, &sweep_lease, DraftStatus::Draft)
                .unwrap()
        );

        // Immediate send claims first → sweep must lose.
        let send_lease = db
            .claim_draft_for_immediate_send(&draft.id, rev)
            .unwrap()
            .expect("immediate claim");
        assert!(
            db.claim_draft_for_sending(&draft.id, rev)
                .unwrap()
                .is_none()
        );
        assert!(
            db.release_sending_draft(&draft.id, &send_lease, DraftStatus::Draft)
                .unwrap()
        );

        // Immediate claim also rejects a stale revision and non-`draft` states.
        db.update_draft_content(&draft.id, None, None, None, None, Some("edited"), None)
            .unwrap();
        assert!(
            db.claim_draft_for_immediate_send(&draft.id, rev)
                .unwrap()
                .is_none(),
            "stale revision must lose the immediate claim"
        );
        let current = db.get_draft(&draft.id).unwrap().unwrap();
        db.update_draft_status(&draft.id, DraftStatus::PendingReview)
            .unwrap();
        assert!(
            db.claim_draft_for_immediate_send(&draft.id, current.revision)
                .unwrap()
                .is_none(),
            "a pending_review draft must not be immediately sendable"
        );
    }

    /// The provider-sync (`modify`) claim excludes the send sweep for its
    /// whole duration, restores the prior status on release, and keeps the
    /// row inert for everyone else — while permitting the holder's IMAP
    /// bookkeeping and storage-metadata finalization.
    #[test]
    fn sync_claim_excludes_send_claims_and_restores_prior_status() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Queued"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00Z")
            .unwrap();
        db.update_draft_status(&draft.id, DraftStatus::PendingReview)
            .unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;

        // Stale revision loses the sync claim.
        assert!(
            db.claim_draft_for_sync(&draft.id, rev + 1)
                .unwrap()
                .is_none()
        );

        let claim = db
            .claim_draft_for_sync(&draft.id, rev)
            .unwrap()
            .expect("sync claim");
        assert_eq!(claim.prior_status, DraftStatus::PendingReview);
        assert!(db.holds_sync_claim(&draft.id, &claim.token).unwrap());
        assert_eq!(
            db.get_draft(&draft.id).unwrap().unwrap().status,
            DraftStatus::Syncing
        );

        // Neither send path can claim a syncing row, a second sync loses, and
        // it is not due.
        assert!(
            db.claim_draft_for_sending(&draft.id, rev)
                .unwrap()
                .is_none()
        );
        assert!(
            db.claim_draft_for_immediate_send(&draft.id, rev)
                .unwrap()
                .is_none()
        );
        assert!(db.claim_draft_for_sync(&draft.id, rev).unwrap().is_none());
        assert!(
            !db.list_drafts_due_for_send()
                .unwrap()
                .iter()
                .any(|d| d.id == draft.id)
        );

        // Other actors cannot mutate content/schedule/status/approval…
        assert!(
            db.update_draft_content(&draft.id, None, None, None, None, Some("x"), None)
                .is_err()
        );
        assert!(db.update_draft_attachments(&draft.id, &[]).is_err());
        assert!(
            db.update_draft_send_after(&draft.id, "2999-01-01T00:00:00Z")
                .is_err()
        );
        assert!(
            db.update_draft_status(&draft.id, DraftStatus::Draft)
                .is_err()
        );
        assert!(
            db.record_draft_human_approval(
                &draft.id,
                rev,
                "human:dashboard",
                "2026-07-10T09:00:00Z"
            )
            .is_err()
        );
        // Generic UID/Message-ID bookkeeping is refused too — only the
        // token-checked holder variants may write while syncing.
        assert!(db.update_draft_imap_uid(&draft.id, 4242).is_err());
        assert!(db.mark_draft_message_id(&draft.id, "<x@x>").is_err());
        assert!(
            db.set_draft_metadata(&draft.id, &serde_json::json!({}))
                .is_err()
        );

        // The token-checked holder path applies the full edit atomically and
        // finalizes bookkeeping; a non-owner token is refused everywhere.
        let edited = db
            .apply_synced_draft_edit(
                &draft.id,
                &claim.token,
                "new-to@test.com",
                None,
                None,
                "Edited subject",
                "edited body",
                None,
                &[],
                &serde_json::json!({"in_reply_to": "<p@x>"}),
            )
            .unwrap();
        assert_eq!(edited.to_addr, "new-to@test.com");
        assert_eq!(edited.revision, rev + 1);
        assert!(
            db.apply_synced_draft_edit(
                &draft.id,
                "not-the-owner",
                "attacker@evil.example",
                None,
                None,
                "x",
                "x",
                None,
                &[],
                &serde_json::json!({}),
            )
            .is_err()
        );
        assert!(
            db.finalize_synced_draft_bookkeeping(
                &draft.id,
                "not-the-owner",
                Some(1),
                &serde_json::json!({}),
            )
            .is_err()
        );
        assert!(!db.holds_sync_claim(&draft.id, "not-the-owner").unwrap());
        db.finalize_synced_draft_bookkeeping(
            &draft.id,
            &claim.token,
            Some(4242),
            &serde_json::json!({"imap_synced": true}),
        )
        .unwrap();
        let finalized = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(finalized.imap_uid, Some(4242));
        assert_eq!(
            finalized.metadata.as_ref().unwrap()["storage"]["imap_synced"],
            true
        );

        // Release restores the prior status (owner token required, cleared on
        // release); releasing again is a no-op.
        assert!(
            !db.release_syncing_draft(&draft.id, "not-the-owner", DraftStatus::Draft)
                .unwrap()
        );
        assert!(
            db.release_syncing_draft(&draft.id, &claim.token, DraftStatus::PendingReview)
                .unwrap()
        );
        assert_eq!(
            db.get_draft(&draft.id).unwrap().unwrap().status,
            DraftStatus::PendingReview
        );
        assert!(
            !db.release_syncing_draft(&draft.id, &claim.token, DraftStatus::Draft)
                .unwrap()
        );
    }

    /// Regression: an SMTP-accepted-but-unrecorded send parks as the terminal
    /// `delivery_uncertain` state. It must be rejected by dashboard approval
    /// and queueing, excluded by the due selector (send_after cleared
    /// atomically under the lease), unclaimable, and recoverable only through
    /// the explicit operator reconciliation (discard) — never approval.
    #[test]
    fn delivery_uncertain_park_is_terminal_and_never_resendable() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Queued"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00Z")
            .unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        let lease = db
            .claim_draft_for_sending(&draft.id, rev)
            .unwrap()
            .expect("claim");

        // A non-owner cannot park.
        assert!(
            !db.park_delivery_uncertain(&draft.id, "not-the-owner")
                .unwrap()
        );
        assert_eq!(
            db.get_draft(&draft.id).unwrap().unwrap().status,
            DraftStatus::Sending
        );

        // The owner parks: terminal state, send_after and token cleared in the
        // same atomic statement.
        assert!(db.park_delivery_uncertain(&draft.id, &lease).unwrap());
        let parked = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(parked.status, DraftStatus::DeliveryUncertain);
        assert!(
            parked.send_after.is_none(),
            "send_after must be cleared atomically with the park"
        );
        assert!(!parked.status.is_editable());

        // Approval cannot promote it back to a sendable draft…
        assert!(matches!(
            db.approve_draft_revision(
                &draft.id,
                parked.revision,
                "human:dashboard",
                "2026-07-10T09:00:00Z",
            )
            .unwrap_err(),
            StoreError::DraftNotEditable(_)
        ));
        // …nor can the dashboard/CLI queue, edit, re-schedule, or attest it.
        assert!(matches!(
            db.queue_draft_with_human_send(
                &draft.id,
                parked.revision,
                "2026-07-10T09:02:00Z",
                "human:dashboard",
                "2026-07-10T09:00:00Z",
            )
            .unwrap_err(),
            StoreError::DraftNotEditable(_)
        ));
        assert!(
            db.update_draft_content(&draft.id, None, None, None, None, Some("x"), None)
                .is_err()
        );
        assert!(
            db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00Z")
                .is_err()
        );
        assert!(
            db.record_draft_human_approval(
                &draft.id,
                parked.revision,
                "human:dashboard",
                "2026-07-10T09:00:00Z"
            )
            .is_err()
        );

        // Never due, never claimable — by either send path or a sync.
        assert!(
            !db.list_drafts_due_for_send()
                .unwrap()
                .iter()
                .any(|d| d.id == draft.id)
        );
        assert!(
            db.claim_draft_for_sending(&draft.id, parked.revision)
                .unwrap()
                .is_none()
        );
        assert!(
            db.claim_draft_for_immediate_send(&draft.id, parked.revision)
                .unwrap()
                .is_none()
        );
        assert!(
            db.claim_draft_for_sync(&draft.id, parked.revision)
                .unwrap()
                .is_none()
        );

        // The dead lease is cleared: it cannot act on the parked row.
        assert!(!db.park_delivery_uncertain(&draft.id, &lease).unwrap());
        assert!(
            !db.release_sending_draft(&draft.id, &lease, DraftStatus::Draft)
                .unwrap()
        );

        // Explicit operator reconciliation: discard works and stays terminal.
        assert!(db.discard_draft(&draft.id).unwrap());
        assert_eq!(
            db.get_draft(&draft.id).unwrap().unwrap().status,
            DraftStatus::Discarded
        );
    }

    /// Releases move a claim to retry or park states and never clobber
    /// anything else; a released-to-draft row becomes due again.
    #[test]
    fn release_sending_transitions_only_claimed_rows() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Queued"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00Z")
            .unwrap();

        // Releasing an unclaimed row is a no-op.
        assert!(
            !db.release_sending_draft(&draft.id, "no-lease", DraftStatus::Blocked)
                .unwrap()
        );
        assert_eq!(
            db.get_draft(&draft.id).unwrap().unwrap().status,
            DraftStatus::Draft
        );

        // Claim → transient failure → release to draft → due again (retry).
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        let lease = db
            .claim_draft_for_sending(&draft.id, rev)
            .unwrap()
            .expect("claim");
        // A non-owner token can neither release nor mark sent; nothing moves.
        assert!(
            !db.release_sending_draft(&draft.id, "not-the-owner", DraftStatus::Draft)
                .unwrap()
        );
        assert!(
            db.mark_draft_sent(&draft.id, "not-the-owner", Some("<m@x>"))
                .is_err()
        );
        assert_eq!(
            db.get_draft(&draft.id).unwrap().unwrap().status,
            DraftStatus::Sending
        );
        assert!(
            db.release_sending_draft(&draft.id, &lease, DraftStatus::Draft)
                .unwrap()
        );
        assert!(
            db.list_drafts_due_for_send()
                .unwrap()
                .iter()
                .any(|d| d.id == draft.id)
        );
        // The token was cleared on release: a dead lease cannot act on a new
        // claim.
        let lease2 = db
            .claim_draft_for_sending(&draft.id, rev)
            .unwrap()
            .expect("re-claim after release");
        assert!(
            !db.release_sending_draft(&draft.id, &lease, DraftStatus::Draft)
                .unwrap(),
            "a released lease must not act on a new claim"
        );

        // Claim → durable verdict → park to pending_review → not due.
        assert!(
            db.release_sending_draft(&draft.id, &lease2, DraftStatus::PendingReview)
                .unwrap()
        );
        assert_eq!(
            db.get_draft(&draft.id).unwrap().unwrap().status,
            DraftStatus::PendingReview
        );
        assert!(
            !db.list_drafts_due_for_send()
                .unwrap()
                .iter()
                .any(|d| d.id == draft.id)
        );
    }

    #[test]
    fn discard_draft() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Sub"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        assert!(db.discard_draft(&draft.id).unwrap());
        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(fetched.status, DraftStatus::Discarded);
    }

    #[test]
    fn mark_draft_sent_sets_status_and_sent_at() {
        // Regression for the stale-draft incident: a successful send must move
        // the local row to status=sent AND stamp sent_at. A draft that has been
        // "sent" must never remain status=draft with a null sent_at.
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(draft.status, DraftStatus::Draft);
        assert!(draft.sent_at.is_none());

        // Ownership rule: only a held `sending` claim can be marked sent. An
        // actor that never claimed (or lost the claim) must be refused.
        assert!(matches!(
            db.mark_draft_sent(&draft.id, "never-claimed", Some("<mid@host>"))
                .unwrap_err(),
            StoreError::DraftNotEditable(_)
        ));
        let lease = db
            .claim_draft_for_immediate_send(&draft.id, draft.revision)
            .unwrap()
            .expect("claim");
        // The owner's lease — and only it — marks the row sent (and clears
        // the token on the terminal state).
        assert!(
            db.mark_draft_sent(&draft.id, "not-the-owner", Some("<mid@host>"))
                .is_err()
        );
        db.mark_draft_sent(&draft.id, &lease, Some("<mid@host>"))
            .unwrap();

        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(fetched.status, DraftStatus::Sent);
        assert!(
            fetched.sent_at.is_some(),
            "sent_at must be stamped after a successful send"
        );
        assert_eq!(fetched.message_id.as_deref(), Some("<mid@host>"));
    }

    #[test]
    fn cooldown_queue_defers_send_and_stays_draft() {
        // Regression for the "agents send too fast" incident: an allowed send
        // queues into the outbox with a FUTURE send_after instead of
        // transmitting immediately. Until the cooldown elapses, the draft is
        // NOT returned by the scheduled-send sweep and remains status=draft
        // (never sent). A due draft (past send_after) IS returned.
        let db = setup();
        let queued = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Queued"),
                Some("Body"),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        // 1 hour in the future — well past any reasonable default cooldown.
        let future = (chrono::Utc::now() + chrono::Duration::hours(1))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        db.update_draft_send_after(&queued.id, &future).unwrap();

        let due = db.list_drafts_due_for_send().unwrap();
        assert!(
            !due.iter().any(|d| d.id == queued.id),
            "a queued draft within its cooldown must not be due for send"
        );
        let fetched = db.get_draft(&queued.id).unwrap().unwrap();
        assert_eq!(fetched.status, DraftStatus::Draft);
        assert!(fetched.sent_at.is_none());
        assert_eq!(fetched.send_after.as_deref(), Some(future.as_str()));

        // Once the cooldown has elapsed (past send_after), the sweep sees it.
        let past = (chrono::Utc::now() - chrono::Duration::minutes(1))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        db.update_draft_send_after(&queued.id, &past).unwrap();
        let due = db.list_drafts_due_for_send().unwrap();
        assert!(
            due.iter().any(|d| d.id == queued.id),
            "a draft past its cooldown must be due for send"
        );
    }

    #[test]
    fn update_imap_uid() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        assert_eq!(draft.imap_uid, None);

        db.update_draft_imap_uid(&draft.id, 42).unwrap();
        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(fetched.imap_uid, Some(42));
    }

    #[test]
    fn get_draft_by_imap_uid_prioritizes_active_local_draft() {
        let db = setup();
        let active = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Active"),
                Some("Body"),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        db.update_draft_imap_uid(&active.id, 38103).unwrap();

        let discarded = db
            .create_draft(
                "acc1",
                "old@test.com",
                Some("Old"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        db.update_draft_imap_uid(&discarded.id, 38103).unwrap();
        db.discard_draft(&discarded.id).unwrap();

        let found = db.get_draft_by_imap_uid("acc1", 38103).unwrap().unwrap();
        assert_eq!(found.id, active.id);
        assert_eq!(found.imap_uid, Some(38103));
        assert!(db.get_draft_by_imap_uid("acc1", 999).unwrap().is_none());
    }

    #[test]
    fn mark_message_id() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        assert_eq!(draft.message_id, None);

        db.mark_draft_message_id(&draft.id, "<test@example.com>")
            .unwrap();
        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(fetched.message_id, Some("<test@example.com>".to_string()));
    }

    #[test]
    fn count_active_drafts_excludes_sent_and_discarded() {
        let db = setup();

        // Create two active drafts
        let d1 = db
            .create_draft(
                "acc1",
                "a@test.com",
                Some("Active"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let _d2 = db
            .create_draft(
                "acc1",
                "b@test.com",
                Some("Active2"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        // Promote one to pending_review
        db.update_draft_status(&d1.id, DraftStatus::PendingReview)
            .unwrap();

        // Create a sent draft and a discarded draft (historical, should not count)
        let sent = db
            .create_draft(
                "acc1",
                "c@test.com",
                Some("Sent"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let lease = db
            .claim_draft_for_immediate_send(&sent.id, sent.revision)
            .unwrap()
            .expect("claim");
        db.mark_draft_sent(&sent.id, &lease, None).unwrap();

        let discard = db
            .create_draft(
                "acc1",
                "d@test.com",
                Some("Discarded"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        db.discard_draft(&discard.id).unwrap();

        // Only the two active drafts (draft + pending_review) should count
        let count = db.count_active_drafts(None).unwrap();
        assert_eq!(count, 2);

        // Account-scoped count matches
        let scoped = db.count_active_drafts(Some("acc1")).unwrap();
        assert_eq!(scoped, 2);
    }

    #[test]
    fn set_and_read_draft_metadata_round_trips() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("S"),
                Some("B"),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        let meta = serde_json::json!({
            "draft_kind": "reply",
            "source": {"folder": "INBOX", "uid": 42, "message_id": "parent@x"},
            "references": ["a@x", "parent@x"],
            "signature_applied": false,
        });
        db.set_draft_metadata(&draft.id, &meta).unwrap();

        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        let stored = fetched.metadata.expect("metadata persisted");
        assert_eq!(stored["draft_kind"], "reply");
        assert_eq!(stored["source"]["uid"], 42);
        assert_eq!(stored["references"][1], "parent@x");
    }

    #[test]
    fn update_draft_attachments_round_trips() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        assert!(draft.attachments.is_empty());

        let attachments = vec![
            serde_json::json!({
                "filename": "packet.txt",
                "content_type": "text/plain",
                "size": 5,
                "data_base64": "aGVsbG8=",
            }),
            serde_json::json!({
                "filename": "report.pdf",
                "content_type": "application/pdf",
                "size": 3,
                "data_base64": "Zm9v",
            }),
        ];
        db.update_draft_attachments(&draft.id, &attachments)
            .unwrap();

        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(fetched.attachments.len(), 2);
        assert_eq!(fetched.attachments[0]["filename"], "packet.txt");
        assert_eq!(fetched.attachments[0]["size"], 5);
        assert_eq!(fetched.attachments[1]["data_base64"], "Zm9v");
    }

    /// The revision-guarded variant is what interactive surfaces use: the
    /// operator is looking at a rendered list, so a stale view must lose the
    /// race instead of writing back an array rebuilt from what it last saw.
    #[test]
    fn update_draft_attachments_for_revision_enforces_the_caller_view() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        let one = vec![serde_json::json!({
            "filename": "one.txt",
            "content_type": "text/plain",
            "size": 1,
            "data_base64": "YQ==",
        })];
        let updated = db
            .update_draft_attachments_for_revision(&draft.id, draft.revision, &one)
            .unwrap();
        assert_eq!(updated.attachments.len(), 1);
        assert_eq!(
            updated.revision,
            draft.revision + 1,
            "an attachment change is an edit and bumps the revision"
        );

        // A second writer still holding the pre-update revision is refused, and
        // its view of the array (empty) is not written back.
        let err = db
            .update_draft_attachments_for_revision(&draft.id, draft.revision, &[])
            .unwrap_err();
        assert!(
            matches!(err, StoreError::DraftModifiedConcurrently(_)),
            "stale revision must conflict, got {err:?}"
        );
        assert_eq!(
            db.get_draft(&draft.id).unwrap().unwrap().attachments.len(),
            1
        );
    }

    /// Changing what will be attached invalidates the approval that was given
    /// for the previous set — same contract as a body edit.
    #[test]
    fn update_draft_attachments_for_revision_drops_the_approval() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        // `set_draft_metadata` strips `human_approval` by design — the
        // attestation only exists via the atomic queue path, so that is what
        // this test has to go through.
        let approved = db
            .queue_draft_with_human_send(
                &draft.id,
                draft.revision,
                "2026-08-18T12:00:00Z",
                "human:dashboard",
                "2026-08-18T11:59:00Z",
            )
            .unwrap();
        assert!(
            approved
                .metadata
                .as_ref()
                .and_then(|m| m.get("human_approval"))
                .is_some(),
            "the queue path must record the attestation this test then invalidates"
        );

        let updated = db
            .update_draft_attachments_for_revision(
                &approved.id,
                approved.revision,
                &[serde_json::json!({
                    "filename": "late.txt",
                    "content_type": "text/plain",
                    "size": 1,
                    "data_base64": "YQ==",
                })],
            )
            .unwrap();
        assert!(
            updated
                .metadata
                .as_ref()
                .and_then(|m| m.get("human_approval"))
                .is_none(),
            "attaching after approval must invalidate it"
        );
    }

    #[test]
    fn list_drafts_includes_imap_uid() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        db.update_draft_imap_uid(&draft.id, 99).unwrap();

        let drafts = db.list_drafts("acc1", Some("draft"), 100, 0).unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].imap_uid, Some(99));
    }

    // ── Hold: unqueue without discarding ──────────────────────────────
    //
    // Cancelling a scheduled send used to mean discarding the draft, so an
    // operator who only wanted to stop the clock and finish the message later
    // had to lose it. Hold is the non-destructive verb: the schedule goes, the
    // draft stays.

    /// Queue a fresh draft `send_after` seconds-from-nothing (a literal
    /// timestamp) and return it.
    fn queued_draft(db: &Database, send_after: &str) -> Draft {
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&draft.id, send_after).unwrap();
        db.get_draft(&draft.id).unwrap().unwrap()
    }

    #[test]
    fn hold_clears_send_after_and_keeps_an_editable_draft() {
        let db = setup();
        let queued = queued_draft(&db, "2030-01-01T00:00:00");

        let held = db.hold_scheduled_draft(&queued.id).unwrap();

        assert!(held.send_after.is_none(), "the schedule must be cleared");
        assert_eq!(
            held.status,
            DraftStatus::Draft,
            "hold must not discard or park the draft"
        );
        assert!(held.status.is_editable());
        // The message itself is untouched — that is the whole point of hold.
        assert_eq!(held.to_addr, "to@test.com");
        assert_eq!(held.subject.as_deref(), Some("Subject"));
        assert_eq!(held.text_content.as_deref(), Some("Body"));
        // No content changed, so an open editor's expected_revision stays good.
        assert_eq!(held.revision, queued.revision);
    }

    #[test]
    fn a_held_draft_is_no_longer_due_for_the_send_sweep() {
        let db = setup();
        // Already due: without the hold this is exactly what the sweep sends.
        let queued = queued_draft(&db, "2000-01-01T00:00:00");
        assert_eq!(db.list_drafts_due_for_send().unwrap().len(), 1);

        db.hold_scheduled_draft(&queued.id).unwrap();

        assert!(
            db.list_drafts_due_for_send().unwrap().is_empty(),
            "a held draft must be invisible to the scheduled-send sweep"
        );
    }

    #[test]
    fn hold_withdraws_the_human_approval_attestation() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        let attested = db
            .queue_draft_with_human_send(
                &draft.id,
                draft.revision,
                "2030-01-01T00:00:00",
                "human:dashboard",
                "2026-07-10T09:00:00Z",
            )
            .unwrap();
        assert!(attested.human_approved());

        let held = db.hold_scheduled_draft(&draft.id).unwrap();

        assert!(
            !held.human_approved(),
            "the approval authorized this send; holding withdraws it"
        );
        assert!(
            held.metadata
                .as_ref()
                .is_none_or(|m| m.get("human_approval").is_none())
        );
    }

    #[test]
    fn hold_preserves_unrelated_draft_metadata() {
        let db = setup();
        let queued = queued_draft(&db, "2030-01-01T00:00:00");
        db.conn()
            .execute(
                "UPDATE drafts SET metadata = json('{\"reply_to_uid\": 42}') WHERE id = ?1",
                params![queued.id],
            )
            .unwrap();

        let held = db.hold_scheduled_draft(&queued.id).unwrap();

        assert_eq!(held.metadata.as_ref().unwrap()["reply_to_uid"], 42);
    }

    #[test]
    fn hold_refuses_a_draft_the_send_sweep_already_claimed() {
        let db = setup();
        let queued = queued_draft(&db, "2000-01-01T00:00:00");
        let token = db
            .claim_draft_for_sending(&queued.id, queued.revision)
            .unwrap();
        assert!(token.is_some(), "the sweep must win the claim first");

        let err = db.hold_scheduled_draft(&queued.id).unwrap_err();

        assert!(
            matches!(err, StoreError::DraftNotEditable(ref s) if s == "sending"),
            "a send already in flight must not be yanked back: {err}"
        );
        // The claim is intact — hold changed nothing.
        let after = db.get_draft(&queued.id).unwrap().unwrap();
        assert_eq!(after.status, DraftStatus::Sending);
        assert_eq!(after.send_after.as_deref(), Some("2000-01-01T00:00:00"));
    }

    #[test]
    fn hold_refuses_a_draft_that_was_never_queued() {
        let db = setup();
        let draft = db
            .create_draft(
                "acc1",
                "to@test.com",
                Some("Subject"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();

        let err = db.hold_scheduled_draft(&draft.id).unwrap_err();

        assert!(
            matches!(err, StoreError::DraftNotScheduled(_)),
            "an unqueued draft must not report a schedule was cleared: {err}"
        );
    }

    #[test]
    fn hold_refuses_a_missing_draft() {
        let db = setup();
        let err = db.hold_scheduled_draft("no-such-draft").unwrap_err();
        assert!(matches!(err, StoreError::DraftNotFound(_)), "{err}");
    }

    #[test]
    fn hold_refuses_a_discarded_draft() {
        let db = setup();
        let queued = queued_draft(&db, "2030-01-01T00:00:00");
        assert!(db.discard_draft(&queued.id).unwrap());

        let err = db.hold_scheduled_draft(&queued.id).unwrap_err();

        assert!(
            matches!(err, StoreError::DraftNotEditable(ref s) if s == "discarded"),
            "{err}"
        );
    }

    /// Hold is reversible: the operator edits the held draft and queues it
    /// again, which re-attests the approval the hold withdrew.
    #[test]
    fn a_held_draft_can_be_edited_and_re_queued() {
        let db = setup();
        let queued = queued_draft(&db, "2030-01-01T00:00:00");
        let held = db.hold_scheduled_draft(&queued.id).unwrap();

        let edited = db
            .update_draft_content_for_revision(
                &held.id,
                held.revision,
                None,
                None,
                None,
                Some("Second thoughts"),
                Some("Rewritten body"),
                None,
            )
            .unwrap();
        assert_eq!(edited.subject.as_deref(), Some("Second thoughts"));
        assert!(
            edited.send_after.is_none(),
            "editing a held draft must not resurrect the schedule"
        );

        let requeued = db
            .queue_draft_with_human_send(
                &edited.id,
                edited.revision,
                "2030-06-01T00:00:00",
                "human:dashboard",
                "2026-07-10T09:00:00Z",
            )
            .unwrap();
        assert_eq!(requeued.send_after.as_deref(), Some("2030-06-01T00:00:00"));
        assert!(requeued.human_approved());
    }
}
