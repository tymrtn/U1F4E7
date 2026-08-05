<script lang="ts">
  // DraftThread — the conversation a draft is replying into, shown above the
  // composer the way Gmail stacks a thread above its reply box.
  //
  // Prior messages are collapsed to sender / date / snippet and expand in
  // place; the message the draft actually answers starts open, because that is
  // the one the operator needs to read before approving the reply.
  //
  // Read-only by construction: it fetches the thread index and, on expand, the
  // message body via BODY.PEEK — opening a message here never marks it \Seen
  // and never mutates the mailbox.

  import BodyFrame from './BodyFrame.svelte';
  import Spinner from './Spinner.svelte';
  import {
    fetchMessageDetail,
    fetchThread,
    normalizeMessageId,
    type MessageDetailFull,
    type ThreadMessage
  } from '$lib/reader-api';

  interface Props {
    /** Account the draft belongs to. */
    accountId: string;
    /** The draft's `In-Reply-To` — the parent message, bracketed or bare. */
    inReplyTo: string | null | undefined;
  }

  let { accountId, inReplyTo }: Props = $props();

  let messages = $state<ThreadMessage[]>([]);
  let loading = $state(false);
  let loadFailed = $state(false);

  /** Row ids the operator has open, plus the auto-opened parent. */
  let expanded = $state<Set<number>>(new Set());
  /** Lazily fetched bodies, keyed by thread-message id. */
  let bodies = $state<Map<number, MessageDetailFull>>(new Map());
  let bodyLoading = $state<Set<number>>(new Set());
  let bodyErrors = $state<Set<number>>(new Set());

  // Every async completion proves it still belongs to the current target
  // before touching state — the composer re-targets on route change.
  let generation = 0;

  const parentId = $derived(inReplyTo ? normalizeMessageId(inReplyTo) : '');

  /** Oldest first, matching how a conversation reads. */
  const ordered = $derived(
    [...messages].sort((a, b) => (a.date ?? '').localeCompare(b.date ?? ''))
  );

  $effect(() => {
    const target = parentId;
    const account = accountId;
    const gen = ++generation;

    messages = [];
    expanded = new Set();
    bodies = new Map();
    bodyLoading = new Set();
    bodyErrors = new Set();
    loadFailed = false;

    if (!target || !account) {
      loading = false;
      return;
    }

    loading = true;
    void (async () => {
      try {
        const thread = await fetchThread(account, target);
        if (gen !== generation) return;
        messages = thread?.messages ?? [];
        // Open the message being replied to, the way Gmail opens the newest.
        const parent = messages.find((m) => normalizeMessageId(m.message_id ?? '') === target);
        if (parent) {
          expanded = new Set([parent.id]);
          void loadBody(parent, gen);
        }
      } catch {
        if (gen !== generation) return;
        loadFailed = true;
      } finally {
        if (gen === generation) loading = false;
      }
    })();
  });

  async function loadBody(msg: ThreadMessage, gen: number) {
    if (bodies.has(msg.id) || bodyLoading.has(msg.id)) return;
    bodyLoading = new Set(bodyLoading).add(msg.id);
    try {
      const res = await fetchMessageDetail(accountId, msg.uid, msg.folder || 'INBOX');
      if (gen !== generation) return;
      bodies = new Map(bodies).set(msg.id, res.message);
      const errs = new Set(bodyErrors);
      errs.delete(msg.id);
      bodyErrors = errs;
    } catch {
      if (gen !== generation) return;
      bodyErrors = new Set(bodyErrors).add(msg.id);
    } finally {
      if (gen === generation) {
        const next = new Set(bodyLoading);
        next.delete(msg.id);
        bodyLoading = next;
      }
    }
  }

  function toggle(msg: ThreadMessage) {
    const next = new Set(expanded);
    if (next.has(msg.id)) {
      next.delete(msg.id);
    } else {
      next.add(msg.id);
      void loadBody(msg, generation);
    }
    expanded = next;
  }

  /** Display name for a sender: the name part, else the local part. */
  function senderLabel(msg: ThreadMessage): string {
    if (msg.is_outbound) return 'me';
    const addr = msg.from_address;
    if (!addr) return 'unknown';
    const named = addr.match(/^(.+?)\s*<[^>]+>$/);
    if (named) return named[1].trim().replace(/^"|"$/g, '');
    return addr;
  }

  function fmtShort(iso: string | null): string {
    if (!iso) return '';
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return '';
    return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
  }

  function fmtFull(iso: string | null): string {
    if (!iso) return '';
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return '';
    return d.toLocaleString(undefined, {
      month: 'short',
      day: 'numeric',
      year: 'numeric',
      hour: 'numeric',
      minute: '2-digit'
    });
  }

  function readerHref(msg: ThreadMessage): string {
    const q = msg.folder && msg.folder !== 'INBOX' ? `?folder=${encodeURIComponent(msg.folder)}` : '';
    return `/mail/unified/${encodeURIComponent(accountId)}/${msg.uid}${q}`;
  }
</script>

{#if loading || ordered.length > 0 || loadFailed}
  <section class="draft-thread" id="draft-thread" aria-label="Conversation">
    <header class="thread-head">
      <span class="thread-eyebrow">Conversation</span>
      {#if ordered.length > 0}
        <span class="thread-count"
          >{ordered.length} message{ordered.length === 1 ? '' : 's'}</span
        >
      {/if}
    </header>

    {#if loading}
      <div class="thread-loading"><Spinner label="Loading conversation" /></div>
    {:else if loadFailed}
      <p class="thread-failed" role="status">
        The earlier messages in this conversation could not be loaded. The draft below is
        unaffected.
      </p>
    {:else}
      <ol class="thread-list">
        {#each ordered as msg (msg.id)}
          {@const open = expanded.has(msg.id)}
          {@const isParent = normalizeMessageId(msg.message_id ?? '') === parentId}
          {@const body = bodies.get(msg.id)}
          <li class="thread-item" class:is-open={open} class:is-parent={isParent}>
            <button
              type="button"
              class="thread-row"
              aria-expanded={open}
              onclick={() => toggle(msg)}
            >
              <span class="row-sender" class:is-outbound={msg.is_outbound}>
                {senderLabel(msg)}
              </span>
              {#if !open && msg.snippet}
                <span class="row-snippet">{msg.snippet}</span>
              {/if}
              <span class="row-date">{open ? fmtFull(msg.date) : fmtShort(msg.date)}</span>
              <span class="row-chevron" aria-hidden="true">{open ? '▾' : '▸'}</span>
            </button>

            {#if open}
              <div class="thread-body">
                {#if bodyLoading.has(msg.id)}
                  <Spinner label="Loading message" />
                {:else if bodyErrors.has(msg.id)}
                  <p class="body-error" role="status">
                    This message could not be fetched from the mail server.
                  </p>
                {:else if body}
                  <div class="thread-body-scroll">
                    {#if body.html_body}
                      <BodyFrame html={body.html_body} />
                    {:else if body.text_body}
                      <pre class="body-text">{body.text_body}</pre>
                    {:else}
                      <p class="body-empty">This message has no readable body.</p>
                    {/if}
                  </div>
                {/if}
                <div class="body-actions">
                  <a class="body-open" href={readerHref(msg)}>Open in reader</a>
                </div>
              </div>
            {/if}
          </li>
        {/each}
      </ol>

      <p class="thread-note">
        Reading here never marks messages as read. Your reply is below.
      </p>
    {/if}
  </section>
{/if}

<style>
  .draft-thread {
    margin-bottom: 1rem;
  }
  .thread-head {
    display: flex;
    align-items: baseline;
    gap: 0.5rem;
    margin-bottom: 0.5rem;
  }
  .thread-eyebrow {
    font-family: var(--font-mono);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--env-muted);
  }
  .thread-count {
    font-size: 0.6875rem;
    color: var(--env-muted);
  }
  .thread-loading,
  .thread-failed {
    padding: 0.75rem;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-xs, 2px);
    background: var(--env-surface);
    font-size: 0.8125rem;
    color: var(--env-muted);
  }
  .thread-list {
    list-style: none;
    margin: 0;
    padding: 0;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-xs, 2px);
    background: var(--env-surface);
    overflow: hidden;
  }
  .thread-item + .thread-item {
    border-top: 1px solid var(--env-rule);
  }
  .thread-item.is-parent {
    box-shadow: inset 2px 0 0 var(--env-accent);
  }

  .thread-row {
    display: flex;
    align-items: baseline;
    gap: 0.6rem;
    width: 100%;
    padding: 0.6rem 0.75rem;
    background: none;
    border: 0;
    text-align: left;
    cursor: pointer;
    font: inherit;
    color: var(--env-ink);
  }
  .thread-row:hover {
    background: var(--env-accent-soft);
  }
  .thread-row:focus-visible {
    outline: 2px solid var(--env-accent);
    outline-offset: -2px;
  }
  .row-sender {
    font-weight: 600;
    font-size: 0.8125rem;
    white-space: nowrap;
    flex-shrink: 0;
  }
  .row-sender.is-outbound {
    font-weight: 400;
    color: var(--env-muted);
  }
  .row-snippet {
    flex: 1;
    min-width: 0;
    font-size: 0.8125rem;
    color: var(--env-muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .row-date {
    margin-left: auto;
    font-size: 0.6875rem;
    color: var(--env-muted);
    white-space: nowrap;
    flex-shrink: 0;
  }
  .row-chevron {
    font-size: 0.625rem;
    color: var(--env-muted);
    flex-shrink: 0;
  }

  .thread-body {
    padding: 0 0.75rem 0.75rem;
    border-top: 1px solid var(--env-rule);
  }
  /* Context, not the task. A long message (quoted chains run to thousands of
     pixels) must not push the reply composer off the bottom of the page, so
     the body scrolls within a bounded box instead of expanding without limit. */
  .thread-body-scroll {
    max-height: 22rem;
    overflow-y: auto;
    overscroll-behavior: contain;
  }
  .body-text {
    margin: 0.75rem 0 0;
    font-family: var(--font-mono);
    font-size: 0.8125rem;
    line-height: 1.5;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    color: var(--env-ink);
  }
  .body-error,
  .body-empty {
    margin: 0.75rem 0 0;
    font-size: 0.8125rem;
    color: var(--env-muted);
  }
  .body-actions {
    margin-top: 0.75rem;
  }
  .body-open {
    font-size: 0.6875rem;
    color: var(--env-accent);
  }

  .thread-note {
    margin: 0.5rem 0 0;
    font-size: 0.6875rem;
    color: var(--env-muted);
  }
</style>
