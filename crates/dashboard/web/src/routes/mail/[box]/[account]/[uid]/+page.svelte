<script lang="ts">
  // Reader pane: loads one message into headers + first-pass text body.
  //
  // SAFETY: renders text only. HTML bodies are shown as an escaped notice, NOT
  // injected — {@html}/innerHTML of message content is banned until the
  // sandboxing wave. The GET fetches with BODY.PEEK[], so opening a message
  // never sets \Seen (reading does not mark read).
  import { page } from '$app/state';
  import { api, EnvelopeApiError, type MessageDetail } from '$lib/api';
  import { Spinner, MonoTag, Badge } from '$lib/components';

  const accountId = $derived(page.params.account ?? '');
  const uid = $derived(Number(page.params.uid ?? 0));
  // Unified inbox messages live in INBOX; allow an explicit ?folder= override.
  const folder = $derived(page.url.searchParams.get('folder') ?? 'INBOX');

  let message = $state<MessageDetail | null>(null);
  let loading = $state(false);
  let error = $state<{ code: string; message: string } | null>(null);
  let loadKey = $state('');

  async function load(acct: string, u: number, f: string) {
    loading = true;
    error = null;
    message = null;
    try {
      const res = await api.message(acct, u, f);
      message = res.message;
    } catch (e) {
      const err = e as EnvelopeApiError;
      error = { code: err.code ?? 'unknown', message: err.message ?? 'Failed to load message.' };
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

  function fmtDate(iso: string | null): string {
    if (!iso) return '';
    const d = new Date(iso);
    return Number.isNaN(d.getTime()) ? iso : d.toLocaleString();
  }
</script>

<div class="reader-pane">
  {#if loading}
    <div class="reader-loading"><Spinner label="Loading message" /> <span>Loading message…</span></div>
  {:else if error}
    <div class="reader-error" role="alert">
      <p class="reader-error-msg">Couldn't load this message.</p>
      <p class="reader-error-detail">{error.message}</p>
      <p><MonoTag>{error.code}</MonoTag></p>
      <button class="reader-retry" type="button" onclick={() => load(accountId, uid, folder)}>
        Retry
      </button>
    </div>
  {:else if message}
    <article class="msg">
      <header class="msg-head">
        <h1 class="msg-subject">{message.subject || '(no subject)'}</h1>
        {#if message.unread}<Badge variant="warn">Unread</Badge>{/if}
      </header>
      <dl class="msg-meta">
        <dt>From</dt>
        <dd>{message.from_addr}</dd>
        <dt>To</dt>
        <dd>{message.to_addrs?.length ? message.to_addrs.join(', ') : message.to_addr}</dd>
        {#if message.date}
          <dt>Date</dt>
          <dd>{fmtDate(message.date)}</dd>
        {/if}
        <dt>UID</dt>
        <dd><MonoTag>uid {message.uid}</MonoTag></dd>
        {#if message.message_id}
          <dt>Message-ID</dt>
          <dd><MonoTag>{message.message_id}</MonoTag></dd>
        {/if}
      </dl>

      <div class="msg-body">
        {#if message.text_body}
          <pre class="msg-text">{message.text_body}</pre>
        {:else if message.html_body}
          <p class="msg-html-notice">
            This message has an HTML body only. Rendering (sandboxed) lands next
            wave — plain text isn't available for this message.
          </p>
        {:else}
          <p class="msg-empty">This message has no readable body.</p>
        {/if}
      </div>
    </article>
  {/if}
</div>

<style>
  .reader-pane {
    padding: 1.25rem 1.5rem;
    max-width: 44rem;
    width: 100%;
  }
  .reader-loading {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.8125rem;
    color: var(--env-muted);
  }
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
    font-size: 0.8125rem;
    color: var(--env-accent);
    background: none;
    border: none;
    padding: 0;
    cursor: pointer;
    text-decoration: underline;
  }
  .msg-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 0.75rem;
    margin-bottom: 0.75rem;
  }
  .msg-subject {
    margin: 0;
    font-size: 1.0625rem;
    font-weight: 600;
    line-height: 1.3;
  }
  .msg-meta {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 0.25rem 0.85rem;
    margin: 0 0 1rem;
    padding-bottom: 1rem;
    border-bottom: 1px solid var(--env-rule);
  }
  .msg-meta dt {
    font-family: var(--font-mono);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--env-muted);
    padding-top: 0.1rem;
  }
  .msg-meta dd {
    margin: 0;
    font-size: 0.8125rem;
    overflow-wrap: anywhere;
  }
  .msg-text {
    margin: 0;
    font-family: var(--font-sans);
    font-size: 0.875rem;
    line-height: 1.55;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .msg-html-notice,
  .msg-empty {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--env-muted);
    line-height: 1.5;
  }
</style>
