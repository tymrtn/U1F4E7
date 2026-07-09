<script lang="ts">
  // Side drawer, anchored right. Same close semantics as Modal; used for
  // contextual detail panels that shouldn't take over the whole viewport.
  import type { Snippet } from 'svelte';

  let {
    open = false,
    title,
    onclose,
    children
  }: {
    open?: boolean;
    title: string;
    onclose?: () => void;
    children: Snippet;
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
  <div class="env-drawer-backdrop" onclick={close}>
    <div
      class="env-drawer"
      role="dialog"
      aria-modal="true"
      aria-label={title}
      tabindex="-1"
      onclick={(e) => e.stopPropagation()}
    >
      <header class="env-drawer-head">
        <h2 class="env-drawer-title">{title}</h2>
        <button class="env-drawer-close" type="button" aria-label="Close" onclick={close}>×</button>
      </header>
      <div class="env-drawer-body">{@render children()}</div>
    </div>
  </div>
{/if}

<style>
  .env-drawer-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(10, 10, 10, 0.3);
    display: flex;
    justify-content: flex-end;
    z-index: 50;
  }
  .env-drawer {
    background: var(--env-surface);
    border-left: 1px solid var(--env-rule);
    width: 100%;
    max-width: 22rem;
    height: 100%;
    display: flex;
    flex-direction: column;
    box-shadow: -8px 0 32px rgba(10, 10, 10, 0.12);
  }
  .env-drawer-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0.85rem 1rem;
    border-bottom: 1px solid var(--env-rule);
  }
  .env-drawer-title {
    margin: 0;
    font-size: 0.9375rem;
    font-weight: 600;
  }
  .env-drawer-close {
    background: none;
    border: none;
    font-size: 1.25rem;
    line-height: 1;
    color: var(--env-muted);
    cursor: pointer;
  }
  .env-drawer-close:hover {
    color: var(--env-ink);
  }
  .env-drawer-body {
    padding: 1rem;
    overflow-y: auto;
    font-size: 0.875rem;
    line-height: 1.5;
  }
</style>
