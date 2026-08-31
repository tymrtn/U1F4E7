<script lang="ts">
  // Left rail — the dark instrument frame (design plan rev 3). GTD stages as
  // places (Process / Working), review surfaces (Cockpit, Approvals), then
  // accounts with per-account health and identity hue. Clicking an account row
  // opens the contextual AccountDrawer. Every data surface here has an
  // explicit loading / error / empty state — no silent failures.
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
  import { cockpitApi } from '$lib/cockpit-api';
  import { identityColor } from '$lib/hue';
  import Badge from './Badge.svelte';
  import Icon from './Icon.svelte';
  import Spinner from './Spinner.svelte';
  import MonoTag from './MonoTag.svelte';
  import AccountDrawer from './AccountDrawer.svelte';

  let accounts = $state<Account[]>([]);
  let cockpit = $state<CockpitResponse | null>(null);
  let stats = $state<StatsResponse | null>(null);
  // Actionable count for the Approvals place — drafts an agent left for a human
  // (design plan rev 3, D). Best-effort: a failed fetch just hides the badge.
  let awaitingApproval = $state(0);
  let loading = $state(true);
  let error = $state<{ code: string; message: string } | null>(null);

  // The account that owns the message open in the reader, so the rail can say
  // which mailbox the thing you are reading actually came from. The unified
  // list mixes every account together, so without this the rail highlights
  // "Inbox" and nothing else — the message's real home is invisible.
  let { activeAccountId = null }: { activeAccountId?: string | null } = $props();

  let drawerAccount = $state<Account | null>(null);
  let drawerHealth = $state<AccountHealth>('unknown');
  let drawerOpen = $state(false);

  const activeBox = $derived(page.params.box ?? 'unified');
  const onCockpit = $derived(page.url.pathname.startsWith(`${base}/cockpit`));
  const onDigest = $derived(page.url.pathname.startsWith(`${base}/digest`));
  const boxRoutesActive = $derived(!onCockpit && !onDigest);

  const processBoxes = MAILBOXES.filter((b) => b.group === 'process');
  const workingBoxes = MAILBOXES.filter((b) => b.group === 'working');

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

  // Approvals badge is pure decoration on the rail and must never gate the
  // load-bearing accounts render, so it runs on its own and swallows failures.
  async function loadApprovals() {
    try {
      const res = await cockpitApi.agents();
      awaitingApproval = res.summary.awaiting_approval;
    } catch {
      awaitingApproval = 0;
    }
  }

  $effect(() => {
    load();
    loadApprovals();
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

{#snippet boxRow(box: (typeof MAILBOXES)[number])}
  {@const count = boxCount(box.slug)}
  <li>
    <a
      class="rail-item"
      class:is-active={activeBox === box.slug && boxRoutesActive}
      href="{base}/mail/{box.slug}"
      aria-current={activeBox === box.slug && boxRoutesActive ? 'page' : undefined}
    >
      <span class="rail-item-main">
        <Icon name={box.icon} size={15} />
        <span class="rail-item-label">{box.label}</span>
      </span>
      {#if count !== null && count > 0}
        <span class="rail-count">{count}</span>
      {/if}
    </a>
  </li>
{/snippet}

<aside class="rail" aria-label="Mailboxes">
  <p class="rail-label">Process</p>
  <ul class="rail-list">
    <li>
      <a
        class="rail-item"
        class:is-active={onDigest}
        href="{base}/digest"
        aria-current={onDigest ? 'page' : undefined}
      >
        <span class="rail-item-main">
          <Icon name="list-checks" size={15} />
          <span class="rail-item-label">Digest</span>
        </span>
      </a>
    </li>
    {#each processBoxes as box (box.slug)}
      {@render boxRow(box)}
    {/each}
  </ul>

  <p class="rail-label rail-gap">Working</p>
  <ul class="rail-list">
    {#each workingBoxes as box (box.slug)}
      {@render boxRow(box)}
    {/each}
  </ul>

  <p class="rail-label rail-gap">Review</p>
  <ul class="rail-list">
    <li>
      <a
        class="rail-item"
        class:is-active={onCockpit}
        href="{base}/cockpit"
        aria-current={onCockpit ? 'page' : undefined}
      >
        <span class="rail-item-main">
          <Icon name="bot" size={15} />
          <span class="rail-item-label">Cockpit</span>
        </span>
      </a>
    </li>
    <li>
      <a class="rail-item" href="{base}/cockpit#approvals">
        <span class="rail-item-main">
          <Icon name="shield-check" size={15} />
          <span class="rail-item-label">Approvals</span>
        </span>
        {#if awaitingApproval > 0}
          <span class="rail-badge-warn" aria-label="{awaitingApproval} awaiting approval"
            >{awaitingApproval}</span
          >
        {/if}
      </a>
    </li>
  </ul>

  <p class="rail-label rail-gap">Accounts</p>
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
          {@const owns = activeAccountId === account.id}
          <li>
            <button
              class="rail-account"
              class:is-active={owns}
              type="button"
              data-account-id={account.id}
              aria-current={owns ? 'true' : undefined}
              onclick={() => openAccount(account)}
            >
              <span
                class="rail-account-tick"
                style="background: {identityColor(account.id)}"
                aria-hidden="true"
              ></span>
              <span class="rail-account-name">{account.display_name || account.name}</span>
              {#if owns}
                <span class="rail-account-here" title="The open message is in this mailbox"
                  >open here</span
                >
              {/if}
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
    border-right: 1px solid var(--env-rail-ground);
    padding: 0.85rem 0.6rem;
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    background: var(--env-rail-ground);
    color: var(--env-rail-text);
    overflow-y: auto;
  }
  .rail-label {
    font-family: var(--font-mono);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.14em;
    color: var(--env-rail-muted);
    margin: 0 0 0.3rem;
    padding: 0 0.5rem;
  }
  .rail-gap {
    margin-top: 1.1rem;
  }
  .rail-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
  }
  .rail-item {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    padding: 0 0.5rem;
    height: 32px;
    font-size: 0.8125rem;
    border-radius: var(--radius-sm, 3px);
    color: var(--env-rail-text);
    text-decoration: none;
  }
  .rail-item-main {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    min-width: 0;
  }
  .rail-item-main :global(.icon) {
    color: var(--env-rail-muted);
  }
  .rail-item:hover {
    background: var(--env-rail-hover);
  }
  /* Active place: brighter text on a faint lift with an accent edge — the
     rev-3 active language; no green wash. */
  .rail-item.is-active {
    background: var(--env-rail-lift);
    color: var(--env-rail-active-text);
    font-weight: 600;
    box-shadow: inset 2px 0 0 var(--env-rail-accent);
  }
  .rail-item.is-active :global(.icon) {
    color: var(--env-rail-accent);
  }
  .rail-item-label {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  /* Quiet numerals — dim, mono, unboxed (A4). */
  .rail-count {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    color: var(--env-rail-muted);
    flex-shrink: 0;
    font-variant-numeric: tabular-nums;
  }
  .rail-item.is-active .rail-count {
    color: var(--env-rail-text);
  }
  /* Actionable badge — the only lit count in the rail (A4): a human is owed
     something. Warn-colored pill, legible on the dark ground. */
  .rail-badge-warn {
    font-family: var(--font-mono);
    font-size: 0.625rem;
    font-weight: 500;
    color: #2b120a;
    background: var(--env-rail-warn);
    border-radius: 999px;
    padding: 0 0.4rem;
    min-width: 1.15rem;
    text-align: center;
    flex-shrink: 0;
    font-variant-numeric: tabular-nums;
  }
  .rail-account {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    width: 100%;
    padding: 0 0.5rem;
    height: 32px;
    font-size: 0.8125rem;
    border: none;
    background: none;
    border-radius: var(--radius-sm, 3px);
    color: var(--env-rail-text);
    cursor: pointer;
    text-align: left;
  }
  .rail-account:hover {
    background: var(--env-rail-hover);
  }
  /* The mailbox the open message belongs to. Mirrors .rail-item.is-active so
     the rail reads as one selection model. */
  .rail-account.is-active {
    background: var(--env-rail-lift);
    color: var(--env-rail-active-text);
    font-weight: 600;
    box-shadow: inset 2px 0 0 var(--env-rail-accent);
  }
  /* Identity hue as metadata (A3): a small tick, never a row wash. */
  .rail-account-tick {
    width: 3px;
    height: 14px;
    border-radius: 2px;
    flex-shrink: 0;
  }
  .rail-account-here {
    font-family: var(--font-mono);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--env-rail-accent);
    flex-shrink: 0;
    margin-left: auto;
  }
  .rail-account-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .rail-account :global(.env-badge) {
    margin-left: auto;
  }
  .rail-account-here + :global(.env-badge) {
    margin-left: 0;
  }
  .rail-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    flex-shrink: 0;
    margin-left: auto;
  }
  .rail-account-here ~ .rail-dot {
    margin-left: 0;
  }
  .rail-dot-ok {
    background: var(--env-rail-accent);
  }
  .rail-loading {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.8125rem;
    color: var(--env-rail-muted);
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
    color: var(--env-rail-warn);
  }
  .rail-error-code {
    margin: 0;
  }
  .rail-retry {
    align-self: flex-start;
    font-size: 0.75rem;
    color: var(--env-rail-accent);
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
    color: var(--env-rail-text);
  }
  .rail-empty-hint {
    margin: 0;
    font-size: 0.75rem;
    color: var(--env-rail-muted);
    line-height: 1.4;
  }
</style>
