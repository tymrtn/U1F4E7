<script lang="ts">
  // Transient notification. Rendered by a host region; the message text is
  // supplied by the caller and should be human-readable, not a raw code.
  import type { Snippet } from 'svelte';

  type Variant = 'ok' | 'warn' | 'danger';

  let {
    variant = 'ok',
    onclose,
    children
  }: {
    variant?: Variant;
    onclose?: () => void;
    children: Snippet;
  } = $props();
</script>

<div class="env-toast env-toast-{variant}" role="status" aria-live="polite">
  <span class="env-toast-body">{@render children()}</span>
  {#if onclose}
    <button class="env-toast-close" type="button" aria-label="Dismiss" onclick={onclose}>×</button>
  {/if}
</div>

<style>
  .env-toast {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    padding: 0.6rem 0.85rem;
    background: var(--env-surface);
    border: 1px solid var(--env-rule);
    border-left-width: 3px;
    border-radius: var(--radius-sm, 3px);
    box-shadow: 0 4px 14px rgba(10, 10, 10, 0.08);
    font-size: 0.8125rem;
    color: var(--env-ink);
    max-width: 24rem;
  }
  .env-toast-ok {
    border-left-color: var(--env-accent);
  }
  .env-toast-warn {
    border-left-color: var(--env-pending);
  }
  .env-toast-danger {
    border-left-color: var(--env-warn);
  }
  .env-toast-body {
    flex: 1;
  }
  .env-toast-close {
    background: none;
    border: none;
    font-size: 1.1rem;
    line-height: 1;
    color: var(--env-muted);
    cursor: pointer;
    padding: 0;
  }
  .env-toast-close:hover {
    color: var(--env-ink);
  }
</style>
