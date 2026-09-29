<script lang="ts">
  // Sync now + sync status for a mailbox view (#171). The button starts a
  // read-only provider sync; the status line says what the last one did, in
  // words a screen reader announces (role=status, polite). "Synced" appears
  // only when a provider sync actually succeeded — the SSE "Live" dot next to
  // this is a connection state and never implies it. Failed accounts are
  // listed by name with their own Retry, and their cached mail stays visible.
  import type { ScopeSyncState } from '$lib/mailbox-sync.svelte';

  let {
    sync,
    onsync,
    onretry
  }: {
    sync: ScopeSyncState;
    onsync: () => void;
    onretry: (accountId: string) => void;
  } = $props();

  const syncing = $derived(sync.phase === 'syncing');

  function clock(ts: string | null): string | null {
    if (!ts) return null;
    const d = new Date(ts);
    if (Number.isNaN(d.getTime())) return null;
    return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  }

  function retryingName(): string | null {
    const id = sync.retryingAccount;
    if (!id) return null;
    return sync.failures.find((f) => f.account_id === id)?.account_username ?? id;
  }

  const statusText = $derived.by(() => {
    const last = clock(sync.lastSuccessAt);
    const failed = sync.failures.length;
    const accountsWord = `account${failed === 1 ? '' : 's'}`;
    switch (sync.phase) {
      case 'syncing': {
        const name = retryingName();
        return name ? `Syncing ${name}…` : 'Syncing…';
      }
      case 'synced':
        return last ? `Synced ${last}` : 'Synced';
      case 'partial':
        return `Partial sync — ${failed} ${accountsWord} not synced; showing cached mail.`;
      case 'failed':
        return `Sync failed: ${(sync.error ?? 'unknown error').replace(/\.+$/, '')}. Showing cached mail.`;
      default:
        if (failed > 0) return `${failed} ${accountsWord} stale — last sync failed.`;
        return last ? `Last synced ${last}` : 'Not synced yet';
    }
  });
</script>

<div id="sync-control" class="sync-control" class:is-failed={sync.phase === 'failed'}>
  <p id="sync-status" class="sync-status" role="status" aria-live="polite">{statusText}</p>
  {#if sync.phase === 'failed'}
    <button id="sync-retry-btn" class="sync-btn" type="button" onclick={onsync}>Retry sync</button>
  {:else}
    <button
      id="sync-now-btn"
      class="sync-btn"
      type="button"
      disabled={syncing}
      aria-busy={syncing}
      aria-describedby="sync-status"
      title="Fetch this mailbox from your mail provider (read-only)"
      onclick={onsync}
    >{syncing ? 'Syncing…' : 'Sync now'}</button>
  {/if}
</div>

{#if sync.failures.length > 0}
  <ul id="sync-failures" class="sync-failures" aria-label="Accounts that did not sync">
    {#each sync.failures as f (f.account_id)}
      <li class="sync-failure">
        <span class="sync-failure-text">
          <strong class="sync-failure-account">{f.account_username}</strong>
          <span class="sync-failure-error">{f.error}</span>
        </span>
        <button
          class="sync-failure-retry"
          type="button"
          disabled={syncing}
          aria-label="Retry sync for {f.account_username}"
          onclick={() => onretry(f.account_id)}
        >Retry</button>
      </li>
    {/each}
  </ul>
{/if}

<style>
  .sync-control {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 0.5rem;
    min-width: 0;
    flex: 1;
  }
  .sync-status {
    margin: 0;
    min-width: 0;
    font-size: 0.75rem;
    color: var(--env-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .is-failed .sync-status {
    color: var(--env-warn);
    white-space: normal;
  }
  .sync-btn {
    flex-shrink: 0;
    font: inherit;
    font-size: 0.75rem;
    font-weight: 600;
    color: var(--env-ink);
    background: var(--env-surface);
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-sm, 3px);
    padding: 0.2rem 0.6rem;
    min-height: 1.75rem;
    cursor: pointer;
  }
  .sync-btn:disabled {
    color: var(--env-muted);
    cursor: progress;
  }
  .sync-failures {
    /* Full-width row under the status line, bleeding to the bar's edges. */
    flex-basis: 100%;
    list-style: none;
    margin: 0.25rem -0.75rem -0.25rem;
    padding: 0.35rem 0.75rem;
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    font-size: 0.75rem;
    color: var(--env-pending);
    background: var(--env-pending-soft);
  }
  .sync-failure {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
  }
  .sync-failure-text {
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .sync-failure-account {
    font-weight: 600;
    margin-right: 0.35rem;
  }
  .sync-failure-retry {
    flex-shrink: 0;
    font: inherit;
    color: var(--env-accent);
    background: none;
    border: none;
    padding: 0.2rem 0.25rem;
    text-decoration: underline;
    cursor: pointer;
  }
  .sync-failure-retry:disabled {
    color: var(--env-muted);
    cursor: progress;
  }
</style>
