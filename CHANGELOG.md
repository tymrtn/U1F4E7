# Changelog

All notable changes to Envelope Email are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased] — 1.0.22-dev

### Added

- A queued draft's review page now counts down. The banner leads with the time actually remaining — `45s`, `12m 30s`, `2h 05m`, `3d 4h` — ticking every second and rolling over to `due now` once the send time passes, with the wall-clock send time kept beside it as secondary text. Until now the page showed only a clock time, which made a `--at` schedule days out and the 60-second safety cooldown read identically.
- Hold: a queued draft can be taken back out of the outbox without being destroyed. `send_after` is cleared, the row stays a `draft`, and the review composer unlocks so the message can be finished and re-queued later. The human-approval attestation is withdrawn along with the schedule, since it authorized the send that was just called off; re-queueing re-attests it. Available as a control on the queued banner, as `POST /api/accounts/{id}/drafts/{draftId}/hold`, and as `envelope scheduled hold <id>`.
- The queued banner links to `/cockpit#scheduled-panel`, so the whole outbox is one click from any individual queued draft.

### Changed

- `envelope scheduled cancel` is unchanged and still discards the draft — it is now documented as the destructive verb, with `hold` as the one to reach for when the message is still wanted and only the timing is wrong.
- Holding races the scheduled-send sweep honestly. The store guards on `status = 'draft'` inside the UPDATE, so once the sweep has claimed a row for transmission the hold matches nothing and returns 409 rather than appearing to stop a send already in flight. The dashboard surfaces that as "already started sending, or no longer queued" with a reload, never as the revision-conflict banner.

### Added

- Opening a message now shows where it sits. The list header reads `4 of 50` instead of a bare count while a message is open, the selected row scrolls into view when a deep link lands on something below the fold, and the left rail marks the account the message actually belongs to (`open here`) rather than only highlighting the smart mailbox you are browsing. The unified list merges every account, so until now nothing on screen said which mailbox you were reading from.
- A reader link that lost its `?folder=` resolves the mailbox from the list instead of guessing INBOX. The unified list records the folder for every row it paints; the reader consults that when the query string says nothing. An explicit `?folder=` still wins, since only the URL can name a mailbox the list never loaded.
- The draft review composer renders HTML mail. A draft carrying an HTML part now opens on it, rendered through the reader's sandboxed `BodyFrame` — same `srcdoc` iframe, same CSP, `allow-scripts` still absent — with remote images blocked until asked for and the markup behind an `Edit HTML` toggle. Review used to land on the plain-text alternative, which shows bare tracking URLs where the real message has buttons, and reaching the HTML only produced its source.

### Fixed

- Draft review now shows the effective send-as identity from `metadata.from`, with the transport account only as a fallback, and repeats that From identity in the final queue confirmation. The send path already preserved this header, but the dashboard displayed only the authenticating mailbox, making a correctly branded draft look unsafe to approve.

- A draft parked for attribution no longer offers a button that cannot clear it. Approving a bot-attributed draft again re-ran the identical declaration-free attempt: it spent another try and re-parked on the same reason, with the only surface that reported the stop being the one unable to lift it. The banner now names what the park record holds — bot attribution, attempts spent, no fact labels declared for this revision — withholds `Send again` when re-approval provably cannot help, and points at `envelope draft send … --attr`, which is where declaring happens. Which label would pass is deliberately not shown: the park record carries no such field, and naming one would turn a blind declaration into lock-picking.
- The composer's Text/HTML control swaps the body along with the label. It set the format flag and left whatever was already in the box, so a draft carrying both alternatives — every agent-generated HTML message — opened in plain text and then showed that same plain text under an HTML heading. A format switch alone marks the draft dirty, so the next Save wrote the plain-text body into `html_content` and cleared `text_content`: a real HTML part replaced by its text twin, one click from the review screen. Each format now keeps its own buffer, so switching shows that format's body and switching back returns an unsaved edit rather than the server copy.
- The unified inbox heals itself when its cache has been blanked. An account whose index row carries a `last_error` reports zero messages however many rows are actually indexed behind it, so a sidecar started without `ENVELOPE_MASTER_PASSPHRASE_FILE` — a GUI launch inherits no shell environment — could write a credential-store error into the SHARED index for every account and leave the dashboard showing "Inbox is empty" over a full index. The stale-refresh predicate only fired on `stale`/`expired`, never `unavailable`, so it never retried: one such incident sat for a day. An empty list with connected accounts now triggers one refresh to disprove itself, while the steady state of a couple of permanently-unreachable accounts still does not re-IMAP the fleet on every open.
- The `hold` endpoint stripped no attachment bytes. `draft_json` landed before Hold existed, so the new handler serialized the raw store row and shipped every attachment's `data_base64` on each hold. It now routes through `draft_json` like every other draft response, and a guard test fails the build if any future handler serializes a raw draft.
- The Governor gate is skipped only for sends a human actually authored. `1.0.14-dev` reduced the scheduled-send attribution rule to `require_declaration = !human_approved`, dropping the `scheduled_origin` provenance check, so an agent-drafted message skipped attribution scoring entirely the moment an operator clicked Approve. Origin is decided from durable provenance again: only a `human:*`-originated draft carrying a current revision-bound attestation lifts the bot-declaration requirement. Agent, `mcp`, `cli`, and unknown-provenance rows still require their declaration after human approval — approval supplements a bot's attribution responsibility, it never replaces it. Dashboard- and Tauri-composed mail is stamped `human:dashboard` by `compose.rs`, so operator-written sends still bypass the review park the countdown work was written to fix.

- Drafts deep links now open the draft review composer instead of the read-only reader. A message link whose folder is a Drafts mailbox (`Drafts`, `[Gmail]/Drafts`, `INBOX.Drafts`, and the other spellings `classify_folder` recognizes) resolves through the local draft that carries that IMAP UID, so `envelope read`, `envelope draft list`, `envelope inbox`, `envelope search`, and the matching MCP tools emit `/accounts/{account}/drafts/{draft}` on both `review_url` and `message_url`. Historical `/accounts/{account}/messages/{uid}?folder=Drafts` links 308 to the same review path. Everything outside a Drafts folder keeps the canonical reader route `/mail/unified/{account}/{uid}?folder={folder}`.
- Opening a Drafts UID in the dashboard no longer loads the message endpoint or marks the draft read. The reader hands off to the review composer before fetching anything; a Drafts UID with no local draft row renders a draft card explaining that there is no editable copy, rather than the SvelteKit 404 or a read-only message with no Send.

### Compatibility

- The `ui` object's keys and types are unchanged, so the agent contract schema id is unchanged. Only path *values* moved: a Drafts-folder `message_url` now carries the review path. A Drafts UID with no local draft still emits the reader URL.

## [1.0.12] — 2026-08-18

### Added

- Draft attachments are visible and editable on the draft review page. Each attachment renders as a chip carrying its filename, media type, and size, with the panel summarising the count and total; clicking a chip downloads that file, and the `×` on it detaches the file from the draft. `Attach files` and drag-and-drop add more, up to 25 MB per message — the ceiling is checked in the browser before a file is read and again server-side against the draft's running total. Previously this surface rendered a bare sentence ("3 attachments stay on this draft"), naming nothing and offering no way to open, add, or remove anything.
- New endpoints `POST /api/accounts/{id}/drafts/{draft_id}/attachments`, `DELETE /api/accounts/{id}/drafts/{draft_id}/attachments/{filename}`, and `GET /api/accounts/{id}/drafts/{draft_id}/attachments/{filename}`. Download streams from the bytes stored on the draft rather than from IMAP, because an unsent draft's files exist nowhere else; it sends `X-Content-Type-Options: nosniff` and inlines images only, so a mislabelled entry cannot render as active content on the dashboard's origin. Attaching and detaching are edits: both carry the `expected_revision` the operator was shown, bump the revision, and clear any human-approval attestation — a draft cannot pick up a file after approval and still ride that approval.
- Uploaded filenames are reduced to one path segment (`../../etc/passwd` attaches as `passwd`), and a name that collides with an existing attachment is suffixed `name (2).ext`. Downloads address an attachment by name, so a duplicate name would otherwise make one of the two files unreachable.
- Attaching a file no longer costs unsaved work. The review page adopts the server's new revision and attachment list without re-seeding the editor, so text typed but not yet saved survives an attach, and the following save carries the revision the attachment write produced rather than conflicting with it.

### Fixed

- Draft JSON no longer ships attachment bytes to the client. `GET /drafts`, `GET /drafts/{id}`, `by-imap-uid`, and the approve/edit/block responses all strip `data_base64` from each attachment entry, leaving filename, media type, and size. The review page was downloading every attachment in full on each load and rendering only a count — a draft carrying a 10 MB PDF moved 13 MB of base64 per fetch — and the field is one the store is explicit about never logging or echoing, which the CLI has honoured since its first attachment listing. Attachment bytes now leave the API through the download route alone.

## [1.0.11] — 2026-08-15

### Added

- Recipient autocomplete on every dashboard compose surface. To, Cc, and Bcc are now keyboard-first token fields that suggest people you have already corresponded with — arrow keys to move, Enter or Tab to accept, comma to commit a typed address, Backspace to drop the last chip, Escape to dismiss. Pasting a whole recipient list commits one chip per address and leaves anything unfinished in the field rather than dropping it. Suggestions are ranked by textual match strength first (exact, then prefix, then substring) and then by how often and how recently you have exchanged mail with that address.
- New read-only dashboard endpoint `GET /api/accounts/{id}/address-suggestions?q=…&limit=…`, returning at most 10 `{email, name}` rows for one account. It reads local address history only — no IMAP round-trip while typing — and carries no subjects, snippets, or bodies. A recipient learned from a Bcc line comes back as an ordinary row, with nothing marking it as one.
- Address history is reconciled into the existing `contacts` table from three caches already on disk: the local thread cache (`thread_messages`, which on an established install is where years of correspondents actually live), the dashboard's recent-inbox index, and sent drafts. It is backfilled once per account at dashboard start, and kept fresh afterwards by `envelope thread` scans and unified-inbox refreshes. An install that predates this feature gets suggestions immediately from what is already cached — the From and To lines of every thread message, and the To/Cc/Bcc of every sent draft.
- `thread_messages` now retains Cc and Bcc alongside From and To, so a Cc recipient is history worth suggesting. Rows cached before this release have no Cc/Bcc to recover; nothing backfills them, and they fill in as read-only scans revisit those folders.
- Someone you have just written to is suggestible on the very next message you compose. A successful send folds that message's To, Cc, and Bcc recipients into the address history as part of the same durable transition that records the send, so the CLI, MCP, the dashboard, and the scheduled sweep all get it without waiting for a thread scan, an inbox refresh, or a restart. History follows SMTP acceptance: a draft still being written, a send that was refused, and a transition that lost its ownership lease all record nothing.
- Reconciliation is bounded by a durable per-account watermark on `thread_messages.id`, advanced in the same transaction as the contact writes, so every pass after the first reads only the messages that arrived since. The two paths that change already-folded rows — an in-place header rewrite during a rescan, and the folder wipe a UIDVALIDITY change forces — mark the account for a rebuild instead, so neither leaves stale counts nor double-counts; a rescan that changes no address-bearing field costs nothing. Measured against a synthetic 145,000-row, 23-account database (`cargo test --release -p envelope-email-store --test address_history_scale -- --ignored`): first backfill around 5.5s across 41 chunks with no chunk over 250ms; a caught-up reconcile of all 23 accounts around 140ms, re-reading zero thread rows; one new message around 10ms. Typing reads `contacts` alone — under 3ms for the worst query against the largest account's 1,242 contacts.

### Changed

- Recipient list parsing across the dashboard now respects quoted display names, angle brackets, and quoted-pair escapes, so `"Doe, Jane" <jane@example.com>` is one recipient instead of two malformed ones and a display name carrying a literal quote no longer swallows the recipient after it. The store-side parser additionally drops RFC 5322 group labels.
- Recipient validation on both sides now matches the SMTP edge: the composer and the address book use the same address shape `lettre::Mailboxes` enforces on every To, Cc, and Bcc value, so an address with consecutive dots, a malformed domain label, a quoted local part, an invisible Unicode space, or syntax left over after the angle address (`Ada <ada@example.com> trailing`) is rejected where it is typed rather than at send time. The narrower `lettre::Address` is deliberately not the reference — it accepts quoted local parts that no recipient header can actually carry. Size limits are measured in UTF-8 bytes, as the send edge measures them, so an accented local part is not counted short and waved through.
- Recipient domains must be ASCII on both the composer and the suggestion side; punycode is the spelling to use for an internationalized domain. This is deliberately narrower than the send edge, which parses a Unicode domain and then puts it on the wire unrewritten, where delivery depends on the receiving server advertising SMTPUTF8. A Unicode local part has no equivalent ASCII spelling and stays accepted everywhere.
- `envelope contacts add` now reconciles case-insensitively with the address history. Curating `Alice@Example.com` after a header taught the address book `alice@example.com` updates that contact rather than creating a second row, so the curated name, tags, and notes are what the dropdown offers, and the interaction count already earned for the address is carried over. So are the dates: `contacts add` has no timestamps to offer and passes none, which must not erase the first and last contact the history recorded — suggestions break ties on recency, so curating a contact by hand would otherwise have sunk it below every address still carrying a date. Looking a contact up, tagging one, and deleting one all match the address the same way.
- `contacts` gains a `history_count` column (schema migration 13) holding the interaction count derived from local caches. It is kept strictly apart from the `message_count` that `envelope contacts add|import` owns: a reconcile writes only the derived column — never a contact's manual count, tags, or notes — and fills a blank name. Suggestions rank on whichever count is higher, which is what lets a rebuild lower or reset the derived signal without a stale copy of it surviving anywhere.
- `contacts` also gains a `history_derived` column (same migration) recording who owns each row. A row the address history invented is removed again when its last cached source disappears, so an address that only ever existed in a header a later scan corrected leaves the dropdown instead of sitting in it at zero signal. Anything `envelope contacts` created or edited — added, imported, tagged, annotated, including a row that started out derived and was later curated — is manually managed and survives every rebuild, down to a bare address with no name and no counts. Contacts that predate the migration are manually managed, so an upgrade never deletes a row it cannot prove it invented.
- `contacts` gains a `history_sent_count` column (schema migration 14) holding what the send edge recorded, separately from the count a reconcile derives. The separation is what keeps one message worth one interaction: when the Sent-folder copy of a message you already sent is cached and reconciled later, it lands on a count the send edge never touched, and the settled figure comes out exactly where it would have without the immediate write. Both figures are recomputed from a bounded window rather than incremented, so re-running either changes nothing. The column is additive and needs no re-derivation on upgrade.

### Fixed

- Removing an account now removes the address book derived from its mail. `contacts` and the per-account reconciliation boundary are deleted in the same transaction as the account row; previously they survived it, leaving every correspondent's name and address in the database under an account the user believed they had removed. Other accounts keep their own rows, including addresses they share with the removed one.
- CLI/MCP `ui` deep links and dashboard Cockpit/rules message links no longer open the dashboard's 404 page. `message_url` now emits the canonical reader route `/mail/unified/{account}/{uid}?folder={folder}`, and the cockpit/rules links emit the global `/cockpit` and `/rules` routes. The v2 SPA never had `/accounts/{id}/messages/{uid}`, `/accounts/{id}/cockpit`, or `/accounts/{id}/rules` client routes, so those links resolved to the SPA shell and the SvelteKit router then rendered its own 404 inside an HTTP 200. Historical links keep working: the dashboard now answers all three legacy shapes with a 308 to the canonical route, preserving the `folder` query (INBOX when absent).
- Dashboard message rows now carry their own mailbox in the link. Unified-inbox rows use each row's real `folder`, snoozed rows use `snoozed_folder`, and search hits are tagged with the folder the search ran against. IMAP UIDs are mailbox-scoped, so a folder-less link opened whatever message held that UID in INBOX.

### Compatibility

- The `ui` object's keys and types are unchanged (`dashboard_url`, `dashboard_path`, `cockpit_url`, `message_url`, `rules_url`, `review_url`), so the agent contract schema id is unchanged. Only the path *values* moved to the canonical routes. Draft `review_url` still points at `/accounts/{account}/drafts/{draft}`, which is a real SPA route. Consumers that parse an account id out of a `message_url` path should read it from the response body instead.

## [1.0.10] — 2026-08-13

### Fixed

- Reply, forward, and new drafts with mixed CRLF/LF content no longer fail IMAP APPEND with "Message contains bare newlines". All composed RFC822 messages (draft create/reply/forward/modify and the client-appended Sent archive copy) are normalized to strict CRLF before APPEND. Root cause was mail-builder 0.3.2's quoted-printable body encoder emitting a bare LF for a `\n` that follows a CRLF pair — triggered whenever a quoted parent body kept CRLF endings while Envelope's glue joined with `\n`. Backup restore and migration APPENDs are deliberately untouched: they transfer fetched originals byte-for-byte. ([#87](https://github.com/tymrtn/U1F4E7/issues/87))

## [1.0.9] — 2026-08-08

### Added

- The dashboard Unified Inbox now exposes a sticky, accessible selection toolbar for Archive, provider-aware Trash/Junk, Flag, Snooze, read/unread, custom Move, and exact-sender junk-rule creation. Multi-account operations retain failed selections and report partial results honestly; the 390px layout uses full-size touch targets without overflow.
- Dashboard Snooze now has a real validated API path with presets and custom UTC times, backed by Envelope's existing `Snoozed` folder and return sweep.

### Changed

- Opening a message in the interactive dashboard intentionally marks it `\\Seen` after a successful `BODY.PEEK` load and immediately updates the mounted list row. CLI, MCP, quickstart, and evidence reads remain non-mutating.
- Archive, Junk, and Trash actions use canonical semantic destinations resolved against each account's detected provider folders; Gmail, Outlook, and generic IMAP names are never assumed. Exact operator-selected custom folders remain literal.

### Fixed

- Read-state overrides are keyed by account, folder, and UID, preventing mailbox-scoped UID collisions between Inbox, Sent, Junk, and other folders.
- Dashboard Delete now moves ordinary messages to provider Trash instead of permanently expunging them. Hard delete remains explicit and confirmed only from Trash.
- Canonical move targets used by dashboard-created junk rules are resolved by both dashboard and CLI rule executors; unresolved targets fail without creating or moving into a misleading literal folder.

## [1.0.8] — 2026-08-08

### Changed

- Normal Governor-allowed sends now queue for a **60-second** cooldown by default (down from 120s) before the outbox sweep transmits. Explicit confirmed immediate send (`send_now`/`--send-now` + `confirm_send_now`) still transmits now, honestly-declared review still parks the draft, and deny/invalid still stays unsent. Override precedence (`cooldown_seconds`, `ENVELOPE_SEND_COOLDOWN_SECONDS`) is unchanged, and the active `envelope.agent_contract.v2` schema was regenerated to advertise the 60-second default.
- `short_body` is now derived from the **final body actually being sent** through one canonical policy at both the direct (CLI/MCP) and scheduled boundaries, covering every body shape: text (word count), HTML-only (visible-text word count, not markup tokens), dual-format (the text alternative is canonical), and empty/bodyless (zero words → short). A truthful `short_body` declaration on an HTML-only or bodyless message is now corroborated instead of left `host_verification_unavailable`.
- Attribute provenance reconciled honestly for bot-originated sends: `agent_drafted` is now **declarable author-context** (the bot declares its own authorship; Envelope never infers it for a human CLI user). The weight-free public catalog projection reflects the updated per-key provenance; Governor weights, scores, and thresholds remain external and unchanged.

### Fixed

- Sent-folder proof is stored only in a dedicated, folder-qualified `metadata.sent_copy` object (`folder`, `uid` — explicit JSON `null` when unresolved — `lookup_status`, and `copy_source`) and is never written to the Drafts-folder `imap_uid` column. `mark_draft_sent` clears the stale Drafts UID along with `send_after` on the terminal `sent` state (provider Drafts cleanup still uses the pre-transition snapshot), so a Sent UID can never be conflated with a Drafts UID.
- Immediate `draft send` and MCP `send_draft` now persist the resolved Sent-folder proof on the draft row, matching the scheduled sweep — direct and scheduled durable behavior no longer diverge. Ordering is preserved: SMTP accepted → terminal `sent` state → best-effort Sent lookup/append → durable proof annotation; a proof failure never retransmits.
- The client-appended Sent archive copy now preserves the `Reply-To` header that SMTP transmitted, and keeps `Bcc` on the sender-private archive (normal sends still strip `Bcc` from the wire) so the sender retains the true recipient record. It already preserved Message-ID, To/Cc, subject, text/HTML, attachments, and threading headers.
- Sent-copy resolution now matches the transmitted Message-ID **exactly and uniquely** — treating IMAP `SEARCH HEADER` hits as candidates and comparing their actual Message-ID headers after normalization — instead of taking an arbitrary substring hit; duplicate exact copies yield a stable `ambiguous` status with no UID. `copy_source` is now coherent with the observed outcome: a client append that fails and finds no exact copy is `unresolved` (never `not_attempted`), and a provider-side copy observed after a failed append is labeled `provider`.
- Scheduled allowed sends resolve Sent-folder proof through the same source-aware resolver as the immediate CLI/MCP paths: after SMTP success the background sweep looks up the provider Sent copy by Message-ID and, when the provider does not auto-file, client-appends exactly one archive copy and records the truthful proof.
- A scheduled send that the Governor routes to **review** is now parked `pending_review` with `send_after` cleared, so no surface can present a parked-for-review draft as queued. The dashboard draft page renders explicit *Pending review* (never "Queued for sending", a locked-until-it-sends composer, or a stale countdown); the composer no longer treats any `pending_review` row as queued.
- `envelope draft show` now reports the true persisted draft status — distinguishing `sent`, `pending_review`, `queued` (a scheduled `draft` row), and ordinary `drafted` — instead of always emitting `drafted`.

## [1.0.5] — 2026-08-04

### Added

- Envelope rules now derive a `provider_spam` score from `X-Migadu-Spam-Score` or `X-Spam-Score` during read-only header fetches, allowing sender-independent composite junk rules without downloading message bodies. Explicitly persisted scores retain precedence over provider headers.

### Fixed

- Rule test, preview, and run now canonicalize Message-ID keys consistently across CLI and dashboard paths, so persisted tag/score overrides are honored for both full-message and summary evaluation.
- Dashboard rule execution now evaluates the same header-only summaries used by preview instead of re-fetching each full RFC822 message.

## [1.0.4] — 2026-08-01

### Fixed

- The dashboard header now reports the running backend version from `/api/health` instead of displaying a hard-coded `v1.0.0`; if health is unavailable, the version badge is omitted rather than showing a false release.

## [1.0.3] — 2026-08-01

### Added

- Generated draft review links now open a first-class dashboard composer for editing recipients, subject, and body, with revision-conflict protection, explicit human send confirmation, cooldown queueing, persisted queued/read-only states, and safe route-change handling.

### Fixed

- The Svelte dashboard now recognizes `/accounts/{account}/drafts/{draft}` instead of rendering its own 404 for valid draft URLs.
- Draft review preserves both body alternatives for recipient- or subject-only edits, validates To/Cc/Bcc before send, and prevents in-flight edits or route changes from acting on the wrong draft.

## [1.0.2] — 2026-07-31

### Fixed

- Existing drafts edited in the dashboard now replace the stored body representation set atomically. Editing the plain-text form clears a stale HTML alternate (and vice versa), preventing `multipart/alternative` delivery from showing recipients the pre-edit draft; recipient- or subject-only edits still preserve both body forms.

## [1.0.1] — 2026-07-30

### Fixed

- **Draft review URLs honor the configured dashboard host consistently.** CLI
  draft output now uses the canonical dashboard base URL resolver for top-level
  `dashboard_url` and `review_url` fields, matching nested `ui` metadata instead
  of falling back to `http://localhost:3141` when only persistent
  `dashboard.base_url` is configured.

### Fixed

- Draft JSON no longer ships attachment bytes to the client. `GET /accounts/{id}/drafts`, `GET /accounts/{id}/drafts/{draft_id}`, `by-imap-uid`, and the approve/edit/block responses all strip `data_base64` from each attachment entry, leaving the filename, media type, and size a client needs to describe what is attached. Every draft fetch previously carried each attachment in full — a draft holding a 10 MB PDF moved roughly 13 MB of base64 per request — and `data_base64` is a field the store is explicit about never logging or echoing, which the CLI has honoured since its first attachment listing.

## [1.0.0] — 2026-07-11

First public release. Envelope is a bring-your-own-mailbox email client with
agent-native primitives: it runs a fleet of AI agents on one shared inbox with
per-agent identity, per-action attribution, and a fail-closed send gate, plus a
full webmail dashboard for the humans in the loop.

### Added

- **Multi-agent identity on a shared inbox.** Named agent identities
  (`envelope agent create|list|show|revoke`, `envtok_` tokens shown once and
  stored as a SHA-256 hash + display prefix). Per-agent policy
  (`envelope agent policy set`) clamps allowed accounts, folders, actions, a
  send-mode ceiling, and recipient allowlists — a clamp never widens what a
  policy permits. Every mutation and send-policy/Governor event carries
  `agent_id` attribution; `envelope actions tail --agent <id>` shows the
  per-agent trail. MCP contexts resolve `ENVELOPE_AGENT_TOKEN` at startup and
  authorize every tool call pre-dispatch (unknown/revoked token fails loud).
- **License gate.** Free tier runs up to 2 active agent identities; a 3rd
  requires `envelope license activate` (stable `agent_limit_license_required`
  denial). `license activate|status|deactivate` persist locally, prefix-only
  display.
- **Full webmail dashboard (v2).** SvelteKit (Svelte 5) SPA embedded in the
  binary — accounts rail with health badges, unified/smart mailboxes, message
  list with range-selection and contextual bulk toolbar, sandboxed reader that
  never marks messages read, composer (reply/forward, send-later, undo-send),
  and the Agent Cockpit: per-account draft-approval queue, per-agent
  attribution feed, scheduled sends with persisted Governor verdict badges, and
  rules-first authoring with live blast-radius preview.
- **Bulk operations** (`envelope bulk`, MCP `bulk` tool): move/copy/flag/delete/
  tag over UID or search targets (500 cap), partial-failure isolation, dry-run
  default for destructive ops, UID-range coalescing.
- **Durable event push / webhooks.** HMAC-SHA256 signed deliveries with
  exponential backoff and dead-lettering, per-route secrets minted once,
  delivery-result capture. `envelope watch --deliver`,
  `envelope events routes …`, catalog: `send_queued`, `draft_approved`,
  `send_completed`, `governor_blocked`, `agent_action`.
- **Server-sent events** at `/api/events/stream` (metadata-only, heartbeat,
  reconnecting client with polling fallback).
- **Linux support.** Target-conditional keyring backends (macOS `apple-native`,
  Linux secret-service + file/passphrase default), systemd `--user` units with
  `LoadCredential`, `dist/install.sh` (OS/arch detect + sha256 verify), and a
  4-target tag-triggered release pipeline.
- **Passphrase-first credential store.** Interactive passphrase on first
  account add (Argon2), `ENVELOPE_MASTER_PASSPHRASE_FILE` for systemd,
  `envelope accounts rekey`; machine-key storage is now an explicit
  `--insecure-machine-key` opt-in.
- **Onboarding.** Provider app-password guidance on quickstart auth failure,
  and docs: install-linux, credential-backends, agent-fleet-shared-inbox,
  quickstart, webhooks.

### Security

- **MCP trust boundary.** Hostile message fields (body/subject/from/snippet)
  returned to agents are wrapped in an `_envelope_trust: "untrusted-content"`
  envelope so prompt-injection payloads are labelled, not silently trusted.
- **Dashboard CSRF.** Double-submit `__Host-` cookie + `X-Envelope-CSRF` header
  with Origin/Sec-Fetch-Site checks on all state-changing endpoints; valid
  bearer requests are exempt. Stable `dashboard_csrf_required` code.
- **SSRF guard on webhook URLs.** Every sink (CLI rule create/edit, CLI event
  routes, dashboard rule create and update) rejects loopback/link-local/
  private/reserved/documentation targets — including cloud metadata
  `169.254.169.254`, and IPv4-mapped/compatible IPv6 forms of them — before the
  URL is persisted.
- **No automatic retry after an inconclusive SMTP failure.** A scheduled send
  whose SMTP attempt errors parks in `delivery_uncertain` rather than releasing
  for retry: a dropped final acknowledgement cannot prove non-delivery, so an
  operator must reconcile before the draft is discarded — never a silent
  duplicate send.
- **Secrets are never accepted as CLI arguments.** Account passwords and license
  keys use a hidden terminal prompt or explicit `--password-stdin` / `--key-stdin`,
  keeping them out of process listings and shell history.
- **Bearer token required for broad dashboard binds.** A Tailscale identity
  allowlist is honored only on a loopback listener behind `tailscale serve`;
  any non-loopback bind requires a dashboard bearer token.

### Fixed

- **Drafts always land in the real IMAP Drafts folder.** Draft create/reply/
  forward previously appended to IMAP only best-effort and silently fell back to
  a local-only record (invisible to Mail.app and other clients) on any failure.
  The append is now mandatory and fail-loud, and Envelope resolves the Drafts
  folder itself via the RFC 6154 SPECIAL-USE `\Drafts` attribute (plus provider
  and localized-name detection), so a folder named `Brouillons`, `INBOX/draft`,
  or `[Gmail]/Drafts` all resolve without the agent naming it. Send-only accounts
  with no IMAP host now error instead of creating an invisible draft.

### Notes

- Send-mode names (`draft-only`, `confirm-send`, `allowlisted-send`,
  `autonomous-send`) are stable. MCP/agent contexts default to `draft-only`.
- All outbound mail remains gated through Governor attribution; there is no
  send bypass. Mailbox reads use `EXAMINE` + `BODY.PEEK[]` only.
- Agent contract `envelope.agent_contract.v1` grew additively across this
  release; no `--json` output shapes were removed or retyped.

## [0.12.5] — 2026-07-04

### Fixed

- **Malformed `From:` header on client-appended Sent archive copies (issue #81).** The Sent-copy MIME builder received an already-formatted mailbox string (e.g. `"Display Name" <user@example.test>`) and passed it to `MessageBuilder::from` as if it were a raw address, double-wrapping it into a nested `From: <Display Name <user@example.test>>`. The `from` value is now parsed into proper `(display_name, email)` mailbox parts before serialization, matching the RFC5322-safe `From` handling used by SMTP delivery. The same fix applies to the IMAP Drafts APPEND path. Account-name fallback, comma/quote-safe display names, explicit `--from` overrides, and attachment preservation all remain intact.

## [0.12.4] — 2026-07-04

### Fixed

- **Sent-copy source semantics follow-up (issue #79).** CLI immediate send now uses the same pre-append Sent lookup resolver as draft/MCP send paths, so provider-created Sent copies are checked before Envelope considers a client-side IMAP APPEND archive copy.
- MCP send/reply no longer emit an undocumented top-level `copy_source`; the canonical source label is `sent_mail.copy_source`. MCP `reply` and `send_draft` contract schemas now explicitly advertise `provider_sent_copy`, `client_appended_copy`, and Sent-copy source semantics.
- Added wiring regressions so CLI immediate send cannot quietly fall back to the old append-before-lookup helper.

## [0.12.3] — 2026-07-04

### Fixed

- **Sent-copy source semantics (issue #77).** `sent_mail.copy_source` now carries a stable label — `provider`, `client_appended`, `unresolved`, or `not_attempted` — distinguishing a provider-created Sent copy (e.g. Gmail auto-files) from a client-side IMAP APPEND archive copy written by Envelope. A `client_appended` copy is mailbox hygiene only; it is **not** independent delivery or legal proof. Agents and operators must not treat a client-appended Sent entry as provider confirmation of delivery.
- Added `provider_sent_copy` and `client_appended_copy` top-level fields to immediate-send JSON output for explicit source semantics. Existing backward-compatible fields (`sent_folder`, `sent_uid`, `sent_message_url`, `sent_mail`, `sent_mail_appended`, `sent_mail_append_skipped_reason`) are preserved unchanged.
- Updated agent contract docs and JSON schema descriptions to reflect honest semantics: `sent_mail_appended` is described as a client-side archive, not a proof of delivery.

## [0.12.2] — 2026-07-04

### Fixed

- Draft and Sent-copy `From` headers now use the same account-name fallback and RFC5322-safe quoting as SMTP sends, so draft/reply/MCP proof copies no longer regress to bare addresses when `display_name` is unset.
- Sent proof append for `draft send` now runs even when the local draft has no IMAP Drafts UID, so local-only drafts on generic IMAP/SMTP providers can still produce durable Sent-folder proof after SMTP acceptance.
- Sent append failures now surface stable skipped reasons instead of silently reporting only SMTP success, and draft JSON includes a `sync_status_reason` when a draft is local-only.
- Folder detection candidates now include lowercase slash layouts such as `INBOX/draft` and `INBOX/sent` used by inbox.eu/martin.fm-style accounts.

## [0.12.1] — 2026-07-04

### Fixed

- Outbound SMTP From headers now fall back to the account name when `display_name` is unset or blank, so accounts such as `Tyler Martin <tyler@martin.fm>` no longer send as a bare email address. Envelope now builds the default From mailbox through lettre's mailbox builder so quoted display names are serialized safely.

## [0.12.0] — 2026-07-02

### Added

- **Dashboard authentication for tailnet/remote exposure.** The dashboard REST API now enforces a credential on every `/api` route whenever an auth method is configured, closing the hole where a `tailscale serve` front-end (or any non-loopback bind) exposed read/delete/send-mail and account management to every reachable device with no authentication.
  - **Bearer token** — `Authorization: Bearer <token>` or `X-Envelope-Token: <token>`, compared in constant time. The agent path for Hermes/OpenClaw/scripted clients. Configure with `envelope config set dashboard.auth_token <token>` (stored `0600`, never echoed) or `ENVELOPE_DASHBOARD_TOKEN`.
  - **Tailscale identity allowlist** — a request whose `Tailscale-User-Login` (injected by `tailscale serve`) is allowlisted is authorized without a token, so a human just opens the `.ts.net` URL. Configure with `envelope config set dashboard.tailscale_allow "you@tailnet.ts.net,agent@tailnet.ts.net"` or `ENVELOPE_DASHBOARD_TAILSCALE_ALLOW`.
- **`envelope serve --bind <addr>`** — bind an explicit address. **Fail-closed:** binding a non-loopback address with no auth configured is refused before the socket opens.
- **New config keys** `dashboard.auth_token` (secret; presence-only in `get`) and `dashboard.tailscale_allow`, resolved env-first then persisted config.

### Changed

- `GET /api/health` returns a minimal `status`/`service`/`version` payload to unauthenticated callers; absolute filesystem paths (`binary_path`, `database_path`, `app_data_dir`) are disclosed only to authorized callers. Open loopback mode is unchanged, so local `envelope doctor` drift detection still sees full paths.
- Loopback (`127.0.0.1`) with no auth configured stays open — local dev, the desktop app, and stdio MCP are unaffected.
- Corrected the dashboard's stale "localhost-only by default" doc claim; the CORS allowlist is documented as a browser-only defense, not the access control.

## [0.11.7] — 2026-07-02

### Fixed

- Dashboard draft review deep links now open the exact linked local draft in Agent Cockpit, including the reviewable detail card, instead of only selecting the account and leaving operators with nothing to review.

## [0.11.6] — 2026-07-01

### Added

- **Governor blind-attribution send gate** — real SMTP transmission now derives sanitized Envelope-specific attribute keys and calls `governor score --catalog envelope`; Envelope treats Governor's opaque `allow` / `review` / `deny` route as authoritative instead of synthesizing shell-command metadata or shadow-scoring policy.
- **Outbox-first actual sends** — allowed sends queue into the scheduled-send/outbox path by default with a safety cooldown; immediate SMTP transmission requires an explicit confirmed bypass.
- **Attachment parity for agent sends and drafts** — send, reply, forward, draft edit/send, scheduled send, and MCP paths snapshot attachment bytes while exposing only safe summaries in JSON and logs.
- Scheduled sends can snapshot and deliver attachments safely, with attachment summaries exposed in `scheduled list` and no base64 payload leakage in outputs.
- `envelope accounts copy-password` provides local secure clipboard credential handoff with non-secret audit metadata.
- `envelope doctor` now emits structured diagnostics, dry-run repair plans, and safe DB/credential backup repair actions.
- Dashboard `/api/health` exposes version/binary/backend/path diagnostics for stale-service drift checks.
- `envelope evidence attachment export` exports attachment bytes with source provenance, hashes, safe paths, optional text extraction, and contract/docs coverage.

### Changed

- Dashboard draft approval now queues through the shared outbox/Governor path instead of using a direct SMTP helper, preserving cooldown, attachments, sent-proof bookkeeping, and contextual reply headers.
- Scheduled-send sweeps re-derive final send attributes from the persisted draft immediately before SMTP and park durable `review` / `deny` verdicts for review rather than retry-storming every sweep.
- The agent contract documents queued-send fields, immediate-send confirmation fields, attachment inputs, and Governor/outbox semantics.

### Fixed

- Contextual replies keep `In-Reply-To` and `References` through queued delivery, avoiding orphaned `Re:` messages.
- Successful sends continue to return stable proof handles, including Message-ID and best-effort Sent mailbox lookup status.
- Dashboard email reader collapses quoted replies without enabling scripts and re-measures iframe height on native toggle.
- Native setup instructions gained password-copy handoff and a read-only dashboard backend endpoint.

## [0.11.0] — 2026-06-17

### Added

- **Account signature CLI** — `envelope accounts signature show|set|clear` manages text/HTML account signatures without direct database edits.
- **Native client setup helper** — `envelope accounts setup-instructions` prints non-secret IMAP/SMTP host, port, security, and username values for Mail.app-style setup.
- **Role-based search** — `envelope search --role/--roles sent,archive ...` resolves provider/custom folder layouts such as `INBOX/sent` and searches every matching folder.
- **Canonical agent skill** — `docs/agents/envelope-skill.md` documents Envelope as the runtime for Hermes, Claude Code, Codex, and similar harnesses.

### Changed

- Bare search terms are normalized to IMAP `TEXT` searches so agent queries like `Hillan` no longer silently miss present messages.
- `read` output now preserves full `to_addrs` and `cc_addrs` recipient lists while keeping scalar compatibility fields.
- IMAP-draft sends from the dashboard delete the original IMAP draft only after SMTP send succeeds.
- Backup export help now documents the `--batch-size` memory tradeoff.

### Fixed

- `read --json` strict JSON behavior is regression-tested against control characters.
- Backup verification now flags symlinked/unsafe extra archive entries instead of traversing them.
- Restore-state sidecar reads/writes reject symlinked paths and unsafe parents.
- Backup JSON mode emits machine-readable fatal error events before returning failure.
- Dashboard deep-link fallback coverage now includes message and cockpit routes.

## [0.10.0] — 2026-05-30

The dashboard's interaction model is now fully baked: the half-wired
controls from the Phase 1 shell either do what they imply or are hidden
when they don't apply, and a Gmail-class keyboard/selection layer lands on
top. Every interaction in this release was verified in-browser against a
real cached-message dataset before shipping.

### Added

- **Gmail-style keyboard shortcuts** — `j`/`k` move the focused row,
  `Enter`/`o` open it, `x` selects, `s` stars, `e` archives, `#` deletes,
  `r`/`a` reply / reply-all, `c` composes, `/` focuses search, `Esc` closes
  the topmost surface, and `?` toggles a discoverable cheat-sheet modal
  (also reachable from the header). Shortcuts are suppressed while typing in
  an input or while a modal is open. A single shortcut table drives both the
  handler and the cheat sheet.
- **Shift-click range selection** — selecting a row and shift-clicking
  another toggles the whole contiguous range, matching Gmail.
- **Focused message row** — the active row for keyboard navigation has a
  visible affordance and scrolls into view.

### Changed

- **Contextual affordances are now a first-class rule.** Controls that have
  no useful action in the current state are hidden rather than rendered
  broken. The account "Reconnect" control only appears when an account is
  actually unhealthy; the bulk toolbar collapses to a hint when nothing is
  selected. Architectural status notes no longer leak into button labels or
  accessible names.
- **Star toggling persists to IMAP `\Flagged`** through the existing flags
  endpoint, instead of being local-only.
- **Account reconnect runs the real IMAP/SMTP `verify` flow** and refreshes
  the health badge, instead of returning a "not wired yet" string.
- **Bulk archive and delete are wired** to the existing per-message
  endpoints with bounded concurrency, per-item failure reporting, and
  terminal status summaries that auto-clear instead of lingering.
- **HTML email reader auto-sizes to its content** (collapse-then-measure,
  clamped to 760px) so short messages no longer sit inside a tall empty
  box. The email iframe grants `allow-same-origin` solely for height
  measurement; scripts/forms remain disabled and message HTML is still
  sanitized, so no email-controlled code can run.
- **Cache-missing accounts read as an unwarmed cache**, not a hard failure:
  no false "all accounts failed" wall, no spurious Reconnect buttons, and a
  clean single refresh CTA.

### Fixed

- Reply / Reply-All no longer open an empty composer when no message is
  selected; they surface a friendly prompt instead.

### Preserved

- Agent Cockpit aggregate endpoints remain read-only — no live auth probes,
  IMAP mutations, or draft sends from aggregate load. Per-account draft
  actions stay the mutating surface.

## [0.9.0] — 2026-05-20

### Added

- **Dashboard Phase 1 Core Shell** — rebuilt the localhost dashboard as an
  operator mail client with a left mailbox sidebar, middle message list, and
  permanent right reader pane. The sidebar exposes Unified Inbox,
  Today/Needs Attention, Snoozed, Sent, Drafts, All Mail, and account mailbox
  groups with nested folders when folder metadata is available.
- **Agent Cockpit attention strip** — the cockpit now starts as a compact
  read-only attention strip and expands into the existing watches, event
  buckets, drafts/actions, auth/action errors, rule runs, and due snoozes
  panels.
- **Message primitive rows and bulk triage shell** — message list rows now carry
  a shared `message` primitive shape with state, actions, audit event,
  render hint, rollback token, and concise equivalent CLI metadata. The message
  list adds dense mail-client controls for selection, local star affordance,
  sender, subject, snippet, labels, attachment hints, date, and an honest
  bulk toolbar that stays non-mutating until backend execution is wired.
- **Account health primitive and sidebar badges** — account rows now expose a
  local-only `account_health` primitive with compact health badges, sync
  freshness, provider capability hints, sanitized failure reasons, and an honest
  reconnect affordance that stays non-mutating until recovery is wired.
- **Path diagnostics** — `envelope paths` (alias: `envelope doctor`)
  prints the resolved database path, file credential path, config/app-data
  directory, current `HOME`, and warnings when those locations are under
  agent-harness directories such as `/private/tmp`, `/tmp`, `/var/folders`,
  or `.codex`.

### Changed

- Workspace and active desktop shell versions bumped to 0.9.0.
- Dashboard first paint now lands on the cached Unified Inbox/local operator
  surfaces and avoids automatic account selection that would probe live IMAP
  folders or messages.

[0.11.0]: https://github.com/tymrtn/U1F4E7/releases/tag/v0.11.0
[0.10.0]: https://github.com/tymrtn/U1F4E7/releases/tag/v0.10.0
[0.9.0]: https://github.com/tymrtn/U1F4E7/releases/tag/v0.9.0

## [0.5.0] — 2026-04-19

### Added

- **IMAP IDLE event stream** — `envelope watch` opens a persistent
  IMAP IDLE connection and emits JSON events on new mail in real time.
  Supports stdout, webhook (`--webhook <url>`), and SQLite event storage.
  Reconnects automatically on connection drop with exponential backoff.
  25-minute IDLE cycle stays under RFC 2177's 29-minute server timeout.

- **Verification code extraction** — `envelope code --wait 60` blocks
  until a verification/OTP code arrives, extracts it from the message
  body (regex patterns for explicit labels, OTP-style codes, HTML-prominent
  numbers, and standalone digits), and prints it to stdout. Pipe-friendly:
  `CODE=$(envelope code --wait 60)`. Filters by `--from` domain and
  `--subject` pattern.

- **MCP server** — `envelope mcp` starts a Model Context Protocol server
  over stdio, exposing 12 tools: inbox, read, search, send, reply,
  move_message, flag, folders, tag, contacts, accounts, and rule_run.
  `envelope mcp --config` prints a ready-to-paste JSON config snippet
  for Claude Code, Cursor, or Zed. Envelope is the only MCP email server
  that works against any IMAP provider (Gmail, Outlook, Migadu, Fastmail,
  self-hosted Dovecot).

- **Scheduled send** — `envelope send --to ... --at "monday 9am"` creates
  a draft with a scheduled send time. The `envelope serve` background
  ticker sends due messages automatically. `envelope scheduled list` and
  `envelope scheduled cancel <id>` manage the queue. Reuses the snooze
  datetime parser (ISO 8601, relative offsets, natural language).

- **Contacts** — `envelope contacts add/list/show/tag/untag/import`.
  Local contact store in SQLite with freeform tags (JSON array). Tags
  integrate with the rules engine: `--match-contact-tag vendor` creates
  a rule that matches any message from a contact tagged "vendor".
  `envelope contacts import --from-inbox` bootstraps the contacts table
  from inbox senders.

- **Webhook rule actions** — `envelope rule create --action webhook=<url>`
  POSTs message context as JSON to the webhook URL when a rule matches.
  10-second timeout, fire-and-forget. Enables integrating Envelope's
  rules engine with external systems (n8n, Make, custom scripts).

- **SQLite schema migrations** — Replaced hand-rolled `CREATE TABLE IF
  NOT EXISTS` with `rusqlite_migration` (v1.3). Tracks schema version
  via `PRAGMA user_version`. Existing databases upgrade seamlessly.
  All v0.5.0 tables (events, contacts) are added as versioned migrations.

- **Agent Workflows help section** — `envelope --help` now shows a
  dedicated "Agent Workflows" section with copy-paste one-liners for
  watch, code extraction, scheduled send, contacts import, and MCP setup.

### Changed

- Workspace version bumped to 0.5.0.
- `regex` added as a dependency (for verification code extraction).
- `rusqlite_migration` added as a dependency (schema versioning).
- `ContactHasTag` added to the rules engine `MatchExpr` enum. Rules
  can now match on the sender's contact tags, not just message-level tags.

[0.5.0]: https://github.com/tymrtn/U1F4E7/releases/tag/v0.5.0

## [0.4.1] — 2026-04-19

### Fixed

- Exposed `snooze check-replies` subcommand — was implemented but not
  wired into the clap dispatch.
- Dashboard compose handler updated for `from_override` parameter added
  to `SmtpSender::send`.

### Changed

- Repo renamed from `tymrtn/envelope-email` to `tymrtn/U1F4E7`. Old URL
  redirects. Brew tap moved to `tymrtn/homebrew-u1f4e7`
  (`brew install tymrtn/u1f4e7/u1f4e7`). Python prototype archived at
  `tymrtn/U1F4E7-python`.

[0.4.1]: https://github.com/tymrtn/U1F4E7/releases/tag/v0.4.1

## [0.4.0] — 2026-04-14

### Added

- **Rules engine** — agents create mail rules that Envelope enforces
  deterministically. `envelope rule create --name "..." --match-from "..."
  --action move=Junk`. Rules evaluate match expressions (FROM/TO/SUBJECT
  globs, tag checks, score thresholds) against messages and execute actions
  (move, flag, unflag, snooze, delete, unsubscribe, add tag). All-match
  default with optional `stop` flag per rule. Batch execution in groups of
  50 with progress reporting.
- **Message tagging + scoring** — `envelope tag set <uid> --score urgent=0.9
  --tag newsletter`. Scores are float dimensions (0.0–1.0), tags are
  freeform strings. Keyed on Message-ID (stable across folder moves).
  Rules can match on tags and scores for agent-trained junk filtering.
- **List-Unsubscribe** — `envelope unsubscribe <uid> --confirm`. Parses
  RFC 2369 `List-Unsubscribe` and RFC 8058 one-click POST headers.
  Dry-run by default (shows what it would do), `--confirm` to execute.
  Never auto-follows GET URLs (tracking risk). Supports HTTPS POST
  and mailto fallback.
- **Sieve export** — `envelope rule export`. Generates RFC 5228 Sieve
  scripts from rules that use pure IMAP-level matches (FROM/TO/SUBJECT).
  Tag/score-based rules are local-only and skipped with a warning.
  ManageSieve upload deferred to v0.5.
- **Background unsnooze ticker** — `envelope serve` now spawns a tokio
  task that sweeps the snooze queue every 60 seconds and returns due
  messages to their original folders automatically.
- **IMAP connection retry** — dashboard folder handler retries with a
  fresh connection on stale IMAP pooled connections.
- **Loading indicators** — dashboard shows "Loading folders…" / "Loading
  messages…" / "Loading message…" while IMAP fetches are in flight.
- **Account list collapse** — sidebar shows 3 accounts by default with
  a "+ N more" toggle for large account lists.
- **Account label in inbox title** — shows "INBOX — tyler@example.com"
  so you always know which account's inbox you're looking at.
- **Rich `--help`** — getting-started examples, agent usage patterns,
  and provider list in the top-level help output.

### Changed

- Workspace version bumped to 0.4.0.
- `reqwest` added as a dependency (for HTTPS unsubscribe).

### Fixed

- **RFC 2047 subject decoding** — IMAP ENVELOPE subjects now decode
  `=?utf-8?q?...?=` and `=?utf-8?b?...?=` encoded words instead of
  showing raw encoded strings. Handles Q-encoding, B-encoding, UTF-8,
  and multiple consecutive encoded words with whitespace folding.
- Sequential folder/message loading — folders load before messages
  (was racing, causing "no account selected" in sidebar).
- Folder error recovery with retry button on IMAP failures.

[0.4.0]: https://github.com/tymrtn/U1F4E7/releases/tag/v0.4.0

## [0.3.0] — 2026-04-09

### Added

- **Full dashboard rewrite.** `envelope serve` now launches a complete
  three-pane email client (folder sidebar, inbox list, reader + composer
  drawers) at [http://localhost:3141](http://localhost:3141). Ported the
  Instrument Sans / DM Mono light-theme aesthetic from the Python U1F4E7
  prototype. HTML/CSS/JS bundled into the binary via `rust-embed` — a
  single `cargo install` ships the whole UI.
- **REST API backing the dashboard** (`/api/*`) with routes for accounts,
  folders (with unread counts), messages (list/read/flag/move/delete/search),
  attachments, compose, reply, drafts, snoozed, threads, and stats.
- **Reply / reply-all** with correct header threading. New
  `envelope_email_transport::reply` module builds `In-Reply-To` and
  `References` headers from the parent message, handles 11 international
  `Re:`/`Fwd:` prefixes, and excludes the account owner from reply-all
  Cc. Works from both the CLI (via `envelope send`) and the dashboard
  reader.
- **SMTP attachments.** `envelope send --to x --attach file.pdf --attach other.png`
  wraps the message body in `multipart/mixed` with one part per file.
  Content-Type detection via `mime_guess`. The dashboard composer
  base64-encodes files client-side and posts them in the JSON envelope.
- **Snooze feature** (`envelope snooze set|list|cancel`, `envelope unsnooze`).
  Flexible datetime parsing: ISO 8601, relative (`2h`, `3d`, `1w`),
  natural (`tomorrow`, `monday`, `next week`). Escalation tiers,
  waiting-reply tracking, per-account IMAP `Snoozed` folder.
- **Threading** (`envelope thread show|list|build`). RFC 2822
  header walking (`Message-ID`, `In-Reply-To`, `References`) with a
  normalized-subject fallback for messages missing threading headers.
  11-language subject prefix stripping (English, German, French, Spanish,
  Dutch, Italian, Portuguese, Swedish/Norwegian).
- **`envelope folders`** now shows per-folder `exists / unseen`
  counts via the IMAP `STATUS` command, both in human output and `--json`.
- **`envelope read <uid>`** uses `BODY.PEEK[]` so reading a message does
  NOT auto-set the `\Seen` flag on the server. Explicit `envelope flag add
  <uid> seen` is required to mark as read.
- **`envelope mark_seen`** helper (library API) for callers that want
  explicit read-marking after `fetch_message`.
- **Orphan detection CI guard** (`ci/check-orphans.sh`). Fails when any
  `.rs` file in `crates/*/src/` is not declared via `mod` — prevents
  the class of silent regression that lost the snooze and threading
  features in commit `27f3919` (see `docs/ORPHANS-AUDIT.md`).
- **`docs/ORPHANS-AUDIT.md`** — post-mortem of the 27f3919 regression
  and the measures taken to prevent recurrence.

### Changed

- **Binary renamed from `envelope-email` to `envelope`.** The Cargo
  package name remains `envelope-email` to preserve the crates.io slot,
  but the binary target is now `envelope`. `cargo install envelope-email`
  installs a binary called `envelope`. Users who installed 0.1.x via
  `cargo install` or Homebrew need to either re-run the install or
  update their PATH.
- `envelope folders` text output gained the `exists / unseen` columns.
- `envelope serve` default port remains 3141. The old 4-endpoint stub
  is gone; the full dashboard is the only option now.
- `crates/dashboard` no longer embeds HTML as a Rust string — it lives
  in `static/` and is bundled at compile time via `rust-embed`.

### Fixed

- **Restored the orphaned snooze feature** (`crates/store/src/snoozed.rs`
  and `crates/cli/src/commands/snooze.rs`, ~1,276 lines). These files
  shipped in commit `27f3919` but were never declared via `mod` and
  never compiled — the feature silently didn't exist. Recreated the
  missing `SnoozedMessage` model, `snoozed` table DDL, and wired the
  impl + CLI into the module tree. Added a `snoozed.reply_received`
  column and `escalation_tier` column that the orphan code expected.
- **Restored the orphaned threading feature** (`crates/email/src/threading.rs`,
  `crates/store/src/threads.rs`, `crates/cli/src/commands/thread.rs`,
  ~2,211 lines). Same silent regression from commit `27f3919`. Recreated
  `Thread` + `ThreadMessage` models, `threads` + `thread_messages` +
  `thread_sync_state` table DDL, and wired everything into `mod`. Fixed
  drift where `thread_messages.id` was defined as `TEXT` but the code
  expected `INTEGER PRIMARY KEY AUTOINCREMENT`. Fixed `Option<String>`
  handling in display code paths.
- `threading::normalize_subject` now delegates to a new
  `strip_reply_prefixes` helper that preserves case; `normalize_subject`
  lowercases the result for thread grouping only. Previously the
  lowercasing leaked into any caller using it for display.
- `envelope read` correctly uses `BODY.PEEK[]` (guarded by a unit test).

### Removed

- **All governor / policy / scoring integration** (`crates/cli/src/governor.rs`,
  `crates/cli/src/commands/governor.rs`, the `Governor` clap subcommand,
  and — most importantly — the `--no-governor` CLI flag that was a
  self-documenting backdoor on a public tool). If you want governance
  around destructive or outbound operations, wrap Envelope from outside:
  `governor envelope send ... -- --attr user_requested`. Envelope's
  job is email.

### Notes

- This is the first release with a proper CHANGELOG. Prior releases
  (0.1.0, 0.2.x) shipped without one; their commit history is the only
  record.
- Total work landed in 0.3.0: ~5,000 lines added, 8 commits, 113 tests
  passing (40 store + 63 email + 10 dashboard), zero clippy warnings in
  new code, `ci/check-orphans.sh` clean.

[0.3.0]: https://github.com/tymrtn/U1F4E7/releases/tag/v0.3.0
