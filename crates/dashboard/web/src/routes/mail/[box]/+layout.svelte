<script lang="ts">
  // A mailbox view: rail + message list + reader outlet. The list lives here so
  // it stays mounted while the reader (a nested [account]/[uid] route) swaps in
  // the third column. Only the Unified Inbox is wired to real data this wave;
  // other boxes render an explicit "not yet wired" state — no fake rows.
  import { base } from '$app/paths';
  import { page } from '$app/state';
  import type { Snippet } from 'svelte';
  import { Rail, Spinner, EmptyState, MonoTag } from '$lib/components';
  import { mailboxBySlug } from '$lib/mailboxes';
  import {
    api,
    EnvelopeApiError,
    type UnifiedInboxMessage,
    type UnifiedInboxError
  } from '$lib/api';

  let { children }: { children: Snippet } = $props();

  const box = $derived(mailboxBySlug(page.params.box ?? 'unified'));
  const selectedUid = $derived(page.params.uid ? Number(page.params.uid) : null);
  const selectedAccount = $derived(page.params.account ?? null);

  let messages = $state<UnifiedInboxMessage[]>([]);
  let listErrors = $state<UnifiedInboxError[]>([]);
  let loading = $state(false);
  let error = $state<{ code: string; message: string } | null>(null);
  let loadedBox = $state<string | null>(null);

  async function loadUnified() {
    loading = true;
    error = null;
    try {
      const res = await api.unifiedInbox(50);
      messages = res.messages;
      listErrors = res.errors ?? [];
    } catch (e) {
      const err = e as EnvelopeApiError;
      error = { code: err.code ?? 'unknown', message: err.message ?? 'Failed to load messages.' };
    } finally {
      loading = false;
    }
  }

  // Load once per box entry into a wired mailbox.
  $effect(() => {
    const slug = page.params.box ?? 'unified';
    if (box?.wired && loadedBox !== slug) {
      loadedBox = slug;
      loadUnified();
    }
  });

  function fmtDate(iso: string | null): string {
    if (!iso) return '';
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return '';
    const now = new Date();
    const sameDay = d.toDateString() === now.toDateString();
    return sameDay
      ? d.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })
      : d.toLocaleDateString([], { month: 'short', day: 'numeric' });
  }

  function senderLabel(m: UnifiedInboxMessage): string {
    return m.from_addr || m.account_username;
  }
</script>

<div class="mail-shell">
  <Rail />

  <section class="list" aria-label="Message list">
    <header class="pane-head">
      <span class="pane-title">{box?.label ?? 'Mailbox'}</span>
      {#if box?.wired && messages.length > 0}
        <span class="pane-count"><MonoTag>{messages.length}</MonoTag></span>
      {/if}
    </header>

    {#if !box}
      <EmptyState title="Unknown mailbox" hint="This mailbox slug isn't recognized." />
    {:else if !box.wired}
      <EmptyState
        title="{box.label} has no messages to show here"
        hint="This smart mailbox doesn't load its own list yet. Unified Inbox has your mail."
      >
        {#snippet action()}
          <a class="empty-link" href="{base}/mail/unified">Go to Unified Inbox</a>
        {/snippet}
      </EmptyState>
    {:else if loading}
      <div class="list-loading"><Spinner label="Loading messages" /> <span>Loading messages…</span></div>
    {:else if error}
      <div class="list-error" role="alert">
        <p class="list-error-msg">Couldn't load messages.</p>
        <p class="list-error-detail">{error.message}</p>
        <p><MonoTag>{error.code}</MonoTag></p>
        <button class="list-retry" type="button" onclick={loadUnified}>Retry</button>
      </div>
    {:else if messages.length === 0}
      <EmptyState
        title="Inbox is empty"
        hint="No messages across your connected accounts. New mail appears here."
      />
    {:else}
      {#if listErrors.length > 0}
        <p class="list-partial" role="status">
          {listErrors.length} account(s) couldn't be reached; showing what loaded.
        </p>
      {/if}
      <ul class="msg-list">
        {#each messages as m (m.account_id + ':' + m.uid)}
          {@const active = selectedUid === m.uid && selectedAccount === m.account_id}
          <li>
            <a
              class="msg-row"
              class:is-unread={m.unread}
              class:is-active={active}
              href="{base}/mail/unified/{encodeURIComponent(m.account_id)}/{m.uid}"
            >
              <div class="msg-line1">
                <span class="msg-sender">{senderLabel(m)}</span>
                <span class="msg-date">{fmtDate(m.date)}</span>
              </div>
              <div class="msg-line2">
                <span class="msg-subject">{m.subject || '(no subject)'}</span>
                <span class="msg-chip" title={m.account_username}>{m.account_display_name || m.account_username}</span>
              </div>
              {#if m.snippet}
                <p class="msg-snippet">{m.snippet}</p>
              {/if}
            </a>
          </li>
        {/each}
      </ul>
    {/if}
  </section>

  <section class="reader" aria-label="Reader">
    {@render children()}
  </section>
</div>

<style>
  .mail-shell {
    flex: 1;
    min-height: 0;
    display: grid;
    grid-template-columns: 240px minmax(320px, 1fr) minmax(360px, 1.4fr);
  }
  .list {
    border-right: 1px solid var(--env-rule);
    display: flex;
    flex-direction: column;
    min-height: 0;
    overflow-y: auto;
  }
  .reader {
    display: flex;
    flex-direction: column;
    min-height: 0;
    overflow-y: auto;
    background: var(--env-surface);
  }
  .pane-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0.75rem 1rem 0.5rem;
    position: sticky;
    top: 0;
    background: var(--env-paper);
    z-index: 1;
  }
  .pane-title {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    text-transform: uppercase;
    letter-spacing: 0.12em;
    color: var(--env-muted);
  }
  .list-loading {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 1rem;
    font-size: 0.8125rem;
    color: var(--env-muted);
  }
  .list-error {
    padding: 1rem;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }
  .list-error-msg {
    margin: 0;
    font-weight: 600;
    color: var(--env-warn);
  }
  .list-error-detail {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--env-muted);
  }
  .list-retry {
    align-self: flex-start;
    font-size: 0.8125rem;
    color: var(--env-accent);
    background: none;
    border: none;
    padding: 0;
    cursor: pointer;
    text-decoration: underline;
  }
  .list-partial {
    margin: 0;
    padding: 0.4rem 1rem;
    font-size: 0.75rem;
    color: var(--env-pending);
    background: var(--env-pending-soft);
  }
  .empty-link {
    font-size: 0.8125rem;
    color: var(--env-accent);
  }
  .msg-list {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .msg-row {
    display: block;
    min-height: 44px;
    padding: 0.5rem 1rem;
    border-bottom: 1px solid var(--env-rule);
    text-decoration: none;
    color: var(--env-ink);
  }
  .msg-row:hover {
    background: var(--env-accent-soft);
  }
  .msg-row.is-active {
    background: var(--env-accent-soft);
    box-shadow: inset 3px 0 0 var(--env-accent);
  }
  .msg-line1 {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 0.5rem;
  }
  .msg-sender {
    font-size: 0.8125rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .is-unread .msg-sender,
  .is-unread .msg-subject {
    font-weight: 700;
  }
  .msg-date {
    font-size: 0.6875rem;
    color: var(--env-muted);
    flex-shrink: 0;
    font-family: var(--font-mono);
  }
  .msg-line2 {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 0.5rem;
    margin-top: 0.1rem;
  }
  .msg-subject {
    font-size: 0.8125rem;
    color: var(--env-ink);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .msg-chip {
    font-family: var(--font-mono);
    font-size: 0.625rem;
    color: var(--env-muted);
    background: var(--env-surface);
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-xs, 2px);
    padding: 0 0.3rem;
    flex-shrink: 0;
    max-width: 40%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .msg-snippet {
    margin: 0.2rem 0 0;
    font-size: 0.75rem;
    color: var(--env-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
