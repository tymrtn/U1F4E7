<script lang="ts">
  // Digest board — the GTD clarify surface and the shell's landing route
  // (design plan rev 3, §4a). The agent categorizes; the human decides in
  // bulk. Until the categorize backend lands (Phase E spike), only the
  // capture bucket is real: category sections render honest awaiting-backend
  // states and the Categorize controls are disabled with the reason on them.
  import { base } from '$app/paths';
  import { api, EnvelopeApiError, type UnifiedInboxResponse } from '$lib/api';
  import { DIGEST_SECTIONS, groupIntoThreads } from '$lib/digest';
  import { SelectionStore } from '$lib/selection.svelte';
  import { Button, EmptyState, Icon, Spinner } from '$lib/components';
  import BulkToolbar from '$lib/components/BulkToolbar.svelte';

  let resp = $state<UnifiedInboxResponse | null>(null);
  let loading = $state(true);
  let refreshing = $state(false);
  let error = $state<{ code: string; message: string } | null>(null);

  const selection = new SelectionStore();

  // The tally reports thread count (grouping is display metadata), but the
  // capture list renders one row per message and selection is 1:1 with a uid.
  // Collapsing threads into a single anchor row would make a bulk op touch
  // only the newest message and silently leave the rest — the unified inbox
  // itself is per-message, so this list matches it. Per-row thread counts
  // wait for thread-aware bulk ops.
  const threadCount = $derived(resp ? groupIntoThreads(resp.messages).length : 0);
  const rows = $derived(
    (resp?.messages ?? []).map((m) => ({
      key: `${m.account_id}:${m.uid}`,
      subject: m.subject ?? '',
      from: m.from_addr ?? '',
      date: m.date ?? null,
      unread: m.unread,
      accountId: m.account_id,
      uid: m.uid,
      folder: m.folder,
      messageId: m.message_id ?? null
    }))
  );
  const orderedKeys = $derived(rows.map((r) => r.key));
  const messageIndex = $derived(
    Object.fromEntries(
      rows.map((r) => [
        r.key,
        {
          accountId: r.accountId,
          uid: r.uid,
          from: r.from,
          folder: r.folder,
          message_id: r.messageId ?? undefined,
          subject: r.subject
        }
      ])
    )
  );

  const today = new Date().toLocaleDateString([], {
    weekday: 'short',
    day: 'numeric',
    month: 'short',
    year: 'numeric'
  });

  function fail(e: unknown) {
    const err = e as EnvelopeApiError;
    error = { code: err.code ?? 'unknown', message: err.message ?? 'Failed to load.' };
  }

  async function load() {
    loading = true;
    error = null;
    try {
      resp = await api.unifiedInbox(50);
    } catch (e) {
      fail(e);
    } finally {
      loading = false;
    }
  }

  async function refresh() {
    refreshing = true;
    error = null;
    try {
      resp = await api.refreshUnifiedInbox(50);
      selection.clear();
    } catch (e) {
      fail(e);
    } finally {
      refreshing = false;
    }
  }

  $effect(() => {
    load();
  });

  function fmtDate(iso: string | null): string {
    if (!iso) return '';
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return '';
    const now = new Date();
    return d.toDateString() === now.toDateString()
      ? d.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })
      : d.toLocaleDateString([], { month: 'short', day: 'numeric' });
  }

  const CATEGORIZE_HINT = 'Agent categorization lands with the digest backend (Phase E spike).';
</script>

<svelte:head>
  <title>Digest — Envelope</title>
</svelte:head>

<div class="digest" id="digest-board">
  <header class="dg-head">
    <h1 class="dg-title">Digest</h1>
    <span class="dg-date">{today}</span>
    <div class="dg-controls">
      <Button variant="ghost" onclick={refresh} disabled={loading || refreshing}>
        {refreshing ? 'Refreshing…' : 'Refresh'}
      </Button>
      <span title={CATEGORIZE_HINT}>
        <Button variant="ghost" disabled>Categorize</Button>
      </span>
      <a class="dg-rules" href="{base}/rules">Settings &amp; rules →</a>
    </div>
  </header>

  {#if loading}
    <div class="dg-loading"><Spinner label="Loading digest" /> <span>Loading…</span></div>
  {:else if error}
    <div class="dg-error" role="alert">
      <p class="dg-error-msg">Couldn't load the digest.</p>
      <p class="dg-error-code"><code>{error.code}</code> {error.message}</p>
      <button class="dg-retry" type="button" onclick={load}>Retry</button>
    </div>
  {:else if resp}
    <p class="dg-tally" data-testid="digest-tally">
      <b>{resp.messages.length}</b> messages · <b>{threadCount}</b> threads ·
      <b>{resp.unread_count}</b> unread
    </p>

    <section class="dg-section" data-section="capture">
      <header class="dg-sec-head tone-capture">
        <h2>Capture</h2>
        <span class="dg-sec-meta">{rows.length} messages, newest first — uncategorized</span>
      </header>
      {#if selection.count > 0}
        <BulkToolbar {selection} folder="INBOX" {messageIndex} onoperated={refresh} />
      {/if}
      {#if rows.length === 0}
        <EmptyState
          title="Nothing captured"
          hint="The unified inbox is empty. New mail lands here for processing."
        />
      {:else}
        <ul class="dg-rows">
          {#each rows as r (r.key)}
            <li class="dg-row" class:is-selected={selection.isSelected(r.key)}>
              <input
                class="dg-check"
                type="checkbox"
                checked={selection.isSelected(r.key)}
                aria-label="Select message"
                onclick={(e) => {
                  if (e.shiftKey) selection.rangeSelect(r.key, orderedKeys);
                  else selection.toggle(r.key);
                }}
              />
              {#if r.unread}
                <span class="dg-sr">Unread.</span>
                <span class="dg-unread" aria-hidden="true"></span>
              {/if}
              <a
                class="dg-link"
                href="{base}/mail/unified/{encodeURIComponent(r.accountId)}/{r.uid}?folder={encodeURIComponent(
                  r.folder
                )}"
              >
                <span class="dg-from" class:is-unread={r.unread}>{r.from || r.accountId}</span>
                <span class="dg-subject">— {r.subject || '(no subject)'}</span>
              </a>
              <span class="dg-when">{fmtDate(r.date)}</span>
            </li>
          {/each}
        </ul>
      {/if}
    </section>

    {#each DIGEST_SECTIONS as section (section.key)}
      <section class="dg-section" data-section={section.key}>
        <header class="dg-sec-head tone-{section.tone}">
          <h2>{section.label}</h2>
          <span class="dg-sec-meta">awaiting categorize backend</span>
          {#if section.bulk}
            <span class="dg-smart" title={CATEGORIZE_HINT}>
              <Icon name="archive" size={12} /> smart all
            </span>
          {/if}
        </header>
        <p class="dg-awaiting">
          The agent will file threads here once categorization is wired. Nothing is guessed
          client-side.
        </p>
      </section>
    {/each}
  {/if}
</div>

<style>
  .digest {
    max-width: 52rem;
    margin: 0 auto;
    padding: 1.5rem 1.5rem 3rem;
    overflow-y: auto;
    height: 100%;
  }
  .dg-head {
    display: flex;
    align-items: baseline;
    gap: 0.75rem;
    flex-wrap: wrap;
  }
  .dg-title {
    margin: 0;
    font-size: 1.375rem;
    font-weight: 700;
    letter-spacing: -0.01em;
  }
  .dg-date {
    font-family: var(--font-mono);
    font-size: 0.75rem;
    color: var(--env-muted);
  }
  .dg-controls {
    margin-left: auto;
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
  .dg-rules {
    font-size: 0.8125rem;
    color: var(--env-accent);
    text-decoration: none;
  }
  .dg-rules:hover {
    text-decoration: underline;
  }
  .dg-tally {
    font-family: var(--font-mono);
    font-size: 0.8125rem;
    color: var(--env-muted);
    border-bottom: 1px solid var(--env-rule);
    margin: 0.75rem 0 0;
    padding-bottom: 0.75rem;
  }
  .dg-tally b {
    color: var(--env-ink);
    font-weight: 500;
  }
  .dg-section {
    margin-top: 1.5rem;
  }
  .dg-sec-head {
    display: flex;
    align-items: baseline;
    gap: 0.6rem;
    padding-left: 0.7rem;
    border-left: 3px solid var(--env-rule);
  }
  .dg-sec-head.tone-capture {
    border-left-color: var(--env-accent);
  }
  .dg-sec-head.tone-do {
    border-left-color: var(--env-warn);
  }
  .dg-sec-head.tone-wait {
    border-left-color: var(--env-pending);
  }
  .dg-sec-head.tone-noise {
    border-left-color: var(--env-muted);
  }
  .dg-sec-head h2 {
    margin: 0;
    font-size: 0.9375rem;
    font-weight: 700;
  }
  .dg-sec-meta {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    color: var(--env-muted);
  }
  .dg-smart {
    margin-left: auto;
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    color: var(--env-muted);
    border: 1px solid var(--env-rule);
    border-radius: 999px;
    padding: 0.1rem 0.55rem;
    opacity: 0.6;
    cursor: not-allowed;
  }
  .dg-awaiting {
    margin: 0.4rem 0 0 0.95rem;
    font-size: 0.8125rem;
    color: var(--env-muted);
    font-style: italic;
  }
  .dg-rows {
    list-style: none;
    margin: 0.25rem 0 0;
    padding: 0;
  }
  .dg-row {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    padding: 0.45rem 0 0.45rem 0.95rem;
    border-bottom: 1px solid var(--env-rule-soft);
    min-height: 36px;
  }
  .dg-row.is-selected {
    background: var(--env-soft);
  }
  .dg-check {
    accent-color: var(--env-accent);
    width: 14px;
    height: 14px;
    flex-shrink: 0;
    cursor: pointer;
  }
  .dg-unread {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--env-accent);
    flex-shrink: 0;
  }
  .dg-link {
    display: flex;
    align-items: baseline;
    gap: 0.35rem;
    min-width: 0;
    flex: 1;
    text-decoration: none;
    color: var(--env-ink);
  }
  .dg-from {
    font-size: 0.8125rem;
    white-space: nowrap;
    flex-shrink: 0;
  }
  .dg-from.is-unread {
    font-weight: 700;
  }
  .dg-subject {
    font-size: 0.8125rem;
    color: var(--env-ink);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .dg-link:hover .dg-subject {
    text-decoration: underline;
  }
  .dg-sr {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
    white-space: nowrap;
    border: 0;
  }
  .dg-when {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    color: var(--env-muted);
    flex-shrink: 0;
    font-variant-numeric: tabular-nums;
  }
  .dg-loading {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 2rem 0;
    color: var(--env-muted);
    font-size: 0.875rem;
  }
  .dg-error {
    padding: 1.5rem 0;
  }
  .dg-error-msg {
    margin: 0;
    font-weight: 600;
    color: var(--env-warn);
  }
  .dg-error-code {
    margin: 0.3rem 0 0;
    font-size: 0.8125rem;
    color: var(--env-muted);
  }
  .dg-error-code code {
    font-family: var(--font-mono);
    background: var(--env-soft);
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-xs, 2px);
    padding: 0 0.3rem;
  }
  .dg-retry {
    margin-top: 0.5rem;
    font-size: 0.8125rem;
    color: var(--env-accent);
    background: none;
    border: none;
    padding: 0;
    cursor: pointer;
    text-decoration: underline;
  }
</style>
