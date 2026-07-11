<script lang="ts">
  // Watch + delivery health panel. Left: watch registry rows (folder + run
  // status). Right: event routes with delivery counts + a dead-letter badge.
  // Fully read-only.
  import Badge from './Badge.svelte';
  import MonoTag from './MonoTag.svelte';
  import EmptyState from './EmptyState.svelte';
  import type { WatchesResponse, HealthBucket } from '$lib/cockpit-api';

  let {
    data,
    age
  }: {
    data: WatchesResponse;
    age: (iso: string | null) => string;
  } = $props();

  function variant(health: HealthBucket): 'ok' | 'pending' | 'danger' {
    return health;
  }
</script>

<div class="watch-panel">
  <section class="watch-col">
    <h3 class="watch-col-title">
      Watches
      <span class="watch-count">{data.summary.watches}</span>
    </h3>
    {#if data.watches.length === 0}
      <EmptyState
        title="No watches running"
        hint="Start one with envelope watch --account <email> to push new-message events."
      />
    {:else}
      <ul class="watch-list">
        {#each data.watches as w (w.id)}
          <li class="watch-item">
            <div class="watch-item-head">
              <MonoTag>{w.folder}</MonoTag>
              <Badge variant={variant(w.health)}>{w.status}</Badge>
            </div>
            <span class="watch-item-sub">heartbeat {age(w.last_heartbeat_at)}</span>
            {#if w.failure_reason}
              <span class="watch-fail">{w.failure_reason}</span>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  </section>

  <section class="watch-col">
    <h3 class="watch-col-title">
      Delivery routes
      {#if data.summary.dead_letter > 0}
        <Badge variant="danger">{data.summary.dead_letter} dead-letter</Badge>
      {/if}
    </h3>
    {#if data.routes.length === 0}
      <EmptyState
        title="No delivery routes"
        hint="Register a webhook with envelope events routes add to push events downstream."
      />
    {:else}
      <ul class="watch-list">
        {#each data.routes as r (r.id)}
          <li class="watch-item">
            <div class="watch-item-head">
              {#if r.secret_prefix}
                <MonoTag>{r.secret_prefix}…</MonoTag>
              {/if}
              <Badge variant={variant(r.health)}>{r.enabled ? 'enabled' : 'disabled'}</Badge>
            </div>
            <span class="watch-item-sub">
              {r.deliveries.delivered} delivered · {r.deliveries.pending} pending ·
              <span class:dead={r.deliveries.dead > 0}>{r.deliveries.dead} dead</span>
            </span>
          </li>
        {/each}
      </ul>
    {/if}
  </section>
</div>

<style>
  .watch-panel {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 1.25rem;
  }
  .watch-col-title {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin: 0 0 0.65rem;
    font-size: 0.8125rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--env-muted);
  }
  .watch-count {
    font-family: var(--font-mono);
    color: var(--env-ink);
  }
  .watch-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .watch-item {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    padding: 0.6rem 0.7rem;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-sm, 3px);
    background: var(--env-surface);
  }
  .watch-item-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
  }
  .watch-item-sub {
    font-size: 0.75rem;
    color: var(--env-muted);
  }
  .watch-item-sub .dead {
    color: var(--env-warn);
    font-weight: 600;
  }
  .watch-fail {
    font-size: 0.75rem;
    color: var(--env-warn);
  }
  @media (max-width: 720px) {
    .watch-panel {
      grid-template-columns: 1fr;
    }
  }
</style>
