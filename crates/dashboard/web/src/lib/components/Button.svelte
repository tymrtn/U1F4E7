<script lang="ts">
  // Action button. The label is ALWAYS an action verb (Send, Archive, Retry) —
  // never a status or noun. Variants map to the ink/accent/warn palette.
  import type { Snippet } from 'svelte';

  type Variant = 'primary' | 'ghost' | 'danger';

  let {
    variant = 'primary',
    type = 'button',
    form,
    disabled = false,
    onclick,
    children
  }: {
    variant?: Variant;
    type?: 'button' | 'submit' | 'reset';
    form?: string;
    disabled?: boolean;
    onclick?: (e: MouseEvent) => void;
    children: Snippet;
  } = $props();
</script>

<button class="env-btn env-btn-{variant}" {type} {form} {disabled} {onclick}>
  {@render children()}
</button>

<style>
  .env-btn {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.4rem 0.8rem;
    font-family: var(--font-sans);
    font-size: 0.8125rem;
    font-weight: 600;
    line-height: 1.1;
    border: 1px solid transparent;
    border-radius: var(--radius-sm, 3px);
    cursor: pointer;
    transition:
      background-color 0.12s ease,
      border-color 0.12s ease,
      color 0.12s ease;
  }
  .env-btn:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }

  .env-btn-primary {
    background: var(--env-accent);
    color: #fff;
  }
  .env-btn-primary:not(:disabled):hover {
    background: #14563b;
  }

  .env-btn-ghost {
    background: transparent;
    color: var(--env-ink);
    /* Mid-tone border (not the pale rule) so ghost reads as an actionable
     * control, clearly distinct from the disabled state's faded look. */
    border-color: var(--env-muted, #8a8780);
  }
  .env-btn-ghost:not(:disabled):hover {
    background: var(--env-accent-soft);
    border-color: var(--env-accent);
    color: var(--env-accent);
  }

  .env-btn-danger {
    background: transparent;
    color: var(--env-warn);
    border-color: var(--env-warn);
  }
  .env-btn-danger:not(:disabled):hover {
    background: var(--env-warn);
    color: #fff;
  }
</style>
