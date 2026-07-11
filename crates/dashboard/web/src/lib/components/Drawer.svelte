<script lang="ts">
  // Side drawer, anchored right. Same close semantics as Modal; used for
  // contextual detail panels that shouldn't take over the whole viewport.
  import type { Snippet } from 'svelte';

  let {
    open = false,
    title,
    eyebrow,
    subtitle,
    size = 'compact',
    onclose,
    actions,
    children
  }: {
    open?: boolean;
    title: string;
    eyebrow?: string;
    subtitle?: string;
    size?: 'compact' | 'wide';
    onclose?: () => void;
    actions?: Snippet;
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
      class:is-wide={size === 'wide'}
      role="dialog"
      aria-modal="true"
      aria-label={title}
      tabindex="-1"
      onclick={(e) => e.stopPropagation()}
    >
      <header class="env-drawer-head">
        <div class="env-drawer-heading">
          {#if eyebrow}<p class="env-drawer-eyebrow">{eyebrow}</p>{/if}
          <h2 class="env-drawer-title">{title}</h2>
          {#if subtitle}<p class="env-drawer-subtitle">{subtitle}</p>{/if}
        </div>
        <div class="env-drawer-actions">
          {#if actions}{@render actions()}{/if}
          <button class="env-drawer-close" type="button" aria-label="Close" title="Close" onclick={close}>×</button>
        </div>
      </header>
      <div class="env-drawer-body">{@render children()}</div>
    </div>
  </div>
{/if}

<style>
  .env-drawer-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(10, 10, 10, 0.16);
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
    box-shadow: -24px 0 60px rgba(10, 10, 10, 0.16);
  }
  .env-drawer.is-wide {
    max-width: 47.5rem;
  }
  .env-drawer-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 1rem;
    padding: 1rem 1.125rem 0.875rem;
    border-bottom: 1px solid var(--env-rule);
    background: var(--env-paper);
  }
  .env-drawer-heading {
    min-width: 0;
  }
  .env-drawer-eyebrow {
    margin: 0 0 0.25rem;
    color: var(--env-muted);
    font-family: var(--font-mono);
    font-size: 0.625rem;
    letter-spacing: 0.15em;
    text-transform: uppercase;
  }
  .env-drawer-title {
    margin: 0;
    font-size: 1rem;
    font-weight: 600;
    line-height: 1.2;
  }
  .env-drawer-subtitle {
    margin: 0.3rem 0 0;
    overflow: hidden;
    color: var(--env-muted);
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    line-height: 1.4;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .env-drawer-actions {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    flex: 0 0 auto;
  }
  .env-drawer-close {
    width: 2rem;
    height: 2rem;
    display: grid;
    place-items: center;
    background: var(--env-surface);
    border: 1px solid var(--env-rule);
    font-size: 1.125rem;
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
  .env-drawer.is-wide .env-drawer-body {
    flex: 1;
    min-height: 0;
    padding: 0;
  }
  @media (max-width: 760px) {
    .env-drawer-backdrop {
      background: var(--env-surface);
    }
    .env-drawer.is-wide {
      max-width: none;
      border-left: 0;
      box-shadow: none;
    }
    .env-drawer-head {
      padding: 0.875rem;
    }
  }
</style>
