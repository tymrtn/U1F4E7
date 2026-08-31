<script lang="ts">
  // A single row in the message list (design plan rev 3, A2 identity rows +
  // A6 hidden verbs). Lead slot shows the sender avatar, swapping to the
  // selection checkbox on hover/selection. Two text lines — who → when, then
  // subject — with a muted snippet. Unread = dot + bold. A GTD verb cluster
  // (reply / snooze / delegate / archive / delete) reveals on hover/focus and
  // is keyboard-driven; it dispatches the same single-item ops the reader and
  // BulkToolbar use, then bumps the shared mailbox-ops signal so the list
  // refreshes. `delegate` is present but disabled until its backend lands
  // (Phase E). Nothing here opens a new send path.
  import type { SelectionStore } from '$lib/selection.svelte';
  import { api, bulkClient, EnvelopeApiError } from '$lib/api';
  import { getMailboxOpsStore } from '$lib/mailbox-ops.svelte';
  import { snoozeOptions, type SnoozeOption } from '$lib/snooze-options';
  import { identityColor } from '$lib/hue';
  import Avatar from './Avatar.svelte';
  import Icon from './Icon.svelte';

  type Message = {
    key: string; // unique key, e.g. "accountId:uid"
    uid: number;
    accountId: string;
    subject: string;
    from: string;
    date: string | null;
    snippet: string | null;
    unread: boolean;
    starred: boolean;
    folder?: string; // source folder — required for the verb cluster ops
    accountChip?: string | null; // display label for unified rows
    href: string;
  };

  let {
    message,
    selection,
    orderedKeys,
    active = false,
    verbs = false,
    onstar,
    onfocus
  }: {
    message: Message;
    selection: SelectionStore;
    orderedKeys: string[];
    active?: boolean;
    /** Enable the GTD hover verb cluster (unified/search inbox surfaces). */
    verbs?: boolean;
    onstar?: (uid: number, accountId: string, star: boolean) => void;
    onfocus?: (key: string) => void;
  } = $props();

  const mailboxOps = getMailboxOpsStore();

  const isSelected = $derived(selection.isSelected(message.key));
  const showVerbs = $derived(verbs && !!message.folder);
  const accountTint = $derived(message.accountChip ? identityColor(message.accountId) : null);

  let acting = $state(false);
  let opError = $state<string | null>(null);
  let snoozeMenuOpen = $state(false);
  let snoozeChoices = $state<SnoozeOption[]>([]);

  function handleCheckbox(e: MouseEvent) {
    e.stopPropagation();
    if (e.shiftKey) {
      selection.rangeSelect(message.key, orderedKeys);
    } else {
      selection.toggle(message.key);
    }
  }

  function handleCheckboxKeydown(e: KeyboardEvent) {
    if (e.key === ' ' || e.key === 'Enter') {
      e.preventDefault();
      e.stopPropagation();
      if (e.shiftKey) selection.rangeSelect(message.key, orderedKeys);
      else selection.toggle(message.key);
    }
  }

  function handleRowKeydown(e: KeyboardEvent) {
    if (e.key === 'x') {
      e.preventDefault();
      selection.keyToggle(message.key);
      return;
    }
    if (!showVerbs || acting) return;
    // Verb keys mirror the cluster. Ignore when typing in a field.
    const target = e.target as HTMLElement | null;
    if (target && /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName)) return;
    switch (e.key) {
      case 'e':
        e.preventDefault();
        void archive();
        break;
      case '#':
        e.preventDefault();
        void del();
        break;
      case 's':
        e.preventDefault();
        toggleSnoozeMenu();
        break;
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

  // ── Verb cluster ops: single-item, same paths as the reader/BulkToolbar ──
  async function runMove(toFolder: string, verb: string) {
    if (acting) return;
    acting = true;
    opError = null;
    try {
      const folder = message.folder!;
      const result = await bulkClient({ type: 'move', to_folder: toFolder, folder }, [
        { accountId: message.accountId, uid: message.uid, folder }
      ]);
      if (result.failed.length > 0) {
        opError = `Couldn't ${verb}: ${result.failed[0].error}`;
      } else {
        mailboxOps.operated();
      }
    } catch (e) {
      const err = e as EnvelopeApiError;
      opError = `Couldn't ${verb}: ${err.message ?? 'operation failed'}`;
    } finally {
      acting = false;
    }
  }

  const archive = () => runMove('\\Archive', 'archive');
  const del = () => runMove('\\Trash', 'delete');

  function toggleSnoozeMenu() {
    if (snoozeMenuOpen) {
      snoozeMenuOpen = false;
      return;
    }
    snoozeChoices = snoozeOptions(new Date());
    snoozeMenuOpen = true;
  }

  async function chooseSnooze(opt: SnoozeOption) {
    snoozeMenuOpen = false;
    if (acting) return;
    acting = true;
    opError = null;
    try {
      await api.snoozeMessage(message.accountId, message.uid, {
        folder: message.folder!,
        // A UTC instant, matching BulkToolbar: the unsnooze sweep compares
        // against UTC now, so a naive local string fires off by the offset.
        return_at: opt.at.toISOString(),
        subject: message.subject
      });
      mailboxOps.operated();
    } catch (e) {
      const err = e as EnvelopeApiError;
      opError = `Couldn't snooze: ${err.message ?? 'operation failed'}`;
    } finally {
      acting = false;
    }
  }

  function snoozeHint(at: Date): string {
    const now = new Date();
    const sameDay = at.toDateString() === now.toDateString();
    return sameDay
      ? at.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })
      : at.toLocaleDateString([], { weekday: 'short', hour: 'numeric' });
  }
</script>

<!-- svelte-ignore a11y_interactive_supports_focus -->
<div
  class="msg-row"
  class:is-selected={isSelected}
  class:is-active={active}
  class:is-unread={message.unread}
  class:has-tint={!!accountTint}
  style={accountTint ? `--account-tint: ${accountTint}` : undefined}
  role="row"
  data-msg-key={message.key}
  aria-selected={isSelected}
  onkeydown={handleRowKeydown}
  onfocus={handleFocus}
>
  <div class="msg-lead">
    <span class="msg-avatar"><Avatar name={message.from || message.accountId} size={30} /></span>
    <!-- svelte-ignore a11y_click_events_have_key_events -->
    <span
      class="msg-check"
      onclick={handleCheckbox}
      onkeydown={handleCheckboxKeydown}
      role="checkbox"
      aria-checked={isSelected}
      aria-label="Select message"
      tabindex="0"
    >
      <input type="checkbox" tabindex="-1" checked={isSelected} aria-hidden="true" />
    </span>
  </div>

  <a class="msg-body" class:is-unread={message.unread} href={message.href} tabindex="0">
    <div class="msg-line1">
      <span class="msg-sender">
        {#if message.unread}
          <span class="msg-sr">Unread.</span>
          <span class="msg-unread-dot" aria-hidden="true"></span>
        {/if}
        {message.from || message.accountId}
      </span>
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

  <div class="msg-tail">
    {#if showVerbs}
      <div class="msg-verbs" role="group" aria-label="Message actions">
        <a class="verb" href={message.href} aria-label="Reply" title="Reply (r)">
          <Icon name="reply" size={15} />
        </a>
        <div class="verb-snooze">
          <button
            class="verb"
            type="button"
            disabled={acting}
            aria-label="Snooze"
            aria-haspopup="menu"
            aria-expanded={snoozeMenuOpen}
            title="Snooze (s)"
            onclick={(e) => {
              e.preventDefault();
              e.stopPropagation();
              toggleSnoozeMenu();
            }}
          >
            <Icon name="clock" size={15} />
          </button>
          {#if snoozeMenuOpen}
            <div class="snooze-menu" role="menu">
              {#each snoozeChoices as opt (opt.key)}
                <button
                  class="snooze-item"
                  type="button"
                  role="menuitem"
                  onclick={(e) => {
                    e.preventDefault();
                    e.stopPropagation();
                    void chooseSnooze(opt);
                  }}
                >
                  <span>{opt.label}</span>
                  <span class="snooze-when">{snoozeHint(opt.at)}</span>
                </button>
              {/each}
            </div>
          {/if}
        </div>
        <button
          class="verb verb-delegate"
          type="button"
          disabled
          aria-label="Delegate to an agent"
          title="Delegate to an agent — lands with the digest backend (Phase E)"
        >
          <Icon name="bot" size={15} />
        </button>
        <button
          class="verb"
          type="button"
          disabled={acting}
          aria-label="Archive"
          title="Archive (e)"
          onclick={(e) => {
            e.preventDefault();
            e.stopPropagation();
            void archive();
          }}
        >
          <Icon name="archive" size={15} />
        </button>
        <button
          class="verb"
          type="button"
          disabled={acting}
          aria-label="Delete"
          title="Delete (#)"
          onclick={(e) => {
            e.preventDefault();
            e.stopPropagation();
            void del();
          }}
        >
          <Icon name="trash" size={15} />
        </button>
      </div>
    {/if}

    <!-- svelte-ignore a11y_click_events_have_key_events -->
    <span
      class="msg-star"
      class:is-starred={message.starred}
      onclick={handleStar}
      role="button"
      aria-label={message.starred ? 'Unstar message' : 'Star message'}
      tabindex="0"
      onkeydown={(e) =>
        e.key === 'Enter' || e.key === ' ' ? handleStar(e as unknown as MouseEvent) : null}
    >
      {message.starred ? '★' : '☆'}
    </span>
  </div>

  {#if opError}
    <p class="msg-op-error" role="alert">
      {opError}
      <button
        class="msg-op-dismiss"
        type="button"
        aria-label="Dismiss error"
        onclick={() => (opError = null)}
      >
        <Icon name="x" size={12} />
      </button>
    </p>
  {/if}
</div>

<svelte:window
  onclick={() => {
    if (snoozeMenuOpen) snoozeMenuOpen = false;
  }}
  onkeydown={(e) => {
    if (e.key === 'Escape' && snoozeMenuOpen) snoozeMenuOpen = false;
  }}
/>

<style>
  .msg-row {
    display: grid;
    grid-template-columns: auto 1fr auto;
    align-items: flex-start;
    column-gap: 0.6rem;
    min-height: 52px;
    padding: 0.5rem 0.75rem 0.5rem 0;
    border-bottom: 1px solid var(--env-rule);
    background: var(--env-paper);
    transition: background 0.07s;
    position: relative;
  }
  /* Account identity tick (A3): a thin left bar tinted by the account hue,
     shown in unified (multi-account) rows and superseded by the active
     accent marker. */
  .msg-row.has-tint::before {
    content: '';
    position: absolute;
    left: 0;
    top: 0;
    bottom: 0;
    width: 3px;
    background: var(--account-tint);
    opacity: 0.85;
  }
  .msg-row:hover {
    background: var(--env-soft);
  }
  .msg-row.is-active {
    background: var(--env-accent-soft);
  }
  .msg-row.is-active::before {
    content: '';
    position: absolute;
    left: 0;
    top: 0;
    bottom: 0;
    width: 3px;
    background: var(--env-accent);
    opacity: 1;
  }
  .msg-row.is-selected {
    background: color-mix(in srgb, var(--env-accent-soft) 60%, transparent);
  }

  /* Lead slot: avatar by default, checkbox on hover/selection (same cell). */
  .msg-lead {
    position: relative;
    width: 30px;
    height: 30px;
    margin-left: 0.75rem;
    flex-shrink: 0;
  }
  .msg-avatar {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    transition: opacity 0.08s;
  }
  .msg-check {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    cursor: pointer;
    color: var(--env-muted);
    opacity: 0;
    transition: opacity 0.08s;
  }
  .msg-row:hover .msg-avatar,
  .msg-row.is-selected .msg-avatar {
    opacity: 0;
  }
  .msg-row:hover .msg-check,
  .msg-row.is-selected .msg-check,
  /* Reveal for keyboard users the moment focus enters the row, so the
     checkbox is never a focus target hidden at opacity 0 (WCAG 2.4.7). */
  .msg-row:focus-within .msg-check {
    opacity: 1;
  }
  .msg-check:focus-visible {
    opacity: 1;
    outline: 2px solid color-mix(in srgb, var(--env-accent) 62%, white);
    outline-offset: 2px;
    border-radius: var(--radius-xs, 2px);
  }
  /* Visually hidden but exposed to assistive tech. */
  .msg-sr {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
    white-space: nowrap;
    border: 0;
  }
  .msg-check input[type='checkbox'] {
    pointer-events: none;
    accent-color: var(--env-accent);
    width: 16px;
    height: 16px;
    cursor: pointer;
  }

  .msg-body {
    min-width: 0;
    padding-top: 0.05rem;
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
    display: flex;
    align-items: center;
    gap: 0.35rem;
  }
  .msg-unread-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--env-accent);
    flex-shrink: 0;
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
    font-variant-numeric: tabular-nums;
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

  /* Tail: verb cluster (revealed) + the always-present star. */
  .msg-tail {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    flex-shrink: 0;
    padding-top: 0.05rem;
  }
  .msg-verbs {
    display: flex;
    align-items: center;
    gap: 0.1rem;
    opacity: 0;
    transition: opacity 0.08s;
  }
  .msg-row:hover .msg-verbs,
  .msg-verbs:focus-within {
    opacity: 1;
  }
  .verb {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 26px;
    height: 26px;
    border: none;
    background: none;
    border-radius: var(--radius-sm, 3px);
    color: var(--env-muted);
    cursor: pointer;
    text-decoration: none;
  }
  .verb:hover:not(:disabled) {
    background: var(--env-surface);
    color: var(--env-ink);
  }
  .verb:disabled {
    cursor: not-allowed;
    opacity: 0.4;
  }
  .verb-delegate {
    /* Disabled until the delegate backend lands, but kept visible so the
       verb reads as a promise, not an omission. */
    color: var(--env-muted);
  }
  .verb-snooze {
    position: relative;
    display: inline-flex;
  }
  .snooze-menu {
    position: absolute;
    top: calc(100% + 4px);
    right: 0;
    z-index: 20;
    min-width: 160px;
    background: var(--env-surface);
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-md, 5px);
    box-shadow: 0 6px 20px rgba(10, 10, 10, 0.14);
    padding: 0.25rem;
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
  }
  .snooze-item {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.75rem;
    padding: 0.4rem 0.5rem;
    border: none;
    background: none;
    border-radius: var(--radius-sm, 3px);
    font-size: 0.8125rem;
    color: var(--env-ink);
    cursor: pointer;
    text-align: left;
  }
  .snooze-item:hover {
    background: var(--env-accent-soft);
  }
  .snooze-when {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    color: var(--env-muted);
  }
  .msg-star {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 24px;
    height: 26px;
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
  .msg-op-error {
    grid-column: 1 / -1;
    margin: 0.35rem 0.75rem 0.1rem 3.6rem;
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.75rem;
    color: var(--env-warn);
  }
  .msg-op-dismiss {
    display: inline-flex;
    border: none;
    background: none;
    color: var(--env-warn);
    cursor: pointer;
    padding: 0;
  }
</style>
