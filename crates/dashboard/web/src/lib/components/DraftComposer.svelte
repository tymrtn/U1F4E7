<script lang="ts">
  // Draft review composer — the surface behind every generated draft link
  // (`review_url` / `dashboard_url` from the CLI and MCP, and the cockpit's
  // Edit action). It is a Gmail-like editable composer, not an approval card:
  // the operator lands on the actual message and can change it before it goes.
  //
  // Safety contract, mirrored from crates/dashboard/src/handlers/drafts.rs:
  //   • Every edit and send carries the revision the operator was SHOWN as
  //     `expected_revision`, so a concurrent change returns 409 instead of
  //     silently overwriting content nobody reviewed.
  //   • Send is never implicit: an explicit confirmation precedes the POST,
  //     which carries `confirm: true`. That endpoint queues into the outbox
  //     cooldown — the shared scheduled sweep does the real SMTP behind the
  //     Governor gate — so this surface reports "queued", never "sent".
  //   • Sending is blocked while edits are unsaved, so the queued copy is
  //     always the copy the operator actually read.
  //   • A queued draft can be HELD: `send_after` is cleared, the row stays a
  //     `draft`, and the editor unlocks. Holding is never a discard — that is
  //     a separate, deliberate action.
  //   • CSRF is handled by the shared request() helper in $lib/api. Nothing
  //     here bypasses it, and there is no direct-send path.

  import { page } from '$app/state';
  import {
    addrKey,
    optionalAddrsValid,
    parseAddrs,
    serializeAddrs,
    validateAddrs
  } from '$lib/addresses';
  import Badge from './Badge.svelte';
  import BodyFrame from './BodyFrame.svelte';
  import Button from './Button.svelte';
  import DraftAttachments from './DraftAttachments.svelte';
  import DraftThread from './DraftThread.svelte';
  import Modal from './Modal.svelte';
  import MonoTag from './MonoTag.svelte';
  import RecipientField from './RecipientField.svelte';
  import Spinner from './Spinner.svelte';
  import { getLiveStore } from '$lib/live.svelte';
  import {
    api,
    isEditableDraftStatus,
    isSendableDraftStatus,
    type Draft,
    type DraftEditBody,
    type DraftQueuedResponse,
    type DraftStatus,
    type EnvelopeApiError
  } from '$lib/api';

  type BodyFormat = 'text' | 'html';

  /** Mirrors the backend default in crates/transport (ENVELOPE_SEND_COOLDOWN_SECONDS). */
  const DEFAULT_COOLDOWN_SECONDS = 60;

  interface Snapshot {
    to: string;
    cc: string;
    bcc: string;
    subject: string;
    body: string;
    format: BodyFormat;
  }

  /** Human framing for each draft status, plus why a status is read-only. */
  const STATUS_META: Record<
    DraftStatus,
    { label: string; variant: 'ok' | 'warn' | 'pending' | 'danger'; note: string }
  > = {
    draft: { label: 'Draft', variant: 'pending', note: '' },
    pending_review: {
      label: 'Pending review',
      variant: 'warn',
      note: ''
    },
    blocked: {
      label: 'Blocked',
      variant: 'warn',
      note: 'Changes were requested on this draft. You can edit and save it here, but it has to be approved again before it can be queued.'
    },
    sending: {
      label: 'Sending',
      variant: 'pending',
      note: 'Envelope is transmitting this message right now, so it can no longer be edited.'
    },
    syncing: {
      label: 'Syncing',
      variant: 'pending',
      note: 'Envelope is syncing this draft with your mail server, so it can no longer be edited.'
    },
    delivery_uncertain: {
      label: 'Delivery uncertain',
      variant: 'danger',
      note: 'Your mail server accepted this message but Envelope could not record the result. Check your Sent folder before doing anything else with it.'
    },
    sent: {
      label: 'Sent',
      variant: 'ok',
      note: 'This message has already been sent, so it is read-only.'
    },
    discarded: {
      label: 'Discarded',
      variant: 'warn',
      note: 'This draft was discarded, so it is read-only.'
    }
  };

  // ── Route-driven identity ─────────────────────────────────────────────

  const accountId = $derived(page.params.account ?? '');
  const draftId = $derived(page.params.draft ?? '');
  const routeKey = $derived(`${accountId}:${draftId}`);

  // This component is a single mounted instance that re-targets whenever the
  // route params change, so every async completion has to prove it still
  // belongs to the current route before it touches state. `loadGeneration` is
  // bumped by each load; a response whose generation is stale is dropped rather
  // than repainting the draft the operator has moved on to.
  let loadedKey = '';
  let loadGeneration = 0;

  /** Route key the currently-held draft was actually fetched for. */
  let loadedForKey = $state('');

  // ── Load state ────────────────────────────────────────────────────────

  let draft = $state<Draft | null>(null);
  let accountLabel = $state('');
  let loading = $state(true);
  let notFound = $state(false);
  let loadError = $state<{ code: string; message: string } | null>(null);

  // ── Editor fields ─────────────────────────────────────────────────────

  let toRaw = $state('');
  let ccRaw = $state('');
  let bccRaw = $state('');
  let subjectRaw = $state('');
  let bodyRaw = $state('');
  let bodyFormat = $state<BodyFormat>('text');
  // The editor keeps one body per format. Both slots are seeded when the draft
  // loads, so switching format shows that format's own body instead of
  // relabelling whatever is already in the box — relabelling is how a save
  // overwrote a draft's real HTML part with its plain-text twin. Parking the
  // live body on the way out means switching back returns the operator's
  // unsaved edit rather than the server copy.
  let bodyByFormat = $state<Record<BodyFormat, string>>({ text: '', html: '' });
  // Approving an HTML body means seeing what the recipient will see, so the
  // preview renders it through the same sandboxed frame the reader uses.
  let previewing = $state(false);
  // Remote images always load in the draft composer. Blocking them is an
  // INBOUND privacy control — it stops a stranger's tracking pixel confirming
  // you opened their mail. This is your own outgoing message: you are the
  // sender, there is no read receipt to leak, and hiding the artwork means
  // reviewing a layout your recipient will see and you will not.
  const remoteImages = true;
  let showBcc = $state(false);
  let baseline = $state<Snapshot>({ to: '', cc: '', bcc: '', subject: '', body: '', format: 'text' });

  // ── Action state ──────────────────────────────────────────────────────

  let saving = $state(false);
  let saved = $state(false);
  let queueing = $state(false);
  let holding = $state(false);
  let confirmOpen = $state(false);
  let queued = $state<DraftQueuedResponse | null>(null);
  let conflict = $state(false);
  let actionError = $state<{ code: string; message: string; reload?: boolean } | null>(null);

  // ── Derived ───────────────────────────────────────────────────────────

  /**
   * Guard for every mutating action: the draft in hand must be the draft the
   * URL currently names. Comparing the fetch-time route key (rather than
   * `draft.account_id`) keeps this correct for the account-by-email form of the
   * URL, which the API resolves server-side.
   */
  const identityMatches = $derived(!!draft && loadedForKey === routeKey && draft.id === draftId);

  /**
   * The account authenticates the SMTP connection, but a draft may carry a
   * validated send-as identity in metadata. Review must show the header that
   * will actually reach the recipient, not merely the transport account.
   */
  function metadataFromIdentity(value: Draft | null): string | null {
    const from = value?.metadata?.from;
    if (typeof from !== 'string') return null;
    const trimmed = from.trim();
    return trimmed && !/[\r\n]/.test(trimmed) ? trimmed : null;
  }
  const effectiveFrom = $derived(metadataFromIdentity(draft) ?? (accountLabel || accountId));

  const statusMeta = $derived(draft ? STATUS_META[draft.status] : null);
  const statusEditable = $derived(!!draft && isEditableDraftStatus(draft.status));

  /**
   * A persisted `send_after` on a `draft`-status row means the draft is ALREADY
   * sitting in the outbox, whether or not this page is the one that put it there.
   *
   * `list_drafts_due_for_send` selects ONLY `status='draft'` rows whose
   * `send_after` has come due, and the edit statement does not clear
   * `send_after` — it only strips the approval attestation. So editing a queued
   * draft does not pull it back: the edited content still ships, just without
   * `tyler_approved`. The queued state therefore has to be recovered from the
   * draft itself on every load, not just from the response of a send this page
   * performed.
   *
   * Scoped strictly to `draft` status: a `pending_review` draft is parked for a
   * human decision and is NEVER due — the backend clears `send_after` when it
   * parks for review, and this guard is the matching defense-in-depth so a
   * review-parked draft renders "Pending review", never "Queued" with a stale
   * countdown, even if a `send_after` somehow lingered.
   */
  const persistedQueue = $derived(
    !!draft && draft.send_after != null && draft.status === 'draft'
  );
  // Server truth ONLY. This used to OR in the local `queued` POST response,
  // which is set once and never cleared — so after the sweep parked or sent the
  // draft, the browser went on rendering a countdown for a row that had neither
  // `send_after` nor `draft` status. That stale flag also suppressed the stop
  // alert below, which is how a Governor-blocked message displayed as "Queued —
  // due now" with a Hold button, for hours, having never been transmitted.
  // On acknowledgement `confirmSend` copies the receipt's durable `send_after`
  // onto `draft`, so this remains server-shaped state rather than a permanent
  // local queued flag.
  const isQueued = $derived(persistedQueue);

  /**
   * A parked or blocked draft must never look silent. Prefer the stored
   * `metadata.send_block`; fall back to attribution park_reason; then a
   * generic "stopped, nothing transmitted" so older rows still explain themselves.
   */
  type SendBlock = {
    code: string;
    title: string;
    explanation: string;
    action?: string;
    /** Where the agent-path block can also be cleared, when the CLI carries it. */
    remedy?: string;
  };
  function asSendBlock(value: unknown): SendBlock | null {
    if (!value || typeof value !== 'object') return null;
    const o = value as Record<string, unknown>;
    const title = typeof o.title === 'string' ? o.title.trim() : '';
    const explanation = typeof o.explanation === 'string' ? o.explanation.trim() : '';
    if (!title && !explanation) return null;
    return {
      code: typeof o.code === 'string' && o.code ? o.code : 'send_stopped',
      title: title || 'This send was stopped',
      explanation: explanation || 'Nothing was transmitted.',
      action: typeof o.action === 'string' ? o.action : undefined
    };
  }
  const sendBlock = $derived.by((): SendBlock | null => {
    if (!draft) return null;
    // The status guard is what keeps stale reasons off screen: a queued or
    // sent draft renders no block even though `send_block` metadata may
    // survive a human-authorized queue, and every fresh park rewrites the
    // reason. A block that renders here therefore describes THIS draft's
    // current parked/blocked state.
    if (draft.status !== 'pending_review' && draft.status !== 'blocked') return null;
    const stored = asSendBlock(draft.metadata?.send_block);
    if (stored) return stored;
    const attribution = draft.metadata?.attribution;
    const park =
      attribution && typeof attribution === 'object'
        ? (attribution as Record<string, unknown>).park_reason
        : null;
    if (park === 'attribution_exhausted') {
      const record = attribution as Record<string, unknown>;
      const declared = Array.isArray(record.declared_attrs) ? record.declared_attrs : [];
      const attempts = typeof record.attempts === 'number' ? record.attempts : 0;
      // Which label would pass is deliberately NOT shown — the park record
      // carries no such field, and coaching one would turn a blind
      // declaration into lock-picking. Human-only Send stays offered: it
      // queues this exact revision on the operator's own authorization, so
      // it does not re-run the exhausted agent attempt.
      const stuck = record.origin === 'bot' && declared.length === 0;
      return {
        code: 'attribution_exhausted',
        title: 'This send was stopped',
        explanation: stuck
          ? `Envelope paused this message because the send is bot-attributed and carries no fact labels. ` +
            `${attempts} attempts were spent and no fact labels were declared for this revision. ` +
            `Approving it again repeats the same governed attempt; Human-only Send transmits it on your own authorization.`
          : 'Envelope paused this message because it could not complete a required fact label. Nothing was transmitted.',
        action: stuck ? undefined : 'send',
        remedy: stuck
          ? `Declaring is the sending agent's job, and only the CLI can carry it: envelope draft send ${draft.id} --attr <label>`
          : undefined
      };
    }
    if (draft.status === 'blocked') {
      return {
        code: 'blocked',
        title: 'This send was stopped',
        explanation: STATUS_META.blocked.note,
        action: 'edit'
      };
    }
    return {
      code: 'send_stopped',
      title: 'This send was stopped',
      explanation:
        'Envelope paused this message before it left. Nothing was transmitted. Envelope did not record a more specific reason on this draft.',
      action: 'send'
    };
  });
  const queuedAt = $derived(queued?.send_after ?? draft?.send_after ?? null);

  // ── Live countdown ────────────────────────────────────────────────────
  //
  // A wall-clock send time alone cannot tell an operator whether they have
  // forty seconds or six hours, which is exactly the decision the queued
  // banner exists to support: a `--at` schedule days out and the 60s safety
  // cooldown otherwise render identically. So the remaining time is computed
  // here and ticked locally rather than waiting on a reload.

  let nowMs = $state(Date.now());

  const dueAtMs = $derived(queuedAt ? parseWhen(queuedAt) : null);
  /** True once the send time has passed — the sweep may fire at any moment. */
  const pastDue = $derived(dueAtMs != null && dueAtMs - nowMs <= 0);
  /** Remaining time, or `null` when `send_after` could not be parsed. */
  const countdown = $derived(
    dueAtMs == null ? null : pastDue ? 'due now' : remaining(dueAtMs - nowMs)
  );

  // Ticks only while something is actually counting down: an unqueued draft,
  // an unparsable timestamp, or an already-due one has nothing to update.
  // `nowMs` is written but never read here, so this does not re-trigger itself.
  $effect(() => {
    if (!isQueued || dueAtMs == null || pastDue) return;
    const timer = setInterval(() => (nowMs = Date.now()), 1000);
    return () => clearInterval(timer);
  });

  // ── Live server truth while a send is in flight ──────────────────────
  //
  // The sweep decides this draft's fate in another process: it may transmit it,
  // park it for review, or defer it. Without a listener the browser keeps the
  // last render forever — the failure the operator actually hit, where a
  // Governor-blocked message sat on screen as "due now" long after the server
  // had stopped it. Subscribe to the draft lifecycle events for THIS draft and
  // re-read the row whenever the server says it moved.
  $effect(() => {
    if (!draft) return;
    // The live channel is a browser capability, not a requirement. Without it
    // the past-due poll below still reconciles state — the composer degrades to
    // slower truth, never to stale truth.
    if (typeof EventSource === 'undefined') return;
    const watchedAccount = accountId;
    const watchedDraft = draftId;
    const live = getLiveStore();
    const off = live.on(
      ['draft_queued', 'draft_status_changed', 'send_status'],
      (event) => {
        if (event.draft_id !== watchedDraft) return;
        if (event.account_id && event.account_id !== watchedAccount) return;
        if (accountId !== watchedAccount || draftId !== watchedDraft) return;
        // The queue receipt already paints the committed transition. Do not let
        // its matching event replace that UI with a blocking full-draft load;
        // later lifecycle events and the past-due poll still reconcile truth.
        if (!queueing) void load(true);
      }
    );
    return off;
  });

  // A past-due draft the sweep has not resolved yet is the one state that
  // genuinely cannot be observed: no event fires while it waits its turn. Poll
  // slowly so "due now" resolves on its own instead of freezing, and stop as
  // soon as the row is no longer queued.
  $effect(() => {
    if (!isQueued || !pastDue) return;
    const timer = setInterval(() => void load(true), 5000);
    return () => clearInterval(timer);
  });

  const editable = $derived(statusEditable && !isQueued);
  const sendable = $derived(!!draft && isSendableDraftStatus(draft.status) && !isQueued);

  const dirty = $derived(
    !!draft &&
      (toRaw !== baseline.to ||
        ccRaw !== baseline.cc ||
        bccRaw !== baseline.bcc ||
        subjectRaw !== baseline.subject ||
        bodyRaw !== baseline.body ||
        bodyFormat !== baseline.format)
  );

  /**
   * Whether the body pair itself changed. The edit endpoint treats
   * `text_content`/`html_content` as ONE unit: supplying either replaces the
   * pair and clears the omitted alternate. That is right when the body really
   * changed, and destructive when it did not — a subject-only save would
   * otherwise silently drop a dual-format draft's HTML part.
   */
  const bodyChanged = $derived(bodyRaw !== baseline.body || bodyFormat !== baseline.format);

  /** The loaded draft carries both a text and an HTML alternative. */
  const hasBothBodies = $derived(!!draft && draft.text_content != null && draft.html_content != null);

  // Saving must be possible on a half-written draft, so it only needs SOME
  // recipient. Queueing puts mail on the wire, so it holds out for addresses
  // the compose surface would also accept — including Cc and Bcc, which reach
  // real people and otherwise only fail at SMTP time.
  const recipientPresent = $derived(toRaw.trim().length > 0);
  const recipientsValid = $derived(
    validateAddrs(toRaw) && optionalAddrsValid(ccRaw) && optionalAddrsValid(bccRaw)
  );

  // No address may appear in two recipient fields of the same draft.
  const usedAddrs = $derived({
    to: parseAddrs(toRaw).map(addrKey),
    cc: parseAddrs(ccRaw).map(addrKey),
    bcc: parseAddrs(bccRaw).map(addrKey)
  });

  const canSave = $derived(editable && dirty && recipientPresent && !saving && identityMatches);
  const canSend = $derived(
    sendable && !dirty && recipientsValid && !saving && !queueing && identityMatches
  );

  // While a save is in flight the editor is locked: otherwise keystrokes made
  // during the round trip are silently discarded when the server's copy of the
  // draft is adopted as the new baseline on completion.
  const inputsLocked = $derived(!editable || saving);

  // ── Loading ───────────────────────────────────────────────────────────

  $effect(() => {
    const key = routeKey;
    if (key === loadedKey) return;
    loadedKey = key;
    resetForRoute();
    if (!accountId || !draftId) {
      // Defensive: the route pattern always supplies both params, so this only
      // guards against a malformed mount. Fail visibly instead of spinning.
      loading = false;
      notFound = true;
      return;
    }
    void load();
  });

  /**
   * Drop everything tied to the previous draft. Without this an open send
   * confirmation, a queued banner, or a conflict from draft A stays on screen
   * over draft B — and confirming that dialog would act on B.
   *
   * Bumping the generation here (not only in `load`) means a route change
   * invalidates in-flight work even on the paths that do not start a new load.
   */
  function resetForRoute() {
    loadGeneration += 1;
    confirmOpen = false;
    queueing = false;
    holding = false;
    saving = false;
    queued = null;
    conflict = false;
    actionError = null;
    saved = false;
    notFound = false;
    loadError = null;
    draft = null;
    loadedForKey = '';
    accountLabel = '';
    loading = true;
    previewing = false;
  }

  /**
   * Re-read the full draft. Lifecycle/poll callers use a background refresh so
   * a slow body or attachment payload never replaces already-rendered truth
   * with a page-sized loading state.
   */
  async function load(background = false) {
    const generation = ++loadGeneration;
    const forKey = routeKey;
    const requestedAccount = accountId;
    const requestedDraft = draftId;

    if (!background) {
      loading = true;
      notFound = false;
      loadError = null;
      conflict = false;
      actionError = null;
      queued = null;
      saved = false;
    }

    try {
      const res = await api.draft(requestedAccount, requestedDraft);
      if (generation !== loadGeneration) return;
      applyDraft(res.draft);
      loadedForKey = forKey;
      accountLabel = res.account?.username ?? res.draft.account_id;
      // This draft is newer server truth than the queue acknowledgement, so it
      // may safely replace its response-only cooldown metadata.
      queued = null;
    } catch (e) {
      if (generation !== loadGeneration) return;
      // A background refresh must preserve the state already on screen. The
      // next lifecycle event/poll can retry; an operator-visible reload still
      // presents a concrete error below.
      if (background) return;
      const err = e as EnvelopeApiError;
      if (err.status === 404) {
        notFound = true;
      } else {
        loadError = {
          code: err.code ?? 'unknown',
          message: err.message ?? 'Could not load this draft.'
        };
      }
    } finally {
      if (!background && generation === loadGeneration) loading = false;
    }
  }

  /** Adopt a server draft as the new editing baseline. */
  function applyDraft(next: Draft) {
    // A draft that HAS an HTML body opens on it, rendered. That body is the
    // message the recipient sees; the text part is the fallback alternative,
    // and opening on the fallback showed bare tracking URLs where the real
    // email has buttons. Saving still writes back exactly the format shown,
    // which clears the other alternate server-side — that is what stops a
    // stale HTML part from being delivered instead of the edit.
    const format: BodyFormat = next.html_content != null ? 'html' : 'text';
    // Keep the values painted into the controls and the saved baseline as one
    // snapshot. In particular, recipient controls render their parsed,
    // canonical header form rather than the server's original whitespace and
    // separators. Taking the baseline from those exact rendered values is what
    // lets a successful edit immediately unlock the human-only send actions.
    const rendered: Snapshot = {
      to: serializeAddrs(next.to_addr ?? ''),
      cc: serializeAddrs(next.cc_addr ?? ''),
      bcc: serializeAddrs(next.bcc_addr ?? ''),
      subject: next.subject ?? '',
      body: (format === 'html' ? next.html_content : next.text_content) ?? '',
      format
    };

    draft = next;
    // Normalized to the recipient field's canonical `a@x, b@y` form BEFORE the
    // baseline snapshot: the field re-serializes whatever it is given, so an
    // un-normalized server value would come back changed and read as an
    // unsaved edit the operator never made — which blocks Send.
    toRaw = rendered.to;
    ccRaw = rendered.cc;
    bccRaw = rendered.bcc;
    subjectRaw = rendered.subject;
    // A draft carrying only one alternative seeds the empty slot from the one
    // it has: switching format there means "send this body as the other
    // format", not "start from an empty box".
    bodyByFormat = {
      text: (next.text_content ?? next.html_content) ?? '',
      html: (next.html_content ?? next.text_content) ?? ''
    };
    bodyRaw = rendered.body;
    bodyFormat = rendered.format;
    // HTML opens rendered. Reading markup is the exception, so it lives behind
    // the toggle rather than being what a review lands on.
    previewing = format === 'html';
    showBcc = bccRaw.length > 0;
    conflict = false;
    baseline = rendered;
  }

  /**
   * Adopt the server's draft after an attachment change — revision and
   * attachment list only.
   *
   * `applyDraft` cannot be used here: it re-seeds the editor fields and the
   * baseline from the server row, which on a draft with unsaved edits would
   * silently discard whatever the operator had typed. Attaching a file
   * mid-sentence must not cost them the sentence. Taking the fresh revision
   * matters just as much — the next save has to echo the revision the
   * attachment write produced, or it 409s against a change this page made.
   */
  function adoptAttachmentChange(next: Draft) {
    draft = next;
    conflict = false;
    // The attachment write cleared any approval attestation server-side, so a
    // "saved" badge from before it would now overstate what is approved.
    saved = false;
  }

  /**
   * Switch which alternative is being edited, swapping the body along with the
   * label. The edit endpoint writes whichever body it is handed and clears the
   * other, so a format that shows the wrong body sends the wrong body.
   */
  function setFormat(next: BodyFormat) {
    if (next === bodyFormat) return;
    bodyByFormat[bodyFormat] = bodyRaw;
    bodyRaw = bodyByFormat[next];
    bodyFormat = next;
    // Entering HTML renders it; plain text has nothing to render.
    previewing = next === 'html';
  }

  function snapshot(): Snapshot {
    return {
      to: toRaw,
      cc: ccRaw,
      bcc: bccRaw,
      subject: subjectRaw,
      body: bodyRaw,
      format: bodyFormat
    };
  }

  // ── Save ──────────────────────────────────────────────────────────────

  function editPayload(revision: number): DraftEditBody {
    const payload: DraftEditBody = {
      expected_revision: revision,
      to_addr: toRaw.trim(),
      cc_addr: ccRaw.trim(),
      bcc_addr: bccRaw.trim(),
      subject: subjectRaw
    };
    // Omit BOTH body fields unless the body actually changed. Sending either
    // one replaces the pair and clears the alternate, so including a body on a
    // subject- or recipient-only save would destroy a dual-format draft's other
    // half. When the body did change, send exactly the edited format — clearing
    // the stale alternate is the intended behaviour there.
    if (bodyChanged) {
      if (bodyFormat === 'html') payload.html_content = bodyRaw;
      else payload.text_content = bodyRaw;
    }
    return payload;
  }

  async function save() {
    if (!draft || !canSave) return;
    // Pin the target and the generation: if the route moves while this is in
    // flight, the response must not be adopted over the next draft.
    const generation = loadGeneration;
    const targetAccount = accountId;
    const targetDraft = draftId;

    saving = true;
    conflict = false;
    actionError = null;
    saved = false;

    try {
      const res = await api.editDraft(targetAccount, targetDraft, editPayload(draft.revision));
      if (generation !== loadGeneration) return;
      applyDraft(res.draft);
      saved = true;
    } catch (e) {
      if (generation !== loadGeneration) return;
      handleActionError(e, 'Could not save this draft.');
    } finally {
      if (generation === loadGeneration) saving = false;
    }
  }

  // ── Send (queue) ──────────────────────────────────────────────────────

  function requestSend() {
    if (!canSend) return;
    confirmOpen = true;
  }

  /**
   * Dismissal is refused while the POST is in flight. Escape, the backdrop, the
   * close button and Keep editing would otherwise all read as "cancelled" while
   * the request can still succeed, leaving the operator believing they stopped
   * a send that is already queued. There is no cancel to offer here: the
   * request is not abortable server-side once it lands.
   */
  function closeConfirm() {
    if (queueing) return;
    confirmOpen = false;
  }

  async function confirmSend(sendNow = false) {
    if (!draft || queueing || !canSend) return;
    // Pin the target: a route change mid-flight must not land this queue
    // result on, or attribute it to, whatever draft is on screen afterwards.
    const generation = loadGeneration;
    const targetAccount = accountId;
    const targetDraft = draftId;

    queueing = true;
    sendNowPending = sendNow;
    conflict = false;
    actionError = null;

    try {
      const res = await api.sendDraft(targetAccount, targetDraft, {
        confirm: true,
        expected_revision: draft.revision,
        send_now: sendNow
      });
      if (generation !== loadGeneration) return;
      queued = res;
      // The POST response is an atomic acknowledgement that the server stored
      // this queue transition. It is not a DraftStatus: the persisted row stays
      // `draft` so the scheduled sweep can find it, and no SMTP has happened.
      // Preserve the reviewed content/revision while copying only the durable
      // queue fact required to paint the outbox state immediately.
      draft = { ...draft, status: 'draft', send_after: res.send_after };
      confirmOpen = false;
      // Do not await a full draft GET here: body and attachment reads can be
      // slow, but the queue is already durably acknowledged. SSE lifecycle
      // events and the past-due poll reconcile later state in the background.
      // Start a nonblocking reconciliation now as well; `load(true)` preserves
      // the receipt-derived UI until it obtains newer server truth.
      queueing = false;
      sendNowPending = false;
      void load(true);
    } catch (e) {
      if (generation !== loadGeneration) return;
      confirmOpen = false;
      handleActionError(e, 'Could not queue this draft.');
    } finally {
      if (generation === loadGeneration) {
        queueing = false;
        sendNowPending = false;
      }
    }
  }

  // ── Hold (unqueue, keep the draft) ────────────────────────────────────

  const canHold = $derived(isQueued && !holding && identityMatches);

  /**
   * `Send now` is available wherever `Send` is, and additionally while a draft
   * is still waiting out its cooldown — that wait is the single most common
   * reason an operator wants immediacy, so refusing them there would defeat the
   * control. It stays disabled once the sweep is already transmitting
   * (`pastDue`), where the request would race a send in flight.
   */
  const canSendNow = $derived(
    !!draft && !queueing && identityMatches && !dirty && recipientsValid && (
      (sendable && isSendableDraftStatus(draft.status)) || (isQueued && !pastDue)
    )
  );

  /** Which button is spinning, so only the pressed one shows a spinner. */
  let sendNowPending = $state(false);

  /** The cooldown named on the default Send button, so the wait is never a surprise. */
  const cooldownLabel = $derived.by(() => {
    const secs = queued?.cooldown_seconds ?? DEFAULT_COOLDOWN_SECONDS;
    if (secs <= 0) return 'now';
    if (secs < 60) return `${secs}s`;
    const mins = Math.round(secs / 60);
    return mins === 1 ? '1 min' : `${mins} min`;
  });

  /**
   * Pull a queued draft back out of the outbox and keep it. The endpoint clears
   * `send_after` and leaves the row in `draft` status, so adopting the returned
   * draft is what unlocks the editor — no reload, no second round trip.
   *
   * No confirmation and no `expected_revision`: hold only ever REMOVES a
   * pending send, so there is nothing here to guard content against, and an
   * operator watching a countdown must be able to stop it in one click.
   * Discarding stays the deliberate, separate destructive path.
   */
  async function hold() {
    if (!draft || !canHold) return;
    // A queue acknowledgement starts a nonblocking reconciliation. Invalidate
    // any response it already has in flight before requesting Hold, so only the
    // returned unlocked draft can become authoritative after a successful hold.
    const generation = ++loadGeneration;
    const targetAccount = accountId;
    const targetDraft = draftId;

    holding = true;
    conflict = false;
    actionError = null;

    try {
      const res = await api.holdDraft(targetAccount, targetDraft);
      if (generation !== loadGeneration) return;
      // Clear the transient send result before adopting the draft: `isQueued`
      // ORs it with the persisted `send_after`, so a stale `queued` would keep
      // the banner and the editor lock over a draft that is no longer queued.
      queued = null;
      applyDraft(res.draft);
      saved = false;
    } catch (e) {
      if (generation !== loadGeneration) return;
      const err = e as EnvelopeApiError;
      if (err.status === 409) {
        // Not the revision guard: the sweep has already claimed this draft for
        // transmission, or it is no longer queued at all. Either way the state
        // on screen is stale, and the honest move is to say so and reload —
        // never to report a hold that did not happen.
        actionError = {
          code: err.code ?? 'draft_not_held',
          message:
            'Envelope would not take this draft back. It has either already started sending, or it is no longer queued. Reload to see where it actually stands.',
          reload: true
        };
        return;
      }
      actionError = {
        code: err.code ?? 'unknown',
        message: err.message ?? 'Could not take this draft out of the outbox.'
      };
    } finally {
      if (generation === loadGeneration) holding = false;
    }
  }

  /**
   * 409 is the revision guard, not a generic failure: the draft changed since
   * it was loaded and the server refused rather than clobbering it. Surface it
   * as its own recoverable state with a reload affordance — never auto-retry,
   * which would overwrite the change the operator has not seen.
   */
  function handleActionError(e: unknown, fallback: string) {
    const err = e as EnvelopeApiError;
    if (err.status === 409) {
      conflict = true;
      return;
    }
    actionError = { code: err.code ?? 'unknown', message: err.message ?? fallback };
  }

  // ── Formatting ────────────────────────────────────────────────────────

  /**
   * Epoch ms for a stored timestamp. Rows written before the RFC 3339 switch
   * carry a naive `YYYY-MM-DDTHH:MM:SS` that is UTC by contract, so an absent
   * zone is read as `Z` — the same frame the backend's due comparison uses.
   * `null` for anything unparsable, so callers can fall back rather than
   * render `NaN`.
   */
  function parseWhen(iso: string): number | null {
    const ms = Date.parse(iso.includes('Z') || iso.includes('+') ? iso : `${iso}Z`);
    return Number.isNaN(ms) ? null : ms;
  }

  function formatWhen(iso: string): string {
    const ms = parseWhen(iso);
    if (ms == null) return iso;
    return new Date(ms).toLocaleString([], {
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit'
    });
  }

  /**
   * Time left, at whatever resolution is decision-useful: seconds when the
   * send is imminent, coarser as it recedes. Always leads with the largest
   * non-zero unit so a multi-day schedule can never be misread as a cooldown.
   */
  function remaining(ms: number): string {
    const total = Math.floor(ms / 1000);
    const days = Math.floor(total / 86400);
    const hours = Math.floor((total % 86400) / 3600);
    const mins = Math.floor((total % 3600) / 60);
    const secs = total % 60;
    if (days > 0) return `${days}d ${hours}h`;
    if (hours > 0) return `${hours}h ${String(mins).padStart(2, '0')}m`;
    if (mins > 0) return `${mins}m ${String(secs).padStart(2, '0')}s`;
    return `${secs}s`;
  }
</script>

<section class="draft-review" aria-label="Draft review">
  {#if loading}
    <div class="draft-state" id="draft-loading">
      <Spinner />
      <p>Loading this draft…</p>
    </div>
  {:else if notFound}
    <div class="draft-state" id="draft-not-found">
      <p class="draft-state-title">This draft is no longer here.</p>
      <p>
        It may have been sent, discarded, or it belongs to a different account. Draft links stop
        working once the draft is gone.
      </p>
      <MonoTag>{draftId}</MonoTag>
    </div>
  {:else if loadError}
    <div class="draft-state" id="draft-load-error" role="alert">
      <p class="draft-state-title">Could not open this draft.</p>
      <p>{loadError.message}</p>
      <MonoTag>{loadError.code}</MonoTag>
      <div class="draft-state-action">
        <Button variant="ghost" onclick={() => load()}>Try again</Button>
      </div>
    </div>
  {:else if draft}
    <header class="draft-head">
      <div class="draft-head-main">
        <p class="draft-eyebrow">Draft review</p>
        <h1 class="draft-title">{subjectRaw.trim() || '(no subject)'}</h1>
      </div>
      <div class="draft-head-meta">
        {#if isQueued}
          <!-- The row is still `draft` status — that is how the sweep finds it —
               but "Draft" beside a queued banner reads as a contradiction. -->
          <Badge variant="ok">Queued</Badge>
        {:else if statusMeta}
          <Badge variant={statusMeta.variant}>{statusMeta.label}</Badge>
        {/if}
        <MonoTag>rev {draft.revision}</MonoTag>
      </div>
    </header>

    {#if statusMeta?.note}
      <p class="draft-banner is-note">{statusMeta.note}</p>
    {/if}

    {#if conflict}
      <div class="draft-banner is-conflict" id="draft-conflict" role="alert">
        <p class="draft-banner-title">This draft changed while you had it open.</p>
        <p>
          Nothing was overwritten. Reload the latest version, then re-apply your edit so you are
          working from what is actually stored.
        </p>
        <div class="draft-banner-action">
          <Button variant="ghost" onclick={() => load()}>Reload latest</Button>
        </div>
      </div>
    {/if}

    {#if actionError}
      <div class="draft-banner is-error" id="draft-action-error" role="alert">
        <p>{actionError.message}</p>
        <MonoTag>{actionError.code}</MonoTag>
        {#if actionError.reload}
          <div class="draft-banner-action">
            <Button variant="ghost" onclick={() => load()}>Reload latest</Button>
          </div>
        {/if}
      </div>
    {/if}

    {#if isQueued}
      <!-- One line of state, the actions beside it, the explanation available
           but not shouted. The old banner spent five sentences describing a
           wait, which is exactly the copy an operator skips — and skipping it
           was free, because nothing in it was actionable. -->
      <div class="draft-outbox" id="draft-queued" role="status">
        <div class="draft-outbox-state">
          <span class="draft-outbox-dot" class:is-imminent={pastDue}></span>
          <span class="draft-outbox-headline" id="draft-countdown">
            {#if pastDue}
              Sending now
            {:else if countdown}
              Sends in <span class="draft-countdown-value" aria-live="off">{countdown}</span>
            {:else}
              Waiting in the outbox
            {/if}
          </span>
          {#if queuedAt && !pastDue}
            <span class="draft-outbox-at">{formatWhen(queuedAt)}</span>
          {/if}
        </div>
        <div class="draft-outbox-actions">
          {#if !pastDue}
            <Button variant="primary" disabled={!canSendNow} onclick={() => confirmSend(true)}>
              {#if queueing}<Spinner label="Sending" />{/if}
              Send now
            </Button>
          {/if}
          <Button variant="ghost" disabled={!canHold} onclick={hold}>
            {#if holding}<Spinner label="Holding" />{/if}
            {holding ? 'Holding' : 'Hold as draft'}
          </Button>
        </div>
        <p class="draft-outbox-note">
          {#if pastDue}
            Envelope is transmitting this message. Nothing more to do here.
          {:else}
            Not transmitted yet. Holding returns it to an editable draft — it is never discarded.
          {/if}
          <a class="draft-outbox-link" id="draft-outbox-link" href="/cockpit#scheduled-panel">
            All queued sends
          </a>
        </p>
      </div>
    {/if}

    {#if sendBlock}
      <div class="draft-banner is-stopped" id="draft-send-block" role="alert">
        <p class="draft-banner-title">{sendBlock.title}</p>
        <p>{sendBlock.explanation}</p>
        <MonoTag>{sendBlock.code}</MonoTag>
        {#if sendBlock.remedy}
          <p class="draft-banner-remedy">{sendBlock.remedy}</p>
        {/if}
        <div class="draft-banner-action">
          {#if sendable}
            <Button variant="primary" disabled={!canSend} onclick={requestSend}>
              Human-only Send again
            </Button>
          {/if}
        </div>
      </div>
    {/if}

    <!-- ── Conversation this draft answers ─────────────────────────── -->
    {#if draft?.in_reply_to}
      <DraftThread {accountId} inReplyTo={draft.in_reply_to} />
    {/if}

    <div class="draft-card draft-addresses">
      <div class="draft-field-row is-static">
        <span class="draft-field-label">From</span>
        <span class="draft-field-value" id="draft-from-identity">{effectiveFrom}</span>
      </div>

      <RecipientField
        id="draft-to"
        label="To"
        bind:value={toRaw}
        {accountId}
        disabled={inputsLocked}
        exclude={[...usedAddrs.cc, ...usedAddrs.bcc]}
        placeholder="recipient@example.com"
        invalid={toRaw.trim() !== '' && !validateAddrs(toRaw)}
      />

      <div class="draft-recipient-row">
        <RecipientField
          id="draft-cc"
          label="Cc"
          bind:value={ccRaw}
          {accountId}
          disabled={inputsLocked}
          exclude={[...usedAddrs.to, ...usedAddrs.bcc]}
          placeholder="Optional"
          invalid={!optionalAddrsValid(ccRaw)}
        />
        {#if !showBcc && !inputsLocked}
          <button class="draft-bcc-toggle" type="button" onclick={() => (showBcc = true)}>Bcc</button>
        {/if}
      </div>

      {#if showBcc}
        <RecipientField
          id="draft-bcc"
          label="Bcc"
          bind:value={bccRaw}
          {accountId}
          disabled={inputsLocked}
          exclude={[...usedAddrs.to, ...usedAddrs.cc]}
          placeholder="Optional"
          invalid={!optionalAddrsValid(bccRaw)}
        />
      {/if}

      <div class="draft-field-row">
        <label for="draft-subject">Subject</label>
        <input
          id="draft-subject"
          type="text"
          placeholder="Subject"
          bind:value={subjectRaw}
          disabled={inputsLocked}
        />
      </div>
    </div>

    <div class="draft-card draft-editor">
      <div class="draft-editor-toolbar">
        <div class="draft-format" role="group" aria-label="Message format">
          <button
            type="button"
            class:is-active={bodyFormat === 'text'}
            aria-pressed={bodyFormat === 'text'}
            disabled={inputsLocked}
            onclick={() => setFormat('text')}>Text</button
          >
          <button
            type="button"
            class:is-active={bodyFormat === 'html'}
            aria-pressed={bodyFormat === 'html'}
            disabled={inputsLocked}
            onclick={() => setFormat('html')}>HTML</button
          >
        </div>
        {#if bodyFormat === 'html'}
          <button
            type="button"
            class="draft-preview-toggle"
            aria-pressed={previewing}
            onclick={() => (previewing = !previewing)}
          >
            {previewing ? 'Edit HTML' : 'Preview'}
          </button>
        {/if}

        {#if bodyChanged && hasBothBodies}
          <p class="draft-format-note">
            This draft has both a plain-text and an HTML version. Saving replaces the body with
            what you see here and drops the other format.
          </p>
        {/if}
      </div>
      {#if previewing && bodyFormat === 'html'}
        <div class="draft-preview">
          <BodyFrame
            html={bodyRaw}
            {remoteImages}
          />
        </div>
      {:else}
        <label class="draft-sr-only" for="draft-body">Message</label>
        <textarea
          id="draft-body"
          placeholder="Write your message"
          bind:value={bodyRaw}
          disabled={inputsLocked}
        ></textarea>
      {/if}
    </div>

    <DraftAttachments
      {draft}
      accountId={accountId}
      editable={editable && identityMatches}
      onchange={adoptAttachmentChange}
      onconflict={() => (conflict = true)}
    />

    {#if sendable}
      <p class="draft-banner draft-send-note" id="draft-human-send-note">
        <strong>Human-only Send</strong> is your explicit send of this exact version. It still waits
        out the outbox cooldown, Hold still takes it back, and Envelope still files the Sent copy as
        proof. Governor does not score a message you send this way. That covers this send alone —
        editing the draft, holding it, or an agent queueing it again withdraws it.
      </p>
    {/if}

    <footer class="draft-actions">
      <div class="draft-actions-status">
        {#if dirty && !recipientPresent}
          <span class="is-warn">Add a recipient before saving.</span>
        {:else if dirty}
          <span class="is-dirty">Unsaved changes. Save your changes before sending.</span>
        {:else if saved}
          <span class="is-saved">Changes saved.</span>
        {:else if isQueued}
          <!-- The outbox strip above already carries the countdown and the Hold
               control. Repeating it here is noise at the moment of decision. -->
          <span class="is-saved">Editing is locked while this message is in the outbox.</span>
        {:else if sendable && !recipientsValid}
          <span class="is-warn">Add a valid recipient before sending.</span>
        {:else if !statusEditable && statusMeta}
          <span>{statusMeta.label} — read-only.</span>
        {/if}
      </div>
      <div class="draft-actions-buttons">
        {#if editable}
          <Button variant="ghost" disabled={!canSave} onclick={save}>
            {#if saving}<Spinner label="Saving" />{/if}
            {saving ? 'Saving' : 'Save changes'}
          </Button>
        {/if}
        {#if sendable}
          <!-- Two ways to send, both plainly labelled and neither hidden behind
               a menu: an operator who means "now" should not have to hunt.
               Both open the same confirmation, which is where the choice is
               actually made — a first send is the one action with no undo
               window, so it gets one look at From/To/Subject first. -->
          <Button variant="ghost" disabled={!canSend} onclick={requestSend}>
            Human-only Send now
          </Button>
          <Button variant="primary" disabled={!canSend} onclick={requestSend}>
            Human-only Send in {cooldownLabel}
          </Button>
        {/if}
      </div>
    </footer>
  {/if}
</section>

<Modal open={confirmOpen} title="Human-only Send this draft?" onclose={closeConfirm}>
  <p class="draft-confirm-line">
    <strong>From</strong>
    {effectiveFrom}
  </p>
  <p class="draft-confirm-line">
    <strong>To</strong>
    {toRaw.trim() || '(no recipient)'}
  </p>
  <p class="draft-confirm-line">
    <strong>Subject</strong>
    {subjectRaw.trim() || '(no subject)'}
  </p>
  <p class="draft-confirm-note">
    You are approving this exact version. Editing it afterwards withdraws that approval.
  </p>
  <p class="draft-confirm-note">
    This click is the send. Governor scores what agents send on their own; it does not score this
    one.
  </p>
  {#if queueing}
    <p class="draft-confirm-note is-locked">
      Queueing this message. This cannot be cancelled here — wait for it to finish.
    </p>
  {/if}
  {#snippet footer()}
    <Button variant="ghost" disabled={queueing} onclick={closeConfirm}>Keep editing</Button>
    <!-- Both destinations are offered at the point of decision, so choosing
         immediacy never costs a second trip through this dialog. -->
    <Button variant="ghost" disabled={queueing} onclick={() => confirmSend(true)}>
      {#if queueing && sendNowPending}<Spinner label="Sending" />{/if}
      Send now
    </Button>
    <Button variant="primary" disabled={queueing} onclick={() => confirmSend(false)}>
      {#if queueing && !sendNowPending}<Spinner label="Queueing" />{/if}
      Send in {cooldownLabel}
    </Button>
  {/snippet}
</Modal>

<style>
  .draft-review {
    flex: 1;
    min-width: 0;
    min-height: 0;
    overflow: auto;
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
    padding: 1.125rem;
    background: var(--env-page);
  }

  /* ── States ── */
  .draft-state {
    margin: auto;
    max-width: 42ch;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.5rem;
    text-align: center;
    color: var(--env-muted);
    font-size: 0.8125rem;
    line-height: 1.5;
  }
  .draft-state p {
    margin: 0;
  }
  .draft-state-title {
    color: var(--env-ink);
    font-size: 0.9375rem;
    font-weight: 600;
  }
  .draft-state-action {
    margin-top: 0.35rem;
  }

  /* ── Header ── */
  .draft-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 1rem;
  }
  .draft-head-main {
    min-width: 0;
  }
  .draft-eyebrow {
    margin: 0;
    color: var(--env-muted);
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    letter-spacing: 0.12em;
    text-transform: uppercase;
  }
  .draft-title {
    margin: 0.15rem 0 0;
    font-size: 1.125rem;
    font-weight: 600;
    line-height: 1.3;
    overflow-wrap: anywhere;
  }
  .draft-head-meta {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    flex-shrink: 0;
  }

  /* ── Banners ── */
  .draft-banner {
    margin: 0;
    padding: 0.65rem 0.875rem;
    border: 1px solid var(--env-rule);
    background: var(--env-surface);
    color: var(--env-muted);
    font-size: 0.8125rem;
    line-height: 1.5;
  }
  .draft-banner p {
    margin: 0;
  }
  .draft-banner-title {
    color: var(--env-ink);
    font-weight: 600;
  }
  .draft-banner-action {
    margin-top: 0.5rem;
  }
  /* Sits directly above the send controls, so it reads as the label's
     explanation rather than another status banner. */
  .draft-send-note {
    border-bottom: 0;
    font-size: 0.75rem;
  }
  .draft-send-note strong {
    color: var(--env-ink);
  }
  .is-note {
    border-color: var(--env-pending);
    background: var(--env-pending-soft);
    color: var(--env-pending);
  }
  .is-conflict,
  .is-error {
    border-color: var(--env-warn);
    background: var(--env-warn-soft);
    color: var(--env-warn);
  }
  .is-conflict .draft-banner-title {
    color: var(--env-warn);
  }
  .is-stopped {
    border-color: var(--env-warn);
    background: var(--env-warn-soft);
    color: var(--env-ink);
  }
  .is-stopped .draft-banner-title {
    color: var(--env-warn);
  }

  /* ── Outbox strip ──
     One row: state on the left, the two decisions on the right, a single line
     of consequence underneath. It replaces a five-sentence banner whose only
     actionable content was one button. */
  .draft-outbox {
    display: grid;
    grid-template-columns: 1fr auto;
    align-items: center;
    gap: 0.5rem 1rem;
    padding: 0.75rem 0.9rem;
    border: 1px solid var(--env-accent);
    border-radius: 0.375rem;
    background: var(--env-accent-soft);
  }
  .draft-outbox-state {
    display: flex;
    align-items: baseline;
    flex-wrap: wrap;
    gap: 0.5rem;
    min-width: 0;
  }
  .draft-outbox-dot {
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 50%;
    background: var(--env-accent);
    flex: none;
    align-self: center;
  }
  /* Once the sweep is transmitting, the dot stops being decoration and starts
     meaning "in flight". */
  .draft-outbox-dot.is-imminent {
    animation: draft-outbox-pulse 1.2s ease-in-out infinite;
  }
  @keyframes draft-outbox-pulse {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.25;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .draft-outbox-dot.is-imminent {
      animation: none;
    }
  }
  .draft-outbox-headline {
    color: var(--env-accent);
    font-size: 0.9375rem;
    font-weight: 600;
  }
  .draft-countdown-value {
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
  }
  .draft-outbox-at {
    color: var(--env-muted);
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
  }
  .draft-outbox-actions {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.5rem;
    justify-self: end;
  }
  .draft-outbox-note {
    grid-column: 1 / -1;
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
    margin: 0;
    color: var(--env-muted);
    font-size: 0.75rem;
  }
  .draft-outbox-link {
    color: var(--env-accent);
    font-family: var(--font-mono);
    font-size: 0.6875rem;
  }
  @media (max-width: 34rem) {
    .draft-outbox {
      grid-template-columns: 1fr;
    }
    .draft-outbox-actions {
      justify-self: stretch;
    }
  }

  /* ── Fields ── */
  .draft-card {
    min-width: 0;
    border: 1px solid var(--env-rule);
    background: var(--env-surface);
  }
  .draft-addresses {
    padding: 0.25rem 0.875rem;
  }
  .draft-field-row {
    min-height: 2.625rem;
    display: grid;
    grid-template-columns: 4.75rem minmax(0, 1fr) auto;
    align-items: center;
    gap: 0.75rem;
    border-bottom: 1px solid var(--env-rule);
  }
  .draft-field-row:last-child {
    border-bottom: 0;
  }
  .draft-field-row label,
  .draft-field-label {
    margin: 0;
    color: var(--env-muted);
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    letter-spacing: 0.12em;
    text-transform: uppercase;
  }
  .draft-field-row input,
  .draft-field-value {
    width: 100%;
    min-width: 0;
    border: 0;
    outline: 0;
    background: transparent;
    color: var(--env-ink);
    font-family: var(--font-mono);
    font-size: 0.8125rem;
    overflow-wrap: anywhere;
  }
  .draft-field-row input::placeholder {
    color: #aaa69e;
  }
  .draft-field-row:focus-within {
    box-shadow: inset 2px 0 0 var(--env-accent);
  }
  .draft-field-row input:disabled {
    color: var(--env-muted);
    -webkit-text-fill-color: var(--env-muted);
    opacity: 1;
  }
  .draft-bcc-toggle {
    border: 0;
    background: transparent;
    color: var(--env-accent);
    cursor: pointer;
    font-family: var(--font-mono);
    font-size: 0.6875rem;
  }

  /* Cc shares its row with the Bcc reveal, so the rule that separates address
     rows moves out to the wrapper and off the field itself. */
  .draft-recipient-row {
    display: flex;
    align-items: flex-start;
    gap: 0.75rem;
    border-bottom: 1px solid var(--env-rule);
  }
  .draft-recipient-row :global(.recipient-field) {
    flex: 1;
    min-width: 0;
    border-bottom: 0;
  }
  .draft-recipient-row .draft-bcc-toggle {
    padding-top: 0.85rem;
  }

  /* ── Editor ── */
  .draft-editor {
    /* `flex: 1` let the column shrink this card to a fraction of the message it
       contains: the rendered body then painted straight over the attachments
       row below it. The card takes the height of its content and the page
       scrolls — the textarea keeps a floor so an empty draft is still writable. */
    flex: none;
    min-height: 18rem;
    display: flex;
    flex-direction: column;
  }
  .draft-editor-toolbar {
    min-height: 2.875rem;
    display: flex;
    align-items: center;
    gap: 0.75rem;
    padding: 0.45rem 0.65rem;
    border-bottom: 1px solid var(--env-rule);
    background: var(--env-paper);
  }
  .draft-format {
    display: inline-grid;
    grid-template-columns: repeat(2, minmax(3.5rem, auto));
    border: 1px solid var(--env-rule);
    background: var(--env-surface);
  }
  .draft-format button {
    min-height: 1.875rem;
    border: 0;
    border-right: 1px solid var(--env-rule);
    background: transparent;
    color: var(--env-muted);
    cursor: pointer;
    font-family: var(--font-mono);
    font-size: 0.6875rem;
  }
  .draft-format button:last-child {
    border-right: 0;
  }
  .draft-format button.is-active {
    background: var(--env-ink);
    color: var(--env-surface);
  }
  .draft-format button:disabled {
    cursor: not-allowed;
    opacity: 0.55;
  }
  .draft-preview-toggle,
  .draft-preview-toggle[aria-pressed='true'] {
    background: var(--env-ink);
    color: var(--env-surface);
  }
  .draft-preview {
    /* No inner scroller and no flex clamp. The message IS this page's content;
       trapping a full HTML email in a 20rem well and asking the operator to
       scroll it inside a page that also scrolls is two scrollbars for one
       document. BodyFrame sizes its iframe to the rendered height, so the body
       flows and the page is the only thing that scrolls. */
    background: var(--env-surface);
  }
  .draft-banner-remedy {
    margin: 0.4rem 0 0;
    font-family: var(--font-mono);
    font-size: 0.75rem;
    overflow-wrap: anywhere;
  }
  .draft-format-note {
    margin: 0;
    color: var(--env-pending);
    font-size: 0.75rem;
  }
  #draft-body {
    flex: 1;
    min-height: 16rem;
    width: 100%;
    resize: none;
    border: 0;
    outline: 0;
    padding: 1rem;
    background: var(--env-surface);
    color: var(--env-ink);
    font-family: var(--font-mono);
    font-size: 0.8125rem;
    line-height: 1.65;
  }
  #draft-body:disabled {
    color: var(--env-muted);
    -webkit-text-fill-color: var(--env-muted);
    opacity: 1;
  }

  /* ── Actions ── */
  .draft-actions {
    min-height: 3rem;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    padding: 0.65rem 0.875rem;
    border: 1px solid var(--env-rule);
    background: var(--env-paper);
  }
  .draft-actions-status {
    min-width: 0;
    color: var(--env-muted);
    font-family: var(--font-mono);
    font-size: 0.6875rem;
  }
  .draft-actions-status .is-dirty {
    color: var(--env-pending);
  }
  .draft-actions-status .is-saved {
    color: var(--env-accent);
  }
  .draft-actions-status .is-warn {
    color: var(--env-warn);
  }
  .draft-actions-buttons {
    display: inline-flex;
    gap: 0.5rem;
    flex-shrink: 0;
  }

  /* ── Confirm dialog ── */
  .draft-confirm-line {
    margin: 0 0 0.4rem;
    font-family: var(--font-mono);
    font-size: 0.8125rem;
    overflow-wrap: anywhere;
  }
  .draft-confirm-line strong {
    display: inline-block;
    min-width: 4.5rem;
    color: var(--env-muted);
    font-size: 0.6875rem;
    font-weight: 500;
    letter-spacing: 0.12em;
    text-transform: uppercase;
  }
  .draft-confirm-note {
    margin: 0.75rem 0 0;
    color: var(--env-muted);
    font-size: 0.8125rem;
    line-height: 1.5;
  }
  .draft-confirm-note.is-locked {
    color: var(--env-pending);
  }

  .draft-sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
    white-space: nowrap;
    border: 0;
  }

  @media (max-width: 760px) {
    .draft-review {
      padding: 0.75rem;
      /* One column, one scroller: the document. The desktop overflow: auto
         makes this pane the scroll container, and on iOS a touch that starts
         on the tall sandboxed preview iframe belongs to the pane's scroller
         and never chains out — the action controls under a long forwarded
         HTML message become unreachable. The content sizes the pane instead;
         BodyFrame already sizes the iframe to its document, so the page is
         the only thing that scrolls. */
      overflow: visible;
    }
    .draft-field-row {
      grid-template-columns: 3.75rem minmax(0, 1fr) auto;
      gap: 0.5rem;
    }
    .draft-actions {
      align-items: flex-start;
      flex-direction: column;
    }
  }
</style>
