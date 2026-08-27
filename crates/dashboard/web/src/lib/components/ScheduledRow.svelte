<script lang="ts">
  // A scheduled send: subject, countdown (or "due"), the latest Governor verdict
  // badge, and a "Cancel send" action (routed to the per-account draft HOLD
  // endpoint by the parent: the draft leaves the outbox and is kept). Read-only
  // aside from that explicit action.
  import Button from './Button.svelte';
  import MonoTag from './MonoTag.svelte';
  import GovernorVerdictBadge from './GovernorVerdictBadge.svelte';
  import type { ScheduledItem } from '$lib/cockpit-api';

  let {
    item,
    countdown,
    busy = false,
    oncancel
  }: {
    item: ScheduledItem;
    countdown: (item: ScheduledItem) => string;
    busy?: boolean;
    oncancel: (item: ScheduledItem) => void;
  } = $props();
</script>

<div class="scheduled-row" class:busy>
  <div class="scheduled-meta">
    <span class="scheduled-subject">{item.subject || '(no subject)'}</span>
    <span class="scheduled-sub">
      {#if item.created_by}
        <MonoTag>{item.created_by}</MonoTag>
      {/if}
      <span class="scheduled-when" class:due={item.due}>{countdown(item)}</span>
      {#if item.governor}
        <GovernorVerdictBadge verdict={item.governor.verdict} decision={item.governor.decision} />
      {/if}
    </span>
  </div>
  <div class="scheduled-actions">
    <Button variant="danger" disabled={busy} onclick={() => oncancel(item)}>Cancel send</Button>
  </div>
</div>

<style>
  .scheduled-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    padding: 0.65rem 0.75rem;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-sm, 3px);
    background: var(--env-surface);
  }
  .scheduled-row.busy {
    opacity: 0.6;
  }
  .scheduled-meta {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    min-width: 0;
  }
  .scheduled-subject {
    font-size: 0.875rem;
    font-weight: 600;
    color: var(--env-ink);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .scheduled-sub {
    display: inline-flex;
    align-items: center;
    gap: 0.5rem;
  }
  .scheduled-when {
    font-family: var(--font-mono);
    font-size: 0.75rem;
    color: var(--env-muted);
  }
  .scheduled-when.due {
    color: var(--env-accent);
    font-weight: 600;
  }
  .scheduled-actions {
    flex-shrink: 0;
  }
</style>
