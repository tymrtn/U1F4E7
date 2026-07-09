<script lang="ts">
  // Left rail: smart mailboxes (deep-linked client routes) + accounts with
  // per-account health badges derived from the cockpit aggregate. Clicking an
  // account row opens the contextual AccountDrawer. Every data surface here has
  // an explicit loading / error / empty state — no silent failures.
  import { base } from '$app/paths';
  import { page } from '$app/state';
  import {
    api,
    accountHealthFromCockpit,
    EnvelopeApiError,
    type Account,
    type CockpitResponse,
    type StatsResponse,
    type AccountHealth
  } from '$lib/api';
  import { MAILBOXES } from '$lib/mailboxes';
  import Badge from './Badge.svelte';
  import Spinner from './Spinner.svelte';
  import MonoTag from './MonoTag.svelte';
  import AccountDrawer from './AccountDrawer.svelte';

  let accounts = $state<Account[]>([]);
  let cockpit = $state<CockpitResponse | null>(null);
  let stats = $state<StatsResponse | null>(null);
  let loading = $state(true);
  let error = $state<{ code: string; message: string } | null>(null);

  let drawerAccount = $state<Account | null>(null);
  let drawerHealth = $state<AccountHealth>('unknown');
  let drawerOpen = $state(false);

  const activeBox = $derived(page.params.box ?? 'unified');

  async function load() {
    loading = true;
    error = null;
    try {
      // Cockpit and stats are best-effort context; a failure there must not
      // blank the whole rail. Accounts are the load-bearing fetch.
      const [accountsRes, cockpitRes, statsRes] = await Promise.allSettled([
        api.listAccounts(),
        api.cockpit(),
        api.stats()
      ]);

      if (accountsRes.status === 'rejected') throw accountsRes.reason;
      accounts = accountsRes.value.accounts;
      cockpit = cockpitRes.status === 'fulfilled' ? cockpitRes.value : null;
      stats = statsRes.status === 'fulfilled' ? statsRes.value : null;
    } catch (e) {
      const err = e as EnvelopeApiError;
      error = { code: err.code ?? 'unknown', message: err.message ?? 'Failed to load accounts.' };
    } finally {
      loading = false;
    }
  }

  $effect(() => {
    load();
  });

  function healthFor(account: Account): AccountHealth {
    return accountHealthFromCockpit(cockpit, account.id);
  }

  function openAccount(account: Account) {
    drawerAccount = account;
    drawerHealth = healthFor(account);
    drawerOpen = true;
  }

  function boxCount(slug: string): number | null {
    if (!stats) return null;
    if (slug === 'snoozed') return stats.snoozed;
    if (slug === 'drafts') return stats.drafts;
    return null;
  }
</script>

<aside class="rail" aria-label="Mailboxes">
  <p class="rail-label">Mailboxes</p>
  <ul class="rail-list">
    {#each MAILBOXES as box (box.slug)}
      {@const count = boxCount(box.slug)}
      <li>
        <a
          class="rail-item"
          class:is-active={activeBox === box.slug}
          href="{base}/mail/{box.slug}"
          aria-current={activeBox === box.slug ? 'page' : undefined}
        >
          <span class="rail-item-label">{box.label}</span>
          {#if count !== null && count > 0}
            <span class="rail-count">{count}</span>
          {/if}
        </a>
      </li>
    {/each}
  </ul>

  <p class="rail-label rail-accounts-label">Accounts</p>
  <div class="rail-accounts">
    {#if loading}
      <div class="rail-loading"><Spinner label="Loading accounts" /> <span>Loading…</span></div>
    {:else if error}
      <div class="rail-error" role="alert">
        <p class="rail-error-msg">Couldn't load accounts.</p>
        <p class="rail-error-code"><MonoTag>{error.code}</MonoTag></p>
        <button class="rail-retry" type="button" onclick={load}>Retry</button>
      </div>
    {:else if accounts.length === 0}
      <div class="rail-empty">
        <p class="rail-empty-title">No accounts yet</p>
        <p class="rail-empty-hint">Add a mailbox with <MonoTag>envelope accounts add</MonoTag>.</p>
      </div>
    {:else}
      <ul class="rail-list">
        {#each accounts as account (account.id)}
          {@const h = healthFor(account)}
          <li>
            <button class="rail-account" type="button" onclick={() => openAccount(account)}>
              <span class="rail-account-name">{account.display_name || account.name}</span>
              {#if h === 'unhealthy'}
                <Badge variant="warn">Reconnect</Badge>
              {:else if h === 'healthy'}
                <span class="rail-dot rail-dot-ok" aria-label="Connected" title="Connected"></span>
              {/if}
            </button>
          </li>
        {/each}
      </ul>
    {/if}
  </div>
</aside>

<AccountDrawer
  account={drawerAccount}
  health={drawerHealth}
  open={drawerOpen}
  onclose={() => (drawerOpen = false)}
  onchanged={load}
/>

<style>
  .rail {
    border-right: 1px solid var(--env-rule);
    padding: 0.85rem 0.75rem;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    background: var(--env-paper);
    overflow-y: auto;
  }
  .rail-label {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    text-transform: uppercase;
    letter-spacing: 0.12em;
    color: var(--env-muted);
    margin: 0 0 0.35rem;
  }
  .rail-accounts-label {
    margin-top: 1rem;
  }
  .rail-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
  }
  .rail-item {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    padding: 0 0.5rem;
    height: 34px;
    font-size: 0.8125rem;
    border-radius: var(--radius-sm, 3px);
    color: var(--env-ink);
    text-decoration: none;
  }
  .rail-item:hover {
    background: var(--env-accent-soft);
  }
  .rail-item.is-active {
    background: var(--env-accent-soft);
    color: var(--env-accent);
    font-weight: 600;
  }
  .rail-count {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    color: var(--env-muted);
    background: var(--env-surface);
    border: 1px solid var(--env-rule);
    border-radius: 999px;
    padding: 0 0.4rem;
    min-width: 1.25rem;
    text-align: center;
  }
  .rail-account {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    width: 100%;
    padding: 0 0.5rem;
    height: 34px;
    font-size: 0.8125rem;
    border: none;
    background: none;
    border-radius: var(--radius-sm, 3px);
    color: var(--env-ink);
    cursor: pointer;
    text-align: left;
  }
  .rail-account:hover {
    background: var(--env-accent-soft);
  }
  .rail-account-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .rail-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .rail-dot-ok {
    background: var(--env-accent);
  }
  .rail-loading {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.8125rem;
    color: var(--env-muted);
    padding: 0.35rem 0.5rem;
  }
  .rail-error {
    padding: 0.5rem;
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
  }
  .rail-error-msg {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--env-warn);
  }
  .rail-error-code {
    margin: 0;
  }
  .rail-retry {
    align-self: flex-start;
    font-size: 0.75rem;
    color: var(--env-accent);
    background: none;
    border: none;
    padding: 0;
    cursor: pointer;
    text-decoration: underline;
  }
  .rail-empty {
    padding: 0.5rem;
  }
  .rail-empty-title {
    margin: 0 0 0.2rem;
    font-size: 0.8125rem;
    font-weight: 600;
  }
  .rail-empty-hint {
    margin: 0;
    font-size: 0.75rem;
    color: var(--env-muted);
    line-height: 1.4;
  }
</style>
