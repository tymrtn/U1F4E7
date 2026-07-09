<script lang="ts">
  // Status badge. Small, quiet, one accent per variant.
  import type { Snippet } from 'svelte';

  type Variant = 'ok' | 'warn' | 'pending' | 'danger';

  let {
    variant = 'ok',
    children
  }: {
    variant?: Variant;
    children: Snippet;
  } = $props();
</script>

<span class="env-badge env-badge-{variant}">{@render children()}</span>

<style>
  .env-badge {
    display: inline-flex;
    align-items: center;
    padding: 0.1rem 0.45rem;
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    font-weight: 500;
    letter-spacing: 0.02em;
    text-transform: uppercase;
    border-radius: var(--radius-xs, 2px);
    border: 1px solid transparent;
  }
  .env-badge-ok {
    background: var(--env-accent-soft);
    color: var(--env-accent);
    border-color: var(--env-accent);
  }
  .env-badge-warn {
    background: var(--env-warn-soft);
    color: var(--env-warn);
    border-color: var(--env-warn);
  }
  .env-badge-pending {
    background: var(--env-pending-soft);
    color: var(--env-pending);
    border-color: var(--env-pending);
  }
  .env-badge-danger {
    background: var(--env-warn);
    color: #fff;
    border-color: var(--env-warn);
  }
</style>
