<script lang="ts">
  // Logs — what agents and Envelope did, newest first, across every account.
  // Read-only: each row links to the draft or message it concerns. Filters
  // re-query the server; "Load more" pages with the server's cursor. Live
  // events on the SSE bus trigger a refetch of the first page.
  import { onMount } from 'svelte';
  import { base } from '$app/paths';
  import { Spinner, EmptyState, MonoTag } from '$lib/components';
  import { api, EnvelopeApiError, type Account } from '$lib/api';
  import { logsApi, type LogEntry } from '$lib/logs-api';

  const LIMIT = 100;

  /** Plain names for what each event type records. Unknown types fall back
   *  to the type with its separators spaced out — legible, never hidden. */
  const LABELS: Record<string, { label: string; tone: 'ok' | 'warn' | 'muted' }> = {
    'send_governor.blocked': { label: 'Blocked by Governor', tone: 'warn' },
    'send_governor.allowed': { label: 'Allowed by Governor', tone: 'ok' },
    'send_governor.attribution_refused': { label: 'Declaration refused', tone: 'warn' },
    send_queued: { label: 'Queued', tone: 'muted' },
    send_completed: { label: 'Sent', tone: 'ok' },
    'send.human_dashboard': { label: 'Human-only Send', tone: 'ok' },
    draft_approved: { label: 'Approved by human', tone: 'ok' },
    'send_policy.allowed': { label: 'Policy allowed', tone: 'muted' },
    'send_policy.draft_only': { label: 'Policy: draft only', tone: 'muted' },
    'credential.clipboard_handoff': { label: 'Credential to clipboard', tone: 'muted' },
    new_message: { label: 'New message', tone: 'muted' }
  };

  const TYPE_OPTIONS: { value: string; label: string }[] = [
    { value: '', label: 'All types' },
    { value: 'send_governor', label: 'Governor decisions' },
    { value: 'send_queued', label: 'Queued' },
    { value: 'send_completed', label: 'Sent' },
    { value: 'send.human_dashboard', label: 'Human-only Send' },
    { value: 'draft_approved', label: 'Approved by human' },
    { value: 'send_policy', label: 'Policy checks' },
    { value: 'credential', label: 'Credentials' },
    { value: 'new_message', label: 'New message' }
  ];

  const RANGE_OPTIONS: { value: string; label: string; hours: number | null }[] = [
    { value: '24h', label: 'Last 24 hours', hours: 24 },
    { value: '7d', label: 'Last 7 days', hours: 24 * 7 },
    { value: '30d', label: 'Last 30 days', hours: 24 * 30 },
    { value: 'all', label: 'All time', hours: null }
  ];

  let accounts = $state<Account[]>([]);
  let account = $state('');
  let type = $state('');
  let range = $state('7d');

  let entries = $state<LogEntry[]>([]);
  let nextBefore = $state<string | null>(null);
  let loading = $state(true);
  let loadingMore = $state(false);
  let error = $state<{ code: string; message: string } | null>(null);
  let generation = 0;

  function sinceFor(rangeValue: string): string | null {
    const hours = RANGE_OPTIONS.find((r) => r.value === rangeValue)?.hours ?? null;
    if (hours == null) return null;
    return new Date(Date.now() - hours * 3600 * 1000).toISOString().replace(/\.\d{3}Z$/, 'Z');
  }

  function labelFor(eventType: string): { label: string; tone: 'ok' | 'warn' | 'muted' } {
    return LABELS[eventType] ?? { label: eventType.replace(/[._]/g, ' '), tone: 'muted' };
  }

  function toError(e: unknown): { code: string; message: string } {
    if (e instanceof EnvelopeApiError) return { code: e.code, message: e.message };
    return { code: 'unknown', message: e instanceof Error ? e.message : String(e) };
  }

  /** A later page may continue the last entry's same-day run: merge by key. */
  function merge(existing: LogEntry[], incoming: LogEntry[]): LogEntry[] {
    const out = existing.slice();
    for (const entry of incoming) {
      const i = out.findIndex((e) => e.group_key === entry.group_key);
      if (i === -1) {
        out.push(entry);
      } else {
        out[i] = {
          ...out[i],
          repeat_count: out[i].repeat_count + entry.repeat_count,
          first_at: entry.first_at < out[i].first_at ? entry.first_at : out[i].first_at
        };
      }
    }
    return out;
  }

  async function load() {
    const mine = ++generation;
    loading = true;
    error = null;
    try {
      const res = await logsApi.list({ account, type, since: sinceFor(range), limit: LIMIT });
      if (mine !== generation) return;
      entries = res.entries;
      nextBefore = res.next_before;
    } catch (e) {
      if (mine !== generation) return;
      error = toError(e);
    } finally {
      if (mine === generation) loading = false;
    }
  }

  async function loadMore() {
    if (!nextBefore || loadingMore) return;
    const mine = generation;
    loadingMore = true;
    try {
      const res = await logsApi.list({
        account,
        type,
        since: sinceFor(range),
        before: nextBefore,
        limit: LIMIT
      });
      if (mine !== generation) return;
      entries = merge(entries, res.entries);
      nextBefore = res.next_before;
    } catch (e) {
      if (mine !== generation) return;
      error = toError(e);
    } finally {
      if (mine === generation) loadingMore = false;
    }
  }

  // Any filter change re-queries; the first run is the initial load. The
  // effect tracks only the three filter reads that happen before `load`
  // awaits.
  $effect(() => {
    void [account, type, range];
    void load();
  });

  onMount(() => {
    void (async () => {
      try {
        accounts = (await api.listAccounts()).accounts;
      } catch {
        // The account filter is a convenience; the log loads without it.
      }
    })();

    // Live refetch of the first page when anything happens on the bus. Guarded
    // so environments without EventSource (tests) never open a connection.
    if (typeof EventSource === 'undefined') return;
    let timer: ReturnType<typeof setTimeout> | null = null;
    let off: (() => void) | null = null;
    void import('$lib/live.svelte').then(({ getLiveStore }) => {
      off = getLiveStore().on('*', () => {
        if (timer) clearTimeout(timer);
        timer = setTimeout(() => void load(), 1000);
      });
    });
    return () => {
      if (timer) clearTimeout(timer);
      off?.();
    };
  });

  function age(iso: string): string {
    const then = Date.parse(iso.includes('Z') || iso.includes('+') ? iso : `${iso}Z`);
    if (Number.isNaN(then)) return iso;
    const secs = Math.max(0, Math.floor((Date.now() - then) / 1000));
    if (secs < 60) return `${secs}s ago`;
    const mins = Math.floor(secs / 60);
    if (mins < 60) return `${mins}m ago`;
    const hrs = Math.floor(mins / 60);
    if (hrs < 24) return `${hrs}h ago`;
    return `${Math.floor(hrs / 24)}d ago`;
  }

  /** "over 8 days" / "over 3h" for a collapsed run. */
  function span(first: string, last: string): string {
    const a = Date.parse(first.includes('Z') || first.includes('+') ? first : `${first}Z`);
    const b = Date.parse(last.includes('Z') || last.includes('+') ? last : `${last}Z`);
    if (Number.isNaN(a) || Number.isNaN(b)) return '';
    const mins = Math.max(0, Math.round((b - a) / 60000));
    if (mins < 60) return `over ${mins}m`;
    const hrs = Math.round(mins / 60);
    if (hrs < 48) return `over ${hrs}h`;
    return `over ${Math.round(hrs / 24)} days`;
  }
</script>

<div class="logs" id="logs">
  <header class="logs-header">
    <h1 class="logs-title">Logs</h1>
    <p class="logs-lede">What agents and Envelope did, newest first, across every account.</p>
  </header>

  <form class="logs-filters" onsubmit={(e) => e.preventDefault()}>
    <label class="logs-filter">
      <span>Account</span>
      <select id="logs-account" bind:value={account}>
        <option value="">All accounts</option>
        {#each accounts as acct (acct.id)}
          <option value={acct.id}>{acct.display_name || acct.name}</option>
        {/each}
      </select>
    </label>
    <label class="logs-filter">
      <span>Type</span>
      <select id="logs-type" bind:value={type}>
        {#each TYPE_OPTIONS as opt (opt.value)}
          <option value={opt.value}>{opt.label}</option>
        {/each}
      </select>
    </label>
    <label class="logs-filter">
      <span>Range</span>
      <select id="logs-range" bind:value={range}>
        {#each RANGE_OPTIONS as opt (opt.value)}
          <option value={opt.value}>{opt.label}</option>
        {/each}
      </select>
    </label>
  </form>

  {#if loading}
    <div class="logs-loading"><Spinner /></div>
  {:else if error}
    <EmptyState title="Logs unavailable" hint={`${error.message} (${error.code})`} />
  {:else if entries.length === 0}
    <EmptyState
      title="Nothing logged for this filter"
      hint="Widen the range or clear the account and type filters."
    />
  {:else}
    <ul class="log-rows">
      {#each entries as e (e.group_key)}
        {@const meta = labelFor(e.event_type)}
        <li class="log-row" class:is-warn={meta.tone === 'warn'} class:is-ok={meta.tone === 'ok'}>
          <div class="log-head">
            <span class="log-label">{meta.label}</span>
            {#if e.repeat_count > 1}
              <span class="log-repeat" title="first {e.first_at}">
                ×{e.repeat_count.toLocaleString()} {span(e.first_at, e.created_at)}
              </span>
            {/if}
            <time class="log-time" datetime={e.created_at} title={e.created_at}>{age(e.created_at)}</time>
          </div>
          {#if e.draft_link}
            <a class="log-subject" href="{base}{e.draft_link}">{e.draft_subject ?? '(no subject)'}</a>
          {:else if e.message_link}
            <a class="log-subject" href="{base}{e.message_link}">{e.subject ?? '(no subject)'}</a>
          {:else if e.subject}
            <span class="log-subject">{e.subject}</span>
          {/if}
          <p class="log-meta">
            {e.account_label}
            {#if e.surface}· {e.surface}{/if}
            {#if e.agent_id}· agent {e.agent_id}{/if}
            {#if e.from_addr}· {e.from_addr}{/if}
            {#if e.policy_mode}· {e.policy_mode}{/if}
            {#if e.denial_code}· {e.denial_code}{/if}
          </p>
          {#if e.block_reason}
            <p class="log-reason">{e.block_reason}</p>
          {/if}
          {#if e.attrs.length > 0}
            <p class="log-attrs">
              {#each e.attrs as attr (attr)}
                <span class="log-attr"><MonoTag>{attr}</MonoTag></span>
              {/each}
            </p>
          {/if}
        </li>
      {/each}
    </ul>
    {#if nextBefore}
      <button class="logs-more" type="button" disabled={loadingMore} onclick={loadMore}>
        {#if loadingMore}<Spinner label="Loading more" />{/if}
        {loadingMore ? 'Loading' : 'Load more'}
      </button>
    {/if}
  {/if}
</div>

<style>
  .logs {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 1rem 1rem 3rem;
    width: 100%;
    max-width: 44rem;
    margin: 0 auto;
  }
  .logs-header {
    margin-bottom: 1rem;
  }
  .logs-title {
    margin: 0;
    font-family: var(--font-sans);
    font-size: 1.5rem;
    font-weight: 700;
    letter-spacing: -0.02em;
    color: var(--env-ink);
  }
  .logs-lede {
    margin: 0.25rem 0 0;
    font-size: 0.9375rem;
    color: var(--env-muted);
  }
  .logs-filters {
    display: flex;
    flex-wrap: wrap;
    gap: 0.75rem;
    margin-bottom: 1rem;
  }
  .logs-filter {
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    font-family: var(--font-mono);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--env-muted);
  }
  .logs-filter select {
    font-family: var(--font-sans);
    font-size: 0.8125rem;
    color: var(--env-ink);
    background: var(--env-surface);
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-sm, 3px);
    padding: 0.3rem 0.5rem;
  }
  .logs-loading {
    display: flex;
    justify-content: center;
    padding: 4rem;
  }
  .log-rows {
    margin: 0;
    padding: 0;
    list-style: none;
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
  }
  .log-row {
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    background: var(--env-paper);
    border: 1px solid var(--env-rule);
    border-left-width: 3px;
    border-radius: var(--radius-sm, 3px);
    padding: 0.5rem 0.7rem;
  }
  .log-row.is-warn {
    border-left-color: var(--env-warn);
  }
  .log-row.is-ok {
    border-left-color: var(--env-accent);
  }
  .log-head {
    display: flex;
    align-items: baseline;
    gap: 0.6rem;
  }
  .log-label {
    font-size: 0.8125rem;
    font-weight: 600;
    color: var(--env-ink);
  }
  .log-repeat {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    color: var(--env-warn);
    font-variant-numeric: tabular-nums;
  }
  .log-time {
    margin-left: auto;
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    color: var(--env-muted);
    white-space: nowrap;
  }
  .log-subject {
    font-size: 0.8125rem;
    color: var(--env-ink);
    text-decoration: none;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  a.log-subject:hover {
    color: var(--env-accent);
  }
  .log-meta,
  .log-reason {
    margin: 0;
    font-size: 0.71875rem;
    color: var(--env-muted);
  }
  .log-reason {
    color: var(--env-ink);
  }
  .log-attrs {
    margin: 0.15rem 0 0;
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem;
  }
  .logs-more {
    margin-top: 0.85rem;
    font-size: 0.8125rem;
    color: var(--env-accent);
    background: none;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-sm, 3px);
    padding: 0.4rem 0.8rem;
    cursor: pointer;
  }
  .logs-more:disabled {
    opacity: 0.6;
    cursor: default;
  }
  @media (min-width: 641px) {
    .logs {
      padding: 1.5rem 2rem 3rem;
    }
  }
</style>
