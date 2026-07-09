<script lang="ts">
  // ReaderPane — full v2 message reader.
  //
  // Owned deliverables in one component:
  //   • Sandboxed HTML rendering via BodyFrame (srcdoc + sandbox=allow-same-origin)
  //   • Text/HTML toggle (session-persistent, default HTML when present)
  //   • Headers block with from/to/cc/date/subject; to+cc collapsed behind Details
  //   • Thread strip (ThreadStrip)
  //   • Attachment list (AttachmentList)
  //   • Explicit read toggle (never auto-marks read — invariant)
  //   • MonoTag copy affordances for uid + message-id (click-to-copy, toast)

  import { page } from '$app/state';
  import { Spinner, MonoTag, Badge, Toast } from '$lib/components';
  import BodyFrame from '$lib/components/BodyFrame.svelte';
  import ThreadStrip from '$lib/components/ThreadStrip.svelte';
  import AttachmentList from '$lib/components/AttachmentList.svelte';
  import {
    fetchMessageDetail,
    fetchThread,
    postFlags,
    isSeen,
    type MessageDetailFull,
    type ThreadMessage
  } from '$lib/reader-api';
  import { EnvelopeApiError } from '$lib/api';

  // ── Route params ──────────────────────────────────────────────────────

  const accountId = $derived(page.params.account ?? '');
  const uid = $derived(Number(page.params.uid ?? 0));
  const folder = $derived(page.url.searchParams.get('folder') ?? 'INBOX');
  const box = $derived(page.params.box ?? 'unified');

  // ── Message state ─────────────────────────────────────────────────────

  let message = $state<MessageDetailFull | null>(null);
  let loading = $state(false);
  let error = $state<{ code: string; message: string } | null>(null);
  let loadKey = $state('');

  // ── Thread state ──────────────────────────────────────────────────────

  let threadMessages = $state<ThreadMessage[]>([]);
  let threadLoading = $state(false);

  // ── Read-toggle state ─────────────────────────────────────────────────

  let flagging = $state(false);
  let localSeen = $state<boolean | null>(null); // null = use message.flags

  let isRead = $derived(() => {
    if (localSeen !== null) return localSeen;
    if (!message) return false;
    return isSeen(message.flags);
  });

  // ── View toggle (text / html) — session-persistent ───────────────────

  const TOGGLE_KEY = 'envelope_reader_body_format';

  function getSessionFormat(): 'html' | 'text' {
    try {
      return (sessionStorage.getItem(TOGGLE_KEY) as 'html' | 'text') || 'html';
    } catch {
      return 'html';
    }
  }

  function setSessionFormat(f: 'html' | 'text') {
    try {
      sessionStorage.setItem(TOGGLE_KEY, f);
    } catch {
      // ignore
    }
    bodyFormat = f;
  }

  let bodyFormat = $state<'html' | 'text'>(getSessionFormat());

  // Derived: what format actually renders (html only if html_body present).
  let effectiveFormat = $derived(() => {
    if (!message) return 'text';
    if (bodyFormat === 'html' && message.html_body) return 'html';
    if (message.text_body) return 'text';
    if (message.html_body) return 'html';
    return 'text';
  });

  // ── Remote images ─────────────────────────────────────────────────────

  let remoteImages = $state(false);
  let remoteBlockedCount = $state(0);

  function onRemoteBlocked(count: number) {
    remoteBlockedCount = count;
  }

  // ── Details disclosure (to/cc) ────────────────────────────────────────

  let headersExpanded = $state(false);

  // ── Toast ─────────────────────────────────────────────────────────────

  let toast = $state<{ text: string; variant: 'ok' | 'warn' } | null>(null);
  let toastTimer: ReturnType<typeof setTimeout> | null = null;

  function showToast(text: string, variant: 'ok' | 'warn' = 'ok') {
    if (toastTimer) clearTimeout(toastTimer);
    toast = { text, variant };
    toastTimer = setTimeout(() => {
      toast = null;
    }, 2500);
  }

  // ── Copy affordance ───────────────────────────────────────────────────

  async function copyToClipboard(text: string, label: string) {
    try {
      await navigator.clipboard.writeText(text);
      showToast(`Copied ${label}`);
    } catch {
      showToast('Copy failed', 'warn');
    }
  }

  // ── Load ──────────────────────────────────────────────────────────────

  async function load(acct: string, u: number, f: string) {
    loading = true;
    error = null;
    message = null;
    threadMessages = [];
    localSeen = null;
    remoteImages = false;
    remoteBlockedCount = 0;

    try {
      const res = await fetchMessageDetail(acct, u, f);
      message = res.message;

      // Thread: load if message_id is present (fire-and-forget, no blocking).
      if (message.message_id) {
        threadLoading = true;
        fetchThread(acct, message.message_id)
          .then((thread) => {
            threadMessages = thread?.messages ?? [];
          })
          .catch(() => {
            threadMessages = [];
          })
          .finally(() => {
            threadLoading = false;
          });
      }

      // Auto-select html if present, text otherwise (session default wins).
      if (bodyFormat === 'html' && !message.html_body && message.text_body) {
        // Session prefers html but only text available — render text quietly.
      }
    } catch (e) {
      const err = e as EnvelopeApiError;
      error = {
        code: err.code ?? 'reader_load_error',
        message: err.message ?? 'Failed to load this message.'
      };
    } finally {
      loading = false;
    }
  }

  $effect(() => {
    const key = `${accountId}:${uid}:${folder}`;
    if (accountId && uid && key !== loadKey) {
      loadKey = key;
      load(accountId, uid, folder);
    }
  });

  // ── Mark read/unread ──────────────────────────────────────────────────

  async function toggleRead() {
    if (!message || flagging) return;
    flagging = true;
    const currentlyRead = isRead();
    const add = currentlyRead ? [] : ['\\Seen'];
    const remove = currentlyRead ? ['\\Seen'] : [];
    try {
      await postFlags(accountId, uid, folder, add, remove);
      localSeen = !currentlyRead;
      showToast(currentlyRead ? 'Marked unread' : 'Marked read');
    } catch (e) {
      const err = e as EnvelopeApiError;
      showToast(err.message ?? 'Could not update flag', 'warn');
    } finally {
      flagging = false;
    }
  }

  // ── Date formatting ───────────────────────────────────────────────────

  function fmtAbsolute(iso: string | null): string {
    if (!iso) return '';
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    return d.toLocaleString(undefined, {
      year: 'numeric',
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit'
    });
  }

  function fmtRelative(iso: string | null): string {
    if (!iso) return '';
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return '';
    const diffMs = Date.now() - d.getTime();
    const diffMins = Math.floor(diffMs / 60000);
    if (diffMins < 1) return 'just now';
    if (diffMins < 60) return `${diffMins}m ago`;
    const diffH = Math.floor(diffMins / 60);
    if (diffH < 24) return `${diffH}h ago`;
    const diffD = Math.floor(diffH / 24);
    if (diffD < 30) return `${diffD}d ago`;
    return '';
  }

  function addrList(multi: string[] | undefined | null, single: string): string {
    if (multi && multi.length > 0) return multi.join(', ');
    return single || '';
  }
</script>

<div class="reader-pane" id="reader-pane">
  <!-- Toast region -->
  {#if toast}
    <div class="reader-toast-region">
      <Toast variant={toast.variant} onclose={() => (toast = null)}>{toast.text}</Toast>
    </div>
  {/if}

  {#if loading}
    <div class="reader-loading">
      <Spinner label="Loading message" />
      <span>Loading…</span>
    </div>
  {:else if error}
    <div class="reader-error" role="alert">
      <p class="reader-error-msg">Couldn't load this message.</p>
      <p class="reader-error-detail">{error.message}</p>
      <p><MonoTag>{error.code}</MonoTag></p>
      <button class="reader-retry" type="button" onclick={() => load(accountId, uid, folder)}>
        Try again
      </button>
    </div>
  {:else if message}
    <article class="msg" id="msg-{message.uid}">
      <!-- ── Header ──────────────────────────────────────────────────── -->
      <header class="msg-head">
        <h1 class="msg-subject">{message.subject || '(no subject)'}</h1>
        <div class="msg-head-actions">
          {#if isRead()}
            <Badge variant="ok">Read</Badge>
          {:else}
            <Badge variant="warn">Unread</Badge>
          {/if}
          <button
            class="reader-action-btn"
            type="button"
            disabled={flagging}
            onclick={toggleRead}
            aria-label={isRead() ? 'Mark unread' : 'Mark read'}
          >
            {isRead() ? 'Mark unread' : 'Mark read'}
          </button>
        </div>
      </header>

      <!-- Reading note — plain language, no protocol jargon -->
      <p class="reader-read-note">Reading here never marks messages as read.</p>

      <!-- ── Thread strip ───────────────────────────────────────────── -->
      {#if threadLoading || threadMessages.length > 1}
        <ThreadStrip
          messages={threadMessages}
          currentUid={uid}
          {folder}
          {box}
          {accountId}
          loading={threadLoading}
        />
      {/if}

      <!-- ── Meta block ─────────────────────────────────────────────── -->
      <dl class="msg-meta">
        <dt>From</dt>
        <dd class="msg-meta-from">{message.from_addr}</dd>

        {#if message.date}
          <dt>Date</dt>
          <dd class="msg-meta-date">
            {fmtAbsolute(message.date)}
            {#if fmtRelative(message.date)}
              <span class="msg-meta-rel">{fmtRelative(message.date)}</span>
            {/if}
          </dd>
        {/if}

        <!-- Collapsible to/cc details -->
        <dt>
          <button
            class="msg-details-toggle"
            type="button"
            aria-expanded={headersExpanded}
            onclick={() => (headersExpanded = !headersExpanded)}
          >
            {headersExpanded ? 'Hide details' : 'Details'}
          </button>
        </dt>
        <dd>
          {#if !headersExpanded}
            <span class="msg-meta-to-brief">{addrList(message.to_addrs, message.to_addr)}</span>
          {/if}
        </dd>

        {#if headersExpanded}
          <dt>To</dt>
          <dd>{addrList(message.to_addrs, message.to_addr)}</dd>
          {#if message.cc_addr || (message.cc_addrs && message.cc_addrs.length > 0)}
            <dt>Cc</dt>
            <dd>{addrList(message.cc_addrs, message.cc_addr ?? '')}</dd>
          {/if}
          {#if message.message_id}
            <dt>Message-ID</dt>
            <dd>
              <button
                class="copy-btn"
                type="button"
                onclick={() => copyToClipboard(message!.message_id!, 'Message-ID')}
                title="Copy Message-ID"
              >
                <MonoTag>{message.message_id}</MonoTag>
              </button>
            </dd>
          {/if}
          <dt>UID</dt>
          <dd>
            <button
              class="copy-btn"
              type="button"
              onclick={() => copyToClipboard(String(message!.uid), 'UID')}
              title="Copy UID"
            >
              <MonoTag>uid {message.uid}</MonoTag>
            </button>
          </dd>
        {/if}
      </dl>

      <!-- ── Body toggle + remote image notice ─────────────────────── -->
      <div class="msg-body-toolbar">
        {#if message.html_body && message.text_body}
          <span class="body-toggle" role="group" aria-label="Body format">
            <button
              class="body-toggle-btn"
              class:is-active={bodyFormat === 'html'}
              type="button"
              onclick={() => setSessionFormat('html')}
            >
              HTML
            </button>
            <button
              class="body-toggle-btn"
              class:is-active={bodyFormat === 'text'}
              type="button"
              onclick={() => setSessionFormat('text')}
            >
              Plain text
            </button>
          </span>
        {:else if message.html_body && !message.text_body}
          <span class="body-format-note">HTML only</span>
        {:else if !message.html_body && message.text_body}
          <span class="body-format-note">Plain text only</span>
        {/if}

        {#if effectiveFormat() === 'html' && remoteBlockedCount > 0 && !remoteImages}
          <button
            class="remote-img-btn"
            type="button"
            onclick={() => (remoteImages = true)}
          >
            Load remote images ({remoteBlockedCount} blocked)
          </button>
        {/if}
      </div>

      <!-- ── Body ──────────────────────────────────────────────────── -->
      <div class="msg-body">
        {#if effectiveFormat() === 'html' && message.html_body}
          <BodyFrame
            html={message.html_body}
            {remoteImages}
            {onRemoteBlocked}
          />
        {:else if message.text_body}
          <pre class="msg-text">{message.text_body}</pre>
        {:else}
          <p class="msg-empty">This message has no readable body.</p>
        {/if}
      </div>

      <!-- ── Attachments ────────────────────────────────────────────── -->
      {#if message.attachments && message.attachments.length > 0}
        <AttachmentList
          attachments={message.attachments}
          {accountId}
          uid={message.uid}
          {folder}
        />
      {/if}
    </article>
  {:else}
    <!-- Empty / no-message-selected state -->
    <div class="reader-empty" id="reader-empty">
      <p class="reader-empty-msg">Select a message to read it.</p>
      <p class="reader-empty-note">Reading here never marks messages as read.</p>
    </div>
  {/if}
</div>

<style>
  .reader-pane {
    position: relative;
    padding: 1.25rem 1.5rem;
    max-width: 44rem;
    width: 100%;
  }

  /* Toast anchored top-right of the pane */
  .reader-toast-region {
    position: absolute;
    top: 1rem;
    right: 1rem;
    z-index: 10;
  }

  /* Loading */
  .reader-loading {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.8125rem;
    color: var(--env-muted);
    padding: 2rem 0;
  }

  /* Error */
  .reader-error {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }
  .reader-error-msg {
    margin: 0;
    font-weight: 600;
    color: var(--env-warn);
  }
  .reader-error-detail {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--env-muted);
  }
  .reader-retry {
    align-self: flex-start;
    margin-top: 0.25rem;
    font-size: 0.8125rem;
    color: var(--env-accent);
    background: none;
    border: none;
    padding: 0;
    cursor: pointer;
    text-decoration: underline;
  }

  /* Empty state */
  .reader-empty {
    padding: 2rem 0;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }
  .reader-empty-msg {
    margin: 0;
    font-size: 0.9375rem;
    color: var(--env-muted);
  }
  .reader-empty-note {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--env-muted);
    opacity: 0.7;
  }

  /* Header */
  .msg-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 0.75rem;
    margin-bottom: 0.25rem;
  }
  .msg-subject {
    margin: 0;
    font-size: 1.0625rem;
    font-weight: 600;
    line-height: 1.3;
    color: var(--env-ink);
  }
  .msg-head-actions {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    flex-shrink: 0;
  }

  /* Reader note */
  .reader-read-note {
    margin: 0 0 0.75rem;
    font-size: 0.75rem;
    color: var(--env-muted);
  }

  /* Meta dl */
  .msg-meta {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 0.25rem 0.85rem;
    margin: 0 0 0.75rem;
    padding-bottom: 0.75rem;
    border-bottom: 1px solid var(--env-rule);
  }
  .msg-meta dt {
    font-family: var(--font-mono);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--env-muted);
    padding-top: 0.1rem;
    white-space: nowrap;
  }
  .msg-meta dd {
    margin: 0;
    font-size: 0.8125rem;
    overflow-wrap: anywhere;
  }
  .msg-meta-date {
    display: flex;
    align-items: baseline;
    gap: 0.45rem;
    flex-wrap: wrap;
  }
  .msg-meta-rel {
    font-size: 0.6875rem;
    color: var(--env-muted);
  }
  .msg-meta-from {
    font-weight: 500;
  }
  .msg-meta-to-brief {
    color: var(--env-muted);
    font-size: 0.8125rem;
  }

  /* Details toggle */
  .msg-details-toggle {
    background: none;
    border: none;
    padding: 0;
    font-family: var(--font-mono);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--env-accent);
    cursor: pointer;
  }
  .msg-details-toggle:hover {
    text-decoration: underline;
  }

  /* Copy button */
  .copy-btn {
    background: none;
    border: none;
    padding: 0;
    cursor: pointer;
    display: inline-flex;
  }
  .copy-btn:hover :global(.env-monotag) {
    border-color: var(--env-accent);
    color: var(--env-accent);
  }

  /* Mark read/unread button */
  .reader-action-btn {
    font-size: 0.75rem;
    font-weight: 500;
    color: var(--env-accent);
    background: none;
    border: 1px solid var(--env-accent);
    border-radius: var(--radius-xs, 2px);
    padding: 0.15rem 0.5rem;
    cursor: pointer;
    transition: background 0.1s ease, color 0.1s ease;
  }
  .reader-action-btn:hover:not(:disabled) {
    background: var(--env-accent-soft);
  }
  .reader-action-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  /* Body toolbar */
  .msg-body-toolbar {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    margin-bottom: 0.5rem;
    flex-wrap: wrap;
  }
  .body-toggle {
    display: inline-flex;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-xs, 2px);
    overflow: hidden;
  }
  .body-toggle-btn {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    background: var(--env-surface);
    color: var(--env-muted);
    border: none;
    padding: 0.2rem 0.5rem;
    cursor: pointer;
    transition: background 0.1s ease, color 0.1s ease;
  }
  .body-toggle-btn.is-active {
    background: var(--env-accent);
    color: #fff;
  }
  .body-toggle-btn:hover:not(.is-active) {
    background: var(--env-accent-soft);
    color: var(--env-accent);
  }
  .body-format-note {
    font-size: 0.6875rem;
    color: var(--env-muted);
    font-family: var(--font-mono);
  }
  .remote-img-btn {
    font-size: 0.75rem;
    color: var(--env-accent);
    background: none;
    border: 1px solid var(--env-accent);
    border-radius: var(--radius-xs, 2px);
    padding: 0.15rem 0.5rem;
    cursor: pointer;
  }
  .remote-img-btn:hover {
    background: var(--env-accent-soft);
  }

  /* Body */
  .msg-body {
    margin-bottom: 1rem;
  }
  .msg-text {
    margin: 0;
    font-family: var(--font-sans);
    font-size: 0.875rem;
    line-height: 1.55;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    color: var(--env-ink);
  }
  .msg-empty {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--env-muted);
  }
</style>
