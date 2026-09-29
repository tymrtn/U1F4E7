<script lang="ts">
  // Durable receipts for single-message actions that take a message out of
  // view (Junk, Archive, Trash, Snooze). Each names where the message went
  // and, when the server returned an exact handle, offers the way back. A
  // receipt stays until dismissed, so a list reload (or a failed one) never
  // hides the fact that the write happened.
  import { getMessageActions, type Receipt } from '$lib/message-actions.svelte';
  import Icon from './Icon.svelte';

  const actions = getMessageActions();
  /** Only the latest few; older ones are still dismissible one by one. */
  const visible = $derived(actions.receipts.slice(-3));
  let undoing = $state<number | null>(null);
  let undoError = $state<{ id: number; message: string } | null>(null);

  async function undo(r: Receipt) {
    if (!r.undo || undoing !== null) return;
    undoing = r.id;
    undoError = null;
    const outcome = await actions.dispatch(r.undo.target, r.undo.command);
    undoing = null;
    if (outcome.status === 'ok') actions.dismissReceipt(r.id);
    else if (outcome.status === 'error') undoError = { id: r.id, message: outcome.message };
  }
</script>

<div id="action-receipts" class="action-receipts" aria-live="polite" aria-label="Message action results">
  {#each visible as r (r.id)}
    <div class="action-receipt" data-kind={r.kind}>
      <span class="action-receipt-text">
        {r.text}{#if r.target.subject}<span class="action-receipt-subject"> · {r.target.subject}</span>{/if}
      </span>
      {#if r.undo}
        <button
          type="button"
          class="action-receipt-undo"
          aria-busy={undoing === r.id}
          disabled={undoing !== null}
          onclick={() => undo(r)}
        >
          {undoing === r.id ? 'Working…' : r.undo.label}
        </button>
      {/if}
      <button
        type="button"
        class="action-receipt-x"
        aria-label="Dismiss"
        onclick={() => actions.dismissReceipt(r.id)}
      >
        <Icon name="x" size={12} />
      </button>
      {#if undoError?.id === r.id}
        <p class="action-receipt-err" role="alert">{undoError.message}</p>
      {/if}
    </div>
  {/each}
</div>

<style>
  .action-receipts {
    position: fixed;
    left: 50%;
    transform: translateX(-50%);
    bottom: calc(1rem + env(safe-area-inset-bottom, 0px));
    z-index: 60;
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    width: min(32rem, calc(100vw - 2rem));
    pointer-events: none;
  }
  .action-receipt {
    pointer-events: auto;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.5rem;
    padding: 0.55rem 0.7rem;
    background: var(--env-ink);
    color: var(--env-paper);
    border-radius: var(--radius-md, 5px);
    box-shadow: 0 6px 20px rgba(10, 10, 10, 0.2);
    font-size: 0.8125rem;
  }
  .action-receipt-text {
    flex: 1 1 12rem;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .action-receipt-subject {
    opacity: 0.7;
  }
  .action-receipt-undo {
    border: 1px solid color-mix(in srgb, var(--env-paper) 45%, transparent);
    background: none;
    color: var(--env-paper);
    border-radius: var(--radius-sm, 3px);
    padding: 0.3rem 0.6rem;
    font: inherit;
    font-size: 0.75rem;
    font-weight: 600;
    cursor: pointer;
    min-height: 32px;
  }
  .action-receipt-undo:disabled {
    opacity: 0.6;
    cursor: progress;
  }
  .action-receipt-x {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 32px;
    height: 32px;
    border: none;
    background: none;
    color: var(--env-paper);
    cursor: pointer;
    opacity: 0.8;
  }
  .action-receipt-err {
    flex-basis: 100%;
    margin: 0;
    font-size: 0.75rem;
    color: #ffb4a8;
  }
</style>
