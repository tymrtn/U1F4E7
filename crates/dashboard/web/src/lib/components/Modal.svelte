<script lang="ts">
  // Centered dialog. Backdrop click and Escape close it. The title is a plain
  // heading; actions go in the footer slot.
  import type { Snippet } from 'svelte';

  let {
    open = false,
    title,
    onclose,
    children,
    footer
  }: {
    open?: boolean;
    title: string;
    onclose?: () => void;
    children: Snippet;
    footer?: Snippet;
  } = $props();

  function close() {
    onclose?.();
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') close();
  }
</script>

<svelte:window on:keydown={open ? onkeydown : undefined} />

{#if open}
  <!-- Backdrop dismiss is a convenience; Escape (window handler) and the
       explicit close button are the accessible paths. -->
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="env-modal-backdrop" onclick={close}>
    <div
      class="env-modal"
      role="dialog"
      aria-modal="true"
      aria-label={title}
      tabindex="-1"
      onclick={(e) => e.stopPropagation()}
    >
      <header class="env-modal-head">
        <h2 class="env-modal-title">{title}</h2>
        <button class="env-modal-close" type="button" aria-label="Close" onclick={close}>×</button>
      </header>
      <div class="env-modal-body">{@render children()}</div>
      {#if footer}
        <footer class="env-modal-foot">{@render footer()}</footer>
      {/if}
    </div>
  </div>
{/if}

<style>
  .env-modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(10, 10, 10, 0.4);
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 1.5rem;
    z-index: 50;
  }
  .env-modal {
    background: var(--env-surface);
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-md, 5px);
    box-shadow: 0 12px 40px rgba(10, 10, 10, 0.2);
    width: 100%;
    max-width: 30rem;
    display: flex;
    flex-direction: column;
  }
  .env-modal-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0.85rem 1rem;
    border-bottom: 1px solid var(--env-rule);
  }
  .env-modal-title {
    margin: 0;
    font-size: 0.9375rem;
    font-weight: 600;
  }
  .env-modal-close {
    background: none;
    border: none;
    font-size: 1.25rem;
    line-height: 1;
    color: var(--env-muted);
    cursor: pointer;
  }
  .env-modal-close:hover {
    color: var(--env-ink);
  }
  .env-modal-body {
    padding: 1rem;
    font-size: 0.875rem;
    line-height: 1.5;
  }
  .env-modal-foot {
    display: flex;
    justify-content: flex-end;
    gap: 0.5rem;
    padding: 0.75rem 1rem;
    border-top: 1px solid var(--env-rule);
  }
</style>
