<script lang="ts">
  import Drawer from './Drawer.svelte';
  import Modal from './Modal.svelte';
  import Button from './Button.svelte';
  import RecipientField from './RecipientField.svelte';
  import Spinner from './Spinner.svelte';
  import {
    api,
    EnvelopeApiError,
    type Account,
    type ComposeAttachment,
    type ComposeResponse
  } from '$lib/api';
  import { getComposerStore, type ComposerMode } from '$lib/composer.svelte';
  import { addrKey, optionalAddrsValid, parseAddrs, serializeAddrs, validateAddrs } from '$lib/addresses';

  type PendingAttachment = ComposeAttachment & { size: number };

  let {
    accounts = [],
    onsent
  }: {
    accounts?: Account[];
    onsent?: (res: ComposeResponse, accountId: string) => void;
  } = $props();

  const composer = getComposerStore();

  let fromAccountId = $state('');
  let toRaw = $state('');
  let ccRaw = $state('');
  let bccRaw = $state('');
  let subject = $state('');
  let body = $state('');
  let bodyFormat = $state<'text' | 'html'>('text');
  let showBcc = $state(false);
  let attachments = $state<PendingAttachment[]>([]);
  let sending = $state(false);
  let readingAttachments = $state(false);
  let sendError = $state<{ code: string; message: string } | null>(null);
  let openSession = $state('');
  // Discard protection: Esc / × / backdrop on a composer with content asks
  // first. There is no autosave yet, so a stray Escape would otherwise throw
  // typed work away with zero network calls.
  let discardConfirmOpen = $state(false);
  const isDirty = $derived(
    toRaw.trim().length > 0 ||
      ccRaw.trim().length > 0 ||
      bccRaw.trim().length > 0 ||
      subject.trim().length > 0 ||
      body.trim().length > 0 ||
      attachments.length > 0
  );
  function requestClose() {
    if (sending) return;
    // Escape while the confirm is showing means "keep editing".
    if (discardConfirmOpen) {
      discardConfirmOpen = false;
      return;
    }
    if (isDirty) {
      // Next tick, not synchronously: the Modal's own window Escape listener
      // evaluates `open` at event time, so opening it during this same keydown
      // would let the very same Escape close it again before it ever painted.
      setTimeout(() => {
        discardConfirmOpen = true;
      }, 0);
      return;
    }
    composer.close();
  }
  function discardDraft() {
    discardConfirmOpen = false;
    composer.close();
  }

  const isFreshMessage = $derived(composer.mode === 'compose' || composer.mode === 'forward');
  const toValid = $derived(optionalAddrsValid(toRaw));
  const recipientReady = $derived(isFreshMessage ? validateAddrs(toRaw) : true);
  // Cc/Bcc are optional, but anything actually typed has to be a usable
  // address — otherwise a malformed Cc rides along on an otherwise valid send
  // and only fails at SMTP time. Only shown (and only populated) on fresh
  // messages; reply mode clears them.
  const ccValid = $derived(optionalAddrsValid(ccRaw));
  const bccValid = $derived(optionalAddrsValid(bccRaw));
  const optionalRecipientsReady = $derived(!isFreshMessage || (ccValid && bccValid));
  const subjectReady = $derived(isFreshMessage ? subject.trim().length > 0 : true);
  const deliveryReady = $derived(fromAccountId.length > 0);

  // Each recipient field must not offer an address the other two already hold.
  const usedAddrs = $derived({
    to: parseAddrs(toRaw).map(addrKey),
    cc: parseAddrs(ccRaw).map(addrKey),
    bcc: parseAddrs(bccRaw).map(addrKey)
  });

  const selectedAccount = $derived(accounts.find((account) => account.id === fromAccountId));
  const accountContext = $derived.by(() => {
    if (!selectedAccount) return 'Choose an account before sending.';
    const name = selectedAccount.display_name || selectedAccount.name || '';
    const address = selectedAccount.username || '';
    return name && name !== address
      ? `Sending from ${name} <${address}>`
      : `Sending from ${address || name}`;
  });

  const modeLabel: Record<ComposerMode, string> = {
    compose: 'New message',
    reply: 'Reply',
    'reply-all': 'Reply all',
    forward: 'Forward'
  };

  const drawerTitle = $derived(modeLabel[composer.mode] ?? 'Compose');
  const canSend = $derived(
    !sending &&
      !readingAttachments &&
      deliveryReady &&
      recipientReady &&
      optionalRecipientsReady &&
      subjectReady
  );

  $effect(() => {
    if (!composer.isOpen) {
      openSession = '';
      return;
    }

    const ctx = composer.context;
    const session = [composer.mode, ctx.accountId, ctx.parentUid, ctx.to, ctx.subject].join(':');
    if (session === openSession) return;
    openSession = session;

    fromAccountId = ctx.accountId || (accounts[0]?.id ?? '');
    // Normalized on the way in so the recipient field's own re-serialization is
    // a no-op rather than an immediate change to the prefilled value.
    toRaw = isFreshMessage ? serializeAddrs(ctx.to ?? '') : '';
    subject = isFreshMessage ? (ctx.subject ?? '') : '';
    body = ctx.bodyPrefix ?? '';
    ccRaw = '';
    bccRaw = '';
    showBcc = false;
    bodyFormat = 'text';
    attachments = [];
    sendError = null;
    discardConfirmOpen = false;
  });

  function attachmentPayloads(): ComposeAttachment[] {
    return attachments.map(({ filename, content_type, data_b64 }) => ({
      filename,
      content_type,
      data_b64
    }));
  }

  async function send() {
    if (!canSend) return;
    sending = true;
    sendError = null;
    const accountId = fromAccountId;

    try {
      let res: ComposeResponse;
      const ctx = composer.context;
      const payloadAttachments = attachmentPayloads();

      if (isFreshMessage) {
        res = await api.compose(accountId, {
          to: toRaw.trim(),
          subject: subject.trim(),
          text: bodyFormat === 'text' ? (body || null) : null,
          html: bodyFormat === 'html' ? (body || null) : null,
          cc: ccRaw.trim() || null,
          bcc: bccRaw.trim() || null,
          attachments: payloadAttachments
        });
      } else {
        res = await api.composeReply(accountId, {
          parent_uid: ctx.parentUid!,
          parent_folder: ctx.parentFolder ?? 'INBOX',
          reply_all: composer.mode === 'reply-all',
          text: bodyFormat === 'text' ? (body || null) : null,
          html: bodyFormat === 'html' ? (body || null) : null,
          attachments: payloadAttachments
        });
      }

      composer.close();
      onsent?.(res, accountId);
    } catch (e) {
      const err = e as EnvelopeApiError;
      sendError = { code: err.code ?? 'unknown', message: err.message ?? 'Could not queue message.' };
    } finally {
      sending = false;
    }
  }

  function handleBodyKey(e: KeyboardEvent) {
    if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') {
      e.preventDefault();
      send();
    }
  }

  async function handleAttachments(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    const files = Array.from(input.files ?? []);
    if (files.length === 0) return;
    readingAttachments = true;

    try {
      const next: PendingAttachment[] = [];
      for (const file of files) {
        const bytes = new Uint8Array(await file.arrayBuffer());
        let binary = '';
        const chunkSize = 0x8000;
        for (let index = 0; index < bytes.length; index += chunkSize) {
          binary += String.fromCharCode(...bytes.subarray(index, index + chunkSize));
        }
        next.push({
          filename: file.name,
          content_type: file.type || 'application/octet-stream',
          data_b64: btoa(binary),
          size: file.size
        });
      }
      attachments = [...attachments, ...next];
    } finally {
      readingAttachments = false;
      input.value = '';
    }
  }

  function removeAttachment(index: number) {
    attachments = attachments.filter((_, attachmentIndex) => attachmentIndex !== index);
  }

  function formatSize(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  }
</script>

{#snippet headerActions()}
  <Button variant="primary" type="submit" form="composer-form" disabled={!canSend}>
    {#if sending}<Spinner label="Queueing" />{/if}
    {sending ? 'Queueing' : 'Send'}
  </Button>
{/snippet}

<Drawer
  open={composer.isOpen}
  title={drawerTitle}
  eyebrow="Draft window"
  subtitle={accountContext}
  size="wide"
  actions={headerActions}
  onclose={requestClose}
>
  <form id="composer-form" class="composer-form" onsubmit={(event) => { event.preventDefault(); send(); }}>
    <div class="composer-scroll">
      <section class="composer-card composer-addresses" aria-label="Message addressing">
        <div class="composer-field-row">
          <label for="composer-from">From</label>
          <select id="composer-from" bind:value={fromAccountId} disabled={sending}>
            {#if accounts.length === 0}<option value="">No accounts available</option>{/if}
            {#each accounts as account (account.id)}
              <option value={account.id}>
                {account.display_name || account.name} &lt;{account.username}&gt;
              </option>
            {/each}
          </select>
        </div>

        {#if isFreshMessage}
          <RecipientField
            id="composer-to"
            label="To"
            bind:value={toRaw}
            accountId={fromAccountId}
            disabled={sending}
            exclude={[...usedAddrs.cc, ...usedAddrs.bcc]}
            placeholder="recipient@example.com"
            invalid={!toValid && toRaw.trim() !== ''}
          />
          {#if !toValid && toRaw.trim() !== ''}
            <p class="composer-validation-note">Enter valid email addresses separated by commas.</p>
          {/if}
          <RecipientField
            id="composer-cc"
            label="Cc"
            bind:value={ccRaw}
            accountId={fromAccountId}
            disabled={sending}
            exclude={[...usedAddrs.to, ...usedAddrs.bcc]}
            placeholder="Optional"
            invalid={!ccValid}
          />
          {#if !ccValid}
            <p class="composer-validation-note">Enter valid Cc addresses separated by commas.</p>
          {/if}
          {#if showBcc}
            <RecipientField
              id="composer-bcc"
              label="Bcc"
              bind:value={bccRaw}
              accountId={fromAccountId}
              disabled={sending}
              exclude={[...usedAddrs.to, ...usedAddrs.cc]}
              placeholder="Optional"
              invalid={!bccValid}
            />
            {#if !bccValid}
              <p class="composer-validation-note">Enter valid Bcc addresses separated by commas.</p>
            {/if}
          {/if}
          <div class="composer-field-row">
            <label for="composer-subject">Subject</label>
            <input id="composer-subject" type="text" placeholder="Subject" bind:value={subject} disabled={sending} />
            {#if !showBcc}
              <button class="bcc-toggle" type="button" onclick={() => (showBcc = true)}>Bcc</button>
            {/if}
          </div>
        {:else}
          <div class="composer-reply-context">
            Replying to {composer.mode === 'reply-all' ? 'all recipients of' : 'the sender of'} message #{composer.context.parentUid}
          </div>
        {/if}
      </section>

      <section class="composer-review" aria-label="Pre-send review">
        <div class="composer-check">
          <span>Recipient</span>
          <strong class:is-ready={recipientReady}>{recipientReady ? (isFreshMessage ? 'Ready' : 'From original') : 'Required'}</strong>
        </div>
        <div class="composer-check">
          <span>Subject</span>
          <strong class:is-ready={subjectReady}>{subjectReady ? (isFreshMessage ? 'Ready' : 'Preserved') : 'Required'}</strong>
        </div>
        <div class="composer-check">
          <span>Delivery</span>
          <strong class:is-ready={deliveryReady} class:is-error={!deliveryReady}>{deliveryReady ? 'Account ready' : 'Select account'}</strong>
        </div>
      </section>

      <section class="composer-editor" aria-label="Message editor">
        <div class="composer-editor-toolbar">
          <div class="composer-format" role="group" aria-label="Message format">
            <button type="button" class:is-active={bodyFormat === 'text'} aria-pressed={bodyFormat === 'text'} onclick={() => (bodyFormat = 'text')}>Text</button>
            <button type="button" class:is-active={bodyFormat === 'html'} aria-pressed={bodyFormat === 'html'} onclick={() => (bodyFormat = 'html')}>HTML</button>
          </div>
          <label class="composer-attach-button" for="composer-attachments">Add attachment</label>
          <input id="composer-attachments" type="file" multiple hidden onchange={handleAttachments} />
        </div>
        <label class="sr-only" for="composer-body">Message</label>
        <textarea
          id="composer-body"
          placeholder="Write your message"
          bind:value={body}
          disabled={sending}
          onkeydown={handleBodyKey}
        ></textarea>
      </section>

      <section class="composer-card composer-attachment-list" aria-labelledby="attachment-list-label">
        <div class="attachment-list-head">
          <p id="attachment-list-label">Attachments</p>
          <span>{attachments.length === 0 ? 'None' : `${attachments.length} file${attachments.length === 1 ? '' : 's'}`}</span>
        </div>
        {#if readingAttachments}
          <p class="attachment-empty">Preparing files…</p>
        {:else if attachments.length === 0}
          <p class="attachment-empty">No files attached.</p>
        {:else}
          <ul>
            {#each attachments as attachment, index (`${attachment.filename}:${index}`)}
              <li>
                <span class="attachment-name" title={attachment.filename}>{attachment.filename}</span>
                <span>{formatSize(attachment.size)}</span>
                <button type="button" aria-label={`Remove ${attachment.filename}`} onclick={() => removeAttachment(index)}>Remove</button>
              </li>
            {/each}
          </ul>
        {/if}
      </section>

      {#if sendError}
        <p class="composer-error" role="alert">
          {sendError.message} <span>{sendError.code}</span>
        </p>
      {/if}
    </div>

    <footer class="composer-footer">
      <span class:ready={canSend}>{canSend ? 'Ready to send.' : 'Complete the required fields before sending.'}</span>
    </footer>
  </form>
</Drawer>

<Modal
  open={discardConfirmOpen}
  title="Discard this draft?"
  onclose={() => (discardConfirmOpen = false)}
>
  <p class="discard-warn">Nothing has been saved yet — closing now throws away what you typed.</p>
  {#snippet footer()}
    <button type="button" class="modal-keep" onclick={() => (discardConfirmOpen = false)}>Keep editing</button>
    <button type="button" class="modal-discard" onclick={discardDraft}>Discard draft</button>
  {/snippet}
</Modal>

<style>
  .composer-form {
    height: 100%;
    min-height: 0;
    display: flex;
    flex-direction: column;
    background: var(--env-surface);
  }
  .composer-scroll {
    flex: 1;
    min-height: 0;
    overflow: auto;
    display: grid;
    grid-template-rows: auto auto minmax(22rem, 1fr) auto auto;
    align-content: stretch;
    gap: 0.75rem;
    padding: 0.875rem;
    background: var(--env-surface);
  }
  .composer-card,
  .composer-editor {
    min-width: 0;
    border: 1px solid var(--env-rule);
    background: var(--env-surface);
  }
  .composer-addresses {
    padding: 0.25rem 0.875rem;
  }
  .composer-field-row {
    min-height: 2.625rem;
    display: grid;
    grid-template-columns: 4.75rem minmax(0, 1fr) auto;
    align-items: center;
    gap: 0.75rem;
    border-bottom: 1px solid var(--env-rule);
  }
  .composer-field-row:last-child {
    border-bottom: 0;
  }
  .composer-field-row label,
  .attachment-list-head p {
    margin: 0;
    color: var(--env-muted);
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    letter-spacing: 0.12em;
    text-transform: uppercase;
  }
  .composer-field-row input,
  .composer-field-row select {
    width: 100%;
    min-width: 0;
    border: 0;
    outline: 0;
    background: transparent;
    color: var(--env-ink);
    font-family: var(--font-mono);
    font-size: 0.8125rem;
  }
  .composer-field-row input::placeholder {
    color: #aaa69e;
  }
  .composer-field-row:focus-within {
    box-shadow: inset 2px 0 0 var(--env-accent);
  }
  .composer-validation-note {
    margin: 0 -0.875rem;
    padding: 0.4rem 0.875rem;
    border-bottom: 1px solid var(--env-rule);
    color: var(--env-warn);
    font-size: 0.75rem;
  }
  .bcc-toggle {
    border: 0;
    background: transparent;
    color: var(--env-accent);
    cursor: pointer;
    font-family: var(--font-mono);
    font-size: 0.6875rem;
  }
  .composer-reply-context {
    min-height: 2.625rem;
    display: flex;
    align-items: center;
    color: var(--env-muted);
    font-family: var(--font-mono);
    font-size: 0.75rem;
  }
  .composer-review {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 1px;
    border: 1px solid var(--env-rule);
    background: var(--env-rule);
  }
  .composer-check {
    min-height: 2.75rem;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.75rem;
    padding: 0.55rem 0.75rem;
    background: var(--env-surface);
  }
  .composer-check span,
  .composer-check strong {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
  }
  .composer-check span {
    color: var(--env-muted);
    letter-spacing: 0.12em;
    text-transform: uppercase;
  }
  .composer-check strong {
    color: var(--env-pending);
    font-weight: 400;
  }
  .composer-check strong.is-ready { color: var(--env-accent); }
  .composer-check strong.is-error { color: var(--env-warn); }
  .composer-editor {
    min-height: 0;
    display: flex;
    flex-direction: column;
  }
  .composer-editor-toolbar {
    min-height: 2.875rem;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.75rem;
    padding: 0.45rem 0.65rem;
    border-bottom: 1px solid var(--env-rule);
    background: var(--env-paper);
  }
  .composer-format {
    display: inline-grid;
    grid-template-columns: repeat(2, minmax(3.5rem, auto));
    border: 1px solid var(--env-rule);
    background: var(--env-surface);
  }
  .composer-format button {
    min-height: 1.875rem;
    border: 0;
    border-right: 1px solid var(--env-rule);
    background: transparent;
    color: var(--env-muted);
    cursor: pointer;
    font-family: var(--font-mono);
    font-size: 0.6875rem;
  }
  .composer-format button:last-child { border-right: 0; }
  .composer-format button.is-active { background: var(--env-ink); color: var(--env-surface); }
  .composer-attach-button {
    min-height: 1.875rem;
    display: inline-flex;
    align-items: center;
    border: 1px solid var(--env-rule);
    background: var(--env-surface);
    color: var(--env-ink);
    cursor: pointer;
    padding: 0 0.75rem;
    font-family: var(--font-mono);
    font-size: 0.6875rem;
  }
  #composer-body {
    flex: 1;
    min-height: 22rem;
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
  .composer-attachment-list {
    min-height: 5.25rem;
    padding: 0.25rem 0.875rem 0.65rem;
  }
  .attachment-list-head {
    min-height: 2.375rem;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
  }
  .attachment-list-head span,
  .attachment-empty,
  .composer-attachment-list li {
    color: var(--env-muted);
    font-family: var(--font-mono);
    font-size: 0.6875rem;
  }
  .attachment-empty { margin: 0.25rem 0 0; }
  .composer-attachment-list ul { list-style: none; margin: 0; padding: 0; }
  .composer-attachment-list li {
    min-height: 2rem;
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto auto;
    align-items: center;
    gap: 0.75rem;
    border-top: 1px solid var(--env-rule);
  }
  .attachment-name { overflow: hidden; color: var(--env-ink); text-overflow: ellipsis; white-space: nowrap; }
  .composer-attachment-list li button {
    border: 0;
    background: transparent;
    color: var(--env-warn);
    cursor: pointer;
    font: inherit;
  }
  .composer-error {
    margin: 0;
    padding: 0.65rem 0.75rem;
    border: 1px solid var(--env-warn);
    background: var(--env-warn-soft);
    color: var(--env-warn);
    font-size: 0.8125rem;
  }
  .composer-error span {
    margin-left: 0.35rem;
    font-family: var(--font-mono);
    font-size: 0.6875rem;
  }
  .composer-footer {
    min-height: 3rem;
    display: flex;
    align-items: center;
    justify-content: flex-end;
    padding: 0.65rem 1.125rem;
    border-top: 1px solid var(--env-rule);
    background: var(--env-paper);
  }
  .composer-footer span {
    color: var(--env-muted);
    font-family: var(--font-mono);
    font-size: 0.6875rem;
  }
  .composer-footer span.ready { color: var(--env-accent); }
  .sr-only {
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
    .composer-scroll {
      grid-template-rows: auto auto minmax(18rem, 1fr) auto auto;
      gap: 0.625rem;
      padding: 0.625rem;
    }
    .composer-field-row {
      grid-template-columns: 3.75rem minmax(0, 1fr) auto;
      gap: 0.5rem;
    }
    .composer-review { grid-template-columns: 1fr; }
    #composer-body { min-height: 18rem; padding: 0.875rem; }
  }
  .discard-warn { margin: 0; font-size: 0.875rem; }
  .modal-keep,
  .modal-discard {
    font: inherit;
    padding: 0.4rem 0.9rem;
    border-radius: 6px;
    border: 1px solid var(--env-rule);
    background: transparent;
    cursor: pointer;
  }
  .modal-discard {
    color: #fff;
    background: var(--env-danger, #b42318);
    border-color: var(--env-danger, #b42318);
  }
</style>
