<script lang="ts">
  // UndoToast — undo-send toast with countdown and cancel affordance.
  //
  // Shown after a successful compose/reply when the response carries
  // `cooldown_seconds > 0`. Counts down the remaining window, then
  // auto-dismisses. Clicking "Undo" calls discard on the queued draft.
  //
  // Props:
  //   draftId     — the draft to discard on undo
  //   accountId   — account owning the draft
  //   seconds     — cooldown_seconds from the compose response
  //   ondismiss   — called when the toast finishes (dismissed or undone)
  import { api, EnvelopeApiError } from '$lib/api';

  let {
    draftId,
    accountId,
    seconds,
    ondismiss
  }: {
    draftId: string;
    accountId: string;
    seconds: number;
    ondismiss?: () => void;
  } = $props();

  // Use $state so reactive reads inside setInterval closure see live values.
  let remaining = $state(0);
  let undoing = $state(false);
  let undoError = $state<string | null>(null);
  let handle: ReturnType<typeof setInterval> | null = null;

  $effect(() => {
    // Initialise from the prop. Re-runs if `seconds` prop changes (rare but safe).
    remaining = seconds;
    // Start a 1-second ticker.
    handle = setInterval(() => {
      remaining -= 1;
      if (remaining <= 0) {
        stop();
        ondismiss?.();
      }
    }, 1000);
    return () => stop();
  });

  function stop() {
    if (handle !== null) {
      clearInterval(handle);
      handle = null;
    }
  }

  async function undo() {
    stop();
    undoing = true;
    undoError = null;
    try {
      await api.discardDraft(accountId, draftId);
      ondismiss?.();
    } catch (e) {
      const err = e as EnvelopeApiError;
      undoError = err.message ?? 'Undo failed.';
      // Restart timer so the toast auto-dismisses even if undo fails.
      remaining = 3;
      handle = setInterval(() => {
        remaining -= 1;
        if (remaining <= 0) {
          stop();
          ondismiss?.();
        }
      }, 1000);
    } finally {
      undoing = false;
    }
  }
</script>

<div id="undo-toast" class="undo-toast" role="status" aria-live="polite">
  {#if undoError}
    <span class="undo-body">Undo failed. Sending now.</span>
  {:else}
    <span class="undo-body">Sending in {remaining}s</span>
    <button
      class="undo-btn"
      type="button"
      aria-label="Undo send"
      disabled={undoing}
      onclick={undo}
    >
      {undoing ? 'Cancelling…' : 'Undo'}
    </button>
  {/if}
  <button
    class="undo-dismiss"
    type="button"
    aria-label="Dismiss"
    onclick={() => { stop(); ondismiss?.(); }}
  >×</button>
</div>

<style>
  .undo-toast {
    display: flex;
    align-items: center;
    gap: 0.65rem;
    padding: 0.6rem 0.85rem;
    background: var(--env-surface);
    border: 1px solid var(--env-rule);
    border-left: 3px solid var(--env-accent);
    border-radius: var(--radius-sm, 3px);
    box-shadow: 0 4px 14px rgba(10, 10, 10, 0.08);
    font-size: 0.8125rem;
    color: var(--env-ink);
    max-width: 24rem;
    position: fixed;
    bottom: 1.25rem;
    left: 50%;
    transform: translateX(-50%);
    z-index: 100;
  }

  .undo-body {
    flex: 1;
  }

  .undo-btn {
    font-size: 0.8125rem;
    font-weight: 600;
    color: var(--env-accent);
    background: none;
    border: none;
    padding: 0 0.1rem;
    cursor: pointer;
    text-decoration: underline;
  }
  .undo-btn:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .undo-dismiss {
    background: none;
    border: none;
    font-size: 1.1rem;
    line-height: 1;
    color: var(--env-muted);
    cursor: pointer;
    padding: 0;
  }
  .undo-dismiss:hover {
    color: var(--env-ink);
  }
</style>
