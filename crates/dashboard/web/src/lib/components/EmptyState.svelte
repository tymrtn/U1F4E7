<script lang="ts">
  // Empty-state placeholder: a title, a hint, and an optional action slot.
  // Copy must be honest and human — never leak status telemetry as the title.
  import type { Snippet } from 'svelte';

  let {
    title,
    hint,
    action
  }: {
    title: string;
    hint?: string;
    action?: Snippet;
  } = $props();
</script>

<div class="env-empty">
  <p class="env-empty-title">{title}</p>
  {#if hint}
    <p class="env-empty-hint">{hint}</p>
  {/if}
  {#if action}
    <div class="env-empty-action">{@render action()}</div>
  {/if}
</div>

<style>
  .env-empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    text-align: center;
    gap: 0.5rem;
    padding: 2.5rem 1.5rem;
    color: var(--env-muted);
  }
  .env-empty-title {
    margin: 0;
    font-family: var(--font-sans);
    font-size: 0.9375rem;
    font-weight: 600;
    color: var(--env-ink);
  }
  .env-empty-hint {
    margin: 0;
    font-size: 0.8125rem;
    max-width: 32ch;
    line-height: 1.4;
  }
  .env-empty-action {
    margin-top: 0.5rem;
  }
</style>
