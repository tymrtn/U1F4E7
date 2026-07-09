<script lang="ts">
  // Monospace inline tag for machine identifiers: UIDs, Message-IDs, tokens.
  import type { Snippet } from 'svelte';

  let { children }: { children: Snippet } = $props();
</script>

<code class="env-monotag">{@render children()}</code>

<style>
  .env-monotag {
    font-family: var(--font-mono);
    font-size: 0.75rem;
    color: var(--env-muted);
    background: var(--env-paper);
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-xs, 2px);
    padding: 0.05rem 0.35rem;
    white-space: nowrap;
  }
</style>
