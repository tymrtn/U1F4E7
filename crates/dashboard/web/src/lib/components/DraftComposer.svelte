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
  //   • CSRF is handled by the shared request() helper in $lib/api. Nothing
  //     here bypasses it, and there is no direct-send path.

  import { page } from '$app/state';
  import { optionalAddrsValid, validateAddrs } from '$lib/addresses';
  import Badge from './Badge.svelte';
  import Button from './Button.svelte';
  import Modal from './Modal.svelte';
  import MonoTag from './MonoTag.svelte';
  import Spinner from './Spinner.svelte';
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
    pending_review: { label: 'Pending review', variant: 'pending', note: '' },
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
  let showBcc = $state(false);
  let baseline = $state<Snapshot>({ to: '', cc: '', bcc: '', subject: '', body: '', format: 'text' });

  // ── Action state ──────────────────────────────────────────────────────

  let saving = $state(false);
  let saved = $state(false);
  let queueing = $state(false);
  let confirmOpen = $state(false);
  let queued = $state<DraftQueuedResponse | null>(null);
  let conflict = $state(false);
  let actionError = $state<{ code: string; message: string } | null>(null);

  // ── Derived ───────────────────────────────────────────────────────────

  /**
   * Guard for every mutating action: the draft in hand must be the draft the
   * URL currently names. Comparing the fetch-time route key (rather than
   * `draft.account_id`) keeps this correct for the account-by-email form of the
   * URL, which the API resolves server-side.
   */
  const identityMatches = $derived(!!draft && loadedForKey === routeKey && draft.id === draftId);

  const statusMeta = $derived(draft ? STATUS_META[draft.status] : null);
  const statusEditable = $derived(!!draft && isEditableDraftStatus(draft.status));

  /**
   * A persisted `send_after` means the draft is ALREADY sitting in the outbox,
   * whether or not this page is the one that put it there.
   *
   * `list_drafts_due_for_send` selects every `draft`-status row whose
   * `send_after` has come due, and the edit statement does not clear
   * `send_after` — it only strips the approval attestation. So editing a queued
   * draft does not pull it back: the edited content still ships, just without
   * `tyler_approved`. The queued state therefore has to be recovered from the
   * draft itself on every load, not just from the response of a send this page
   * performed.
   *
   * Terminal rows keep the `send_after` they were queued with, so this is
   * scoped to statuses the sweep can still act on.
   */
  const persistedQueue = $derived(
    !!draft && draft.send_after != null && isSendableDraftStatus(draft.status)
  );
  const isQueued = $derived(!!queued || persistedQueue);
  const queuedAt = $derived(queued?.send_after ?? draft?.send_after ?? null);

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
  }

  async function load() {
    const generation = ++loadGeneration;
    const forKey = routeKey;
    const requestedAccount = accountId;
    const requestedDraft = draftId;

    loading = true;
    notFound = false;
    loadError = null;
    conflict = false;
    actionError = null;
    queued = null;
    saved = false;

    try {
      const res = await api.draft(requestedAccount, requestedDraft);
      if (generation !== loadGeneration) return;
      applyDraft(res.draft);
      loadedForKey = forKey;
      accountLabel = res.account?.username ?? res.draft.account_id;
    } catch (e) {
      if (generation !== loadGeneration) return;
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
      if (generation === loadGeneration) loading = false;
    }
  }

  /** Adopt a server draft as the new editing baseline. */
  function applyDraft(next: Draft) {
    // A draft with only an HTML body opens in HTML mode; everything else opens
    // in plain text. Saving writes back exactly the format shown, which clears
    // the other alternate server-side — that is what stops a stale HTML part
    // from being delivered instead of the edit.
    const format: BodyFormat =
      next.text_content == null && next.html_content != null ? 'html' : 'text';

    draft = next;
    toRaw = next.to_addr ?? '';
    ccRaw = next.cc_addr ?? '';
    bccRaw = next.bcc_addr ?? '';
    subjectRaw = next.subject ?? '';
    bodyRaw = (format === 'html' ? next.html_content : next.text_content) ?? '';
    bodyFormat = format;
    showBcc = bccRaw.length > 0;
    conflict = false;
    baseline = snapshot();
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

  async function confirmSend() {
    if (!draft || queueing || !canSend) return;
    // Pin the target: a route change mid-flight must not land this queue
    // result on, or attribute it to, whatever draft is on screen afterwards.
    const generation = loadGeneration;
    const targetAccount = accountId;
    const targetDraft = draftId;

    queueing = true;
    conflict = false;
    actionError = null;

    try {
      const res = await api.sendDraft(targetAccount, targetDraft, {
        confirm: true,
        expected_revision: draft.revision
      });
      if (generation !== loadGeneration) return;
      queued = res;
      confirmOpen = false;
    } catch (e) {
      if (generation !== loadGeneration) return;
      confirmOpen = false;
      handleActionError(e, 'Could not queue this draft.');
    } finally {
      if (generation === loadGeneration) queueing = false;
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

  function formatWhen(iso: string): string {
    const ms = Date.parse(iso.includes('Z') || iso.includes('+') ? iso : `${iso}Z`);
    if (Number.isNaN(ms)) return iso;
    return new Date(ms).toLocaleString([], {
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit'
    });
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
      </div>
    {/if}

    {#if isQueued}
      <div class="draft-banner is-queued" id="draft-queued" role="status">
        <p class="draft-banner-title">Queued for sending</p>
        <p>
          This message is waiting in the Envelope outbox{queuedAt
            ? ` until ${formatWhen(queuedAt)}`
            : ''}, which gives you time to catch a mistake. Envelope's scheduled sender delivers it
          after that — nothing has been transmitted yet. It is locked while it waits, because
          editing it here would not pull it back out of the queue.
        </p>
        {#if queued}
          <MonoTag>{queued.queued_reason_code}</MonoTag>
        {/if}
      </div>
    {/if}

    <div class="draft-card draft-addresses">
      <div class="draft-field-row is-static">
        <span class="draft-field-label">From</span>
        <span class="draft-field-value">{accountLabel || accountId}</span>
      </div>

      <div class="draft-field-row">
        <label for="draft-to">To</label>
        <input
          id="draft-to"
          type="text"
          inputmode="email"
          autocomplete="off"
          spellcheck="false"
          placeholder="recipient@example.com"
          bind:value={toRaw}
          disabled={inputsLocked}
        />
      </div>

      <div class="draft-field-row">
        <label for="draft-cc">Cc</label>
        <input
          id="draft-cc"
          type="text"
          inputmode="email"
          autocomplete="off"
          spellcheck="false"
          placeholder="Optional"
          bind:value={ccRaw}
          disabled={inputsLocked}
        />
        {#if !showBcc && !inputsLocked}
          <button class="draft-bcc-toggle" type="button" onclick={() => (showBcc = true)}>Bcc</button>
        {/if}
      </div>

      {#if showBcc}
        <div class="draft-field-row">
          <label for="draft-bcc">Bcc</label>
          <input
            id="draft-bcc"
            type="text"
            inputmode="email"
            autocomplete="off"
            spellcheck="false"
            placeholder="Optional"
            bind:value={bccRaw}
            disabled={inputsLocked}
          />
        </div>
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
            onclick={() => (bodyFormat = 'text')}>Text</button
          >
          <button
            type="button"
            class:is-active={bodyFormat === 'html'}
            aria-pressed={bodyFormat === 'html'}
            disabled={inputsLocked}
            onclick={() => (bodyFormat = 'html')}>HTML</button
          >
        </div>
        {#if bodyChanged && hasBothBodies}
          <p class="draft-format-note">
            This draft has both a plain-text and an HTML version. Saving replaces the body with
            what you see here and drops the other format.
          </p>
        {/if}
      </div>
      <label class="draft-sr-only" for="draft-body">Message</label>
      <textarea
        id="draft-body"
        placeholder="Write your message"
        bind:value={bodyRaw}
        disabled={inputsLocked}
      ></textarea>
    </div>

    {#if draft.attachments.length > 0}
      <p class="draft-attachment-note">
        {draft.attachments.length}
        {draft.attachments.length === 1 ? 'attachment stays' : 'attachments stay'} on this draft. Editing
        the message here does not change them.
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
          <span class="is-saved">Waiting in the outbox — locked until it sends.</span>
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
          <Button variant="primary" disabled={!canSend} onclick={requestSend}>Send</Button>
        {/if}
      </div>
    </footer>
  {/if}
</section>

<Modal open={confirmOpen} title="Queue this draft for sending?" onclose={closeConfirm}>
  <p class="draft-confirm-line">
    <strong>To</strong>
    {toRaw.trim() || '(no recipient)'}
  </p>
  <p class="draft-confirm-line">
    <strong>Subject</strong>
    {subjectRaw.trim() || '(no subject)'}
  </p>
  <p class="draft-confirm-note">
    Envelope holds this message in the outbox for a short cooldown, then sends it on your behalf.
    You are approving this exact version — editing it afterwards withdraws that approval.
  </p>
  {#if queueing}
    <p class="draft-confirm-note is-locked">
      Queueing this message. This cannot be cancelled here — wait for it to finish.
    </p>
  {/if}
  {#snippet footer()}
    <Button variant="ghost" disabled={queueing} onclick={closeConfirm}>Keep editing</Button>
    <Button variant="primary" disabled={queueing} onclick={confirmSend}>
      {#if queueing}<Spinner label="Queueing" />{/if}
      Queue for sending
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
  .is-queued {
    border-color: var(--env-accent);
    background: var(--env-accent-soft);
    color: var(--env-accent);
  }
  .is-queued .draft-banner-title {
    color: var(--env-accent);
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

  /* ── Editor ── */
  .draft-editor {
    flex: 1;
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
  .draft-attachment-note {
    margin: 0;
    color: var(--env-muted);
    font-family: var(--font-mono);
    font-size: 0.6875rem;
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
