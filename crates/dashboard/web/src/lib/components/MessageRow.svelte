<script lang="ts">
  // A single row in the message list. Supports selection via checkbox (with
  // shift-click range), keyboard 'x' toggle, star toggle (optimistic), and
  // an account chip for unified views. ~44px min-height; unread = bold weight.
  import type { SelectionStore } from '$lib/selection.svelte';

  type Message = {
    key: string;       // unique key, e.g. "accountId:uid"
    uid: number;
    accountId: string;
    subject: string;
    from: string;
    date: string | null;
    snippet: string | null;
    unread: boolean;
    starred: boolean;
    accountChip?: string | null;   // display label for unified rows
    href: string;
  };

  let {
    message,
    selection,
    orderedKeys,
    active = false,
    onstar,
    onfocus,
  }: {
    message: Message;
    selection: SelectionStore;
    orderedKeys: string[];
    active?: boolean;
    onstar?: (uid: number, accountId: string, star: boolean) => void;
    onfocus?: (key: string) => void;
  } = $props();

  const isSelected = $derived(selection.isSelected(message.key));

  function handleCheckbox(e: MouseEvent) {
    e.stopPropagation();
    if (e.shiftKey) {
      selection.rangeSelect(message.key, orderedKeys);
    } else {
      selection.toggle(message.key);
    }
  }

  function handleRowKeydown(e: KeyboardEvent) {
    if (e.key === 'x') {
      e.preventDefault();
      selection.keyToggle(message.key);
    }
  }

  function handleStar(e: MouseEvent) {
    e.preventDefault();
    e.stopPropagation();
    onstar?.(message.uid, message.accountId, !message.starred);
  }

  function handleFocus() {
    onfocus?.(message.key);
  }

  function fmtDate(iso: string | null): string {
    if (!iso) return '';
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return '';
    const now = new Date();
    const sameDay = d.toDateString() === now.toDateString();
    return sameDay
      ? d.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })
      : d.toLocaleDateString([], { month: 'short', day: 'numeric' });
  }
</script>

<!-- svelte-ignore a11y_interactive_supports_focus -->
<div
  class="msg-row"
  class:is-selected={isSelected}
  class:is-active={active}
  class:is-unread={message.unread}
  role="row"
  data-msg-key={message.key}
  aria-selected={isSelected}
  onkeydown={handleRowKeydown}
  onfocus={handleFocus}
>
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <span class="msg-check" onclick={handleCheckbox} role="checkbox" aria-checked={isSelected} aria-label="Select message" tabindex="0">
    <input
      type="checkbox"
      tabindex="-1"
      checked={isSelected}
      aria-hidden="true"
    />
  </span>

  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <span
    class="msg-star"
    class:is-starred={message.starred}
    onclick={handleStar}
    role="button"
    aria-label={message.starred ? 'Unstar message' : 'Star message'}
    tabindex="0"
    onkeydown={(e) => e.key === 'Enter' || e.key === ' ' ? handleStar(e as unknown as MouseEvent) : null}
  >
    {message.starred ? '★' : '☆'}
  </span>

  <a class="msg-body" class:is-unread={message.unread} href={message.href} tabindex="0">
    <div class="msg-line1">
      <span class="msg-sender">{message.from || message.accountId}</span>
      <span class="msg-date">{fmtDate(message.date)}</span>
    </div>
    <div class="msg-line2">
      <span class="msg-subject">{message.subject || '(no subject)'}</span>
      {#if message.accountChip}
        <span class="msg-chip" title={message.accountChip}>{message.accountChip}</span>
      {/if}
    </div>
    {#if message.snippet}
      <p class="msg-snippet">{message.snippet}</p>
    {/if}
  </a>
</div>

<style>
  .msg-row {
    display: flex;
    align-items: flex-start;
    min-height: 44px;
    border-bottom: 1px solid var(--env-rule);
    background: var(--env-paper);
    transition: background 0.07s;
  }
  .msg-row:hover {
    background: var(--env-accent-soft);
  }
  .msg-row.is-active {
    background: var(--env-accent-soft);
    box-shadow: inset 3px 0 0 var(--env-accent);
  }
  .msg-row.is-selected {
    background: color-mix(in srgb, var(--env-accent-soft) 60%, transparent);
  }
  .msg-check {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 32px;
    flex-shrink: 0;
    padding-top: 0.5rem;
    cursor: pointer;
    color: var(--env-muted);
  }
  .msg-check input[type='checkbox'] {
    pointer-events: none;
    accent-color: var(--env-accent);
    width: 14px;
    height: 14px;
    cursor: pointer;
  }
  .msg-star {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 24px;
    flex-shrink: 0;
    padding-top: 0.5rem;
    font-size: 0.875rem;
    color: var(--env-muted);
    cursor: pointer;
    user-select: none;
  }
  .msg-star.is-starred {
    color: var(--env-pending);
  }
  .msg-star:hover {
    color: var(--env-pending);
  }
  .msg-body {
    flex: 1;
    min-width: 0;
    padding: 0.5rem 1rem 0.5rem 0.35rem;
    text-decoration: none;
    color: var(--env-ink);
    display: block;
  }
  .msg-line1 {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 0.5rem;
  }
  .msg-sender {
    font-size: 0.8125rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .is-unread .msg-sender,
  .is-unread .msg-subject {
    font-weight: 700;
  }
  .msg-date {
    font-size: 0.6875rem;
    color: var(--env-muted);
    flex-shrink: 0;
    font-family: var(--font-mono);
  }
  .msg-line2 {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 0.5rem;
    margin-top: 0.1rem;
  }
  .msg-subject {
    font-size: 0.8125rem;
    color: var(--env-ink);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .msg-chip {
    font-family: var(--font-mono);
    font-size: 0.625rem;
    color: var(--env-muted);
    background: var(--env-surface);
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-xs, 2px);
    padding: 0 0.3rem;
    flex-shrink: 0;
    max-width: 40%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .msg-snippet {
    margin: 0.2rem 0 0;
    font-size: 0.75rem;
    color: var(--env-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
