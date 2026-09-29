<script lang="ts">
  // A single row in the message list (design plan rev 3, A2 identity rows +
  // A6 hidden verbs). Lead slot shows the sender avatar, swapping to the
  // selection checkbox on hover/selection. Two text lines — who → when, then
  // subject — with a muted snippet. Unread = dot + bold.
  //
  // Actions (#170): every single-message action goes through the shared
  // action model (`message-actions.svelte.ts`) with this row's exact
  // (account, folder, UID, UIDVALIDITY, Message-ID). A visible "More actions"
  // button opens the full menu without hover or selection; the hover verb
  // cluster is a desktop shortcut onto the same dispatch. Progress, errors,
  // and a short done note render on the row itself. `delegate` stays present
  // but disabled until its backend lands. Nothing here opens a send path.
  //
  // Right-click (#172) opens the same menu at the pointer, as does Shift+F10 /
  // the ContextMenu key and a touch long-press. It never opens the message or
  // marks it read. The browser's own menu stays on text selections, inputs,
  // images, and links other than the row itself.
  import type { SelectionStore } from '$lib/selection.svelte';
  import {
    getMessageActions,
    type ActionCommand,
    type ActionTarget
  } from '$lib/message-actions.svelte';
  import { formatExactReturn } from '$lib/snooze-options';
  import { getContextMenu } from '$lib/context-menu.svelte';
  import { readState } from '$lib/read-state.svelte';
  import { identityColor } from '$lib/hue';
  import Avatar from './Avatar.svelte';
  import Icon from './Icon.svelte';
  import MessageActionMenu from './MessageActionMenu.svelte';

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
    folder?: string; // source folder — required for any mailbox action
    uidvalidity?: number | null;
    messageId?: string | null;
    accountChip?: string | null; // display label for unified rows
    /** Absent for rows with no openable mailbox handle (snoozed records). */
    href?: string;
    /** Snoozed record: the row shows its exact return and offers Unsnooze. */
    snooze?: { id: string; returnAt: string; status: string } | null;
  };

  let {
    message,
    selection,
    orderedKeys,
    active = false,
    verbs = false,
    onfocus
  }: {
    message: Message;
    selection: SelectionStore;
    orderedKeys: string[];
    active?: boolean;
    /** Enable mailbox actions (row cluster + More actions menu). */
    verbs?: boolean;
    onfocus?: (key: string) => void;
  } = $props();

  const actions = getMessageActions();

  const isSelected = $derived(selection.isSelected(message.key));
  const hasHandle = $derived(verbs && !!message.folder && !message.snooze);
  const accountTint = $derived(message.accountChip ? identityColor(message.accountId) : null);

  const target = $derived<ActionTarget>({
    accountId: message.accountId,
    folder: message.folder ?? '',
    uid: message.uid,
    uidvalidity: message.uidvalidity ?? null,
    messageId: message.messageId ?? null,
    subject: message.subject
  });
  const rowState = $derived(message.folder ? actions.rowState(target) : undefined);
  const busy = $derived(!!message.folder && actions.isBusy(target));
  const flagged = $derived(
    message.folder ? actions.isFlagged(target, message.starred) : message.starred
  );
  const snoozeReturn = $derived(message.snooze ? new Date(message.snooze.returnAt) : null);
  // Read state from the shared store, so the read toggle's label is right the
  // moment the message is opened or toggled anywhere else.
  const isRead = $derived(
    !readState.isUnread(message.accountId, message.folder ?? '', message.uid, message.unread)
  );

  // ── Context menu (#172) ──────────────────────────────────────────────
  const contextMenu = getContextMenu();
  const menuOpen = $derived(contextMenu.current?.key === message.key ? contextMenu.current : null);
  // Null-safe: the menu's props can be read once more after the store closes,
  // before the {#if} below tears it down.
  const menuPoint = $derived(menuOpen ? { x: menuOpen.x, y: menuOpen.y } : null);
  const canMenu = $derived(hasHandle || (verbs && !!message.snooze));
  const scopeNote = $derived(
    isSelected && selection.count > 1
      ? `This message only. ${selection.count} selected: use the toolbar to act on all of them.`
      : null
  );
  let rowEl = $state<HTMLDivElement | null>(null);

  const LONG_PRESS_MS = 500;
  const PRESS_SLOP_PX = 10;
  let pressTimer: ReturnType<typeof setTimeout> | null = null;
  let pressAt = { x: 0, y: 0 };
  let swallowClick = false;

  function wantsNativeMenu(t: HTMLElement | null): boolean {
    if (!t || !rowEl) return true;
    if (t.closest('input, textarea, select, img, video, [contenteditable], .msg-actions-menu')) {
      return true;
    }
    const link = t.closest('a[href]');
    if (link && !link.classList.contains('msg-body')) return true;
    const sel = window.getSelection();
    if (sel && !sel.isCollapsed && sel.rangeCount > 0) {
      if (rowEl.contains(sel.getRangeAt(0).commonAncestorContainer)) return true;
    }
    return false;
  }

  function focusTarget(t: HTMLElement | null): HTMLElement | null {
    return t?.closest<HTMLElement>('.msg-body') ?? rowEl;
  }

  function openMenuAt(x: number, y: number, returnFocus: HTMLElement | null) {
    contextMenu.openAt({ key: message.key, x, y, returnFocus });
  }

  function openMenuFromElement(el: HTMLElement | null) {
    const r = (el ?? rowEl)?.getBoundingClientRect();
    openMenuAt(r ? r.left + 16 : 0, r ? r.bottom : 0, el ?? rowEl);
  }

  function closeContextMenu() {
    // Another row may already own the menu (retarget); only close our own.
    if (contextMenu.current?.key === message.key) contextMenu.close();
  }

  function handleContextMenu(e: MouseEvent) {
    if (!canMenu) return;
    const t = e.target as HTMLElement | null;
    if (wantsNativeMenu(t)) return;
    e.preventDefault();
    // Shift+F10 / the ContextMenu key also fire a contextmenu event with no
    // pointer position after the keydown already opened the menu.
    if (e.clientX === 0 && e.clientY === 0) {
      if (!menuOpen) openMenuFromElement(focusTarget(t));
      return;
    }
    openMenuAt(e.clientX, e.clientY, focusTarget(t));
  }

  function cancelPress() {
    if (pressTimer) clearTimeout(pressTimer);
    pressTimer = null;
  }

  function handlePointerDown(e: PointerEvent) {
    if (e.pointerType !== 'touch' || !canMenu) return;
    const t = e.target as HTMLElement | null;
    if (wantsNativeMenu(t)) return;
    pressAt = { x: e.clientX, y: e.clientY };
    cancelPress();
    pressTimer = setTimeout(() => {
      pressTimer = null;
      swallowClick = true;
      openMenuAt(pressAt.x, pressAt.y, focusTarget(t));
    }, LONG_PRESS_MS);
  }

  function handlePointerMove(e: PointerEvent) {
    if (!pressTimer) return;
    if (Math.hypot(e.clientX - pressAt.x, e.clientY - pressAt.y) > PRESS_SLOP_PX) cancelPress();
  }

  function handleClickCapture(e: MouseEvent) {
    // The click that ends a long-press must not open the message.
    if (!swallowClick) return;
    swallowClick = false;
    e.preventDefault();
    e.stopPropagation();
  }

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

  function act(command: ActionCommand) {
    if (!hasHandle || busy) return;
    void actions.dispatch(target, command);
  }

  function handleRowKeydown(e: KeyboardEvent) {
    if ((e.key === 'F10' && e.shiftKey) || e.key === 'ContextMenu') {
      const t = e.target as HTMLElement | null;
      if (!canMenu || (t && /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName))) return;
      e.preventDefault();
      openMenuFromElement(focusTarget(t));
      return;
    }
    if (e.key === 'x') {
      e.preventDefault();
      selection.keyToggle(message.key);
      return;
    }
    if (!hasHandle || busy) return;
    // Verb keys mirror the cluster. Ignore when typing in a field.
    const target = e.target as HTMLElement | null;
    if (target && /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName)) return;
    switch (e.key) {
      case 'e':
        e.preventDefault();
        act({ kind: 'archive' });
        break;
      case '#':
        e.preventDefault();
        act({ kind: 'trash' });
        break;
      case 'U':
        e.preventDefault();
        act({ kind: isRead ? 'mark-unread' : 'mark-read' });
        break;
    }
  }

  function toggleFlag(e: Event) {
    e.preventDefault();
    e.stopPropagation();
    act({ kind: flagged ? 'unflag' : 'flag' });
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
  class:has-tint={!!accountTint}
  class:is-busy={busy}
  style={accountTint ? `--account-tint: ${accountTint}` : undefined}
  role="row"
  data-msg-key={message.key}
  aria-selected={isSelected}
  aria-busy={busy}
  bind:this={rowEl}
  onkeydown={handleRowKeydown}
  onfocus={handleFocus}
  oncontextmenu={handleContextMenu}
  onpointerdown={handlePointerDown}
  onpointermove={handlePointerMove}
  onpointerup={cancelPress}
  onpointercancel={cancelPress}
  onclickcapture={handleClickCapture}
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

  <svelte:element
    this={message.href ? 'a' : 'div'}
    class="msg-body"
    class:is-unread={message.unread}
    href={message.href}
    tabindex={message.href ? 0 : undefined}
  >
    <div class="msg-line1">
      <span class="msg-sender">
        {#if message.unread}
          <span class="msg-sr">Unread.</span>
          <span class="msg-unread-dot" aria-hidden="true"></span>
        {/if}
        {#if flagged}
          <span class="msg-sr">Flagged.</span>
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
    {#if message.snooze && snoozeReturn}
      <p class="msg-snooze" class:is-overdue={message.snooze.status === 'overdue'}>
        {#if message.snooze.status === 'overdue'}
          Overdue: was due {formatExactReturn(snoozeReturn)}
        {:else}
          Returns {formatExactReturn(snoozeReturn)}
        {/if}
      </p>
    {:else if message.snippet}
      <p class="msg-snippet">{message.snippet}</p>
    {/if}
  </svelte:element>

  <div class="msg-tail">
    {#if hasHandle}
      <div class="msg-verbs" role="group" aria-label="Quick actions">
        {#if message.href}
          <a class="verb" href={message.href} aria-label="Reply" title="Reply (r)">
            <Icon name="reply" size={15} />
          </a>
        {/if}
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
          disabled={busy}
          aria-label="Move to Junk"
          title="Move to Junk"
          onclick={(e) => {
            e.preventDefault();
            e.stopPropagation();
            act({ kind: 'junk' });
          }}
        >
          <Icon name="ban" size={15} />
        </button>
        <button
          class="verb"
          type="button"
          disabled={busy}
          aria-label="Archive"
          title="Archive (e)"
          onclick={(e) => {
            e.preventDefault();
            e.stopPropagation();
            act({ kind: 'archive' });
          }}
        >
          <Icon name="archive" size={15} />
        </button>
        <button
          class="verb"
          type="button"
          disabled={busy}
          aria-label="Delete"
          title="Move to Trash (#)"
          onclick={(e) => {
            e.preventDefault();
            e.stopPropagation();
            act({ kind: 'trash' });
          }}
        >
          <Icon name="trash" size={15} />
        </button>
      </div>
    {/if}

    {#if hasHandle}
      <button
        class="msg-flag"
        class:is-flagged={flagged}
        type="button"
        aria-pressed={flagged}
        aria-label={flagged ? 'Unflag message' : 'Flag message'}
        title={flagged ? 'Unflag' : 'Flag'}
        disabled={busy}
        onclick={toggleFlag}
      >
        {flagged ? '★' : '☆'}
      </button>
      <MessageActionMenu
        {target}
        context={{ folder: target.folder, read: isRead, flagged }}
      />
    {:else if message.snooze}
      <MessageActionMenu
        {target}
        context={{ folder: target.folder, read: null, flagged: null, snoozeId: message.snooze.id }}
      />
    {:else if flagged}
      <span class="msg-flag is-flagged is-static" aria-hidden="true">★</span>
    {/if}
  </div>

  {#if menuOpen && canMenu}
    <MessageActionMenu
      {target}
      context={message.snooze
        ? { folder: target.folder, read: null, flagged: null, snoozeId: message.snooze.id }
        : { folder: target.folder, read: isRead, flagged }}
      at={menuPoint}
      openHref={message.href}
      {scopeNote}
      returnFocus={menuOpen?.returnFocus ?? null}
      onclose={closeContextMenu}
    />
  {/if}

  {#if rowState?.phase === 'pending'}
    <p class="msg-op-status" role="status">{rowState.label}</p>
  {:else if rowState?.phase === 'error'}
    <p class="msg-op-error" role="alert">
      Couldn't {rowState.kind.replace('-', ' ')}: {rowState.message}
      <button
        class="msg-op-dismiss"
        type="button"
        aria-label="Dismiss error"
        onclick={() => actions.clearRow(target)}
      >
        <Icon name="x" size={12} />
      </button>
    </p>
  {:else if rowState?.phase === 'done' && !rowState.receipt.removesRow}
    <p class="msg-op-status is-done" role="status">{rowState.receipt.text}</p>
  {/if}
</div>

<style>
  @media (pointer: coarse) {
    /* Long-press opens the app menu; keep the OS callout and text selection
       from fighting it. */
    .msg-row {
      -webkit-touch-callout: none;
      -webkit-user-select: none;
      user-select: none;
    }
  }
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
  /* Touch and phone: no hover, so the quick cluster would be invisible dead
     space. The always-visible flag + More actions carry every action. */
  @media (hover: none), (max-width: 640px) {
    .msg-verbs {
      display: none;
    }
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
  .msg-flag {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 28px;
    height: 28px;
    border: none;
    background: none;
    border-radius: var(--radius-sm, 3px);
    font-size: 0.9rem;
    color: var(--env-muted);
    cursor: pointer;
    user-select: none;
  }
  .msg-flag.is-flagged {
    color: var(--env-pending);
  }
  .msg-flag:hover:not(:disabled):not(.is-static) {
    color: var(--env-pending);
    background: var(--env-surface);
  }
  .msg-flag:focus-visible {
    outline: 2px solid color-mix(in srgb, var(--env-accent) 62%, white);
    outline-offset: 1px;
  }
  .msg-flag:disabled {
    cursor: progress;
    opacity: 0.5;
  }
  .msg-flag.is-static {
    cursor: default;
  }
  .msg-row.is-busy {
    opacity: 0.75;
  }
  .msg-snooze {
    margin: 0.2rem 0 0;
    font-size: 0.75rem;
    color: var(--env-muted);
    font-family: var(--font-mono);
  }
  .msg-snooze.is-overdue {
    color: var(--env-warn);
  }
  .msg-op-status {
    grid-column: 1 / -1;
    margin: 0.35rem 0.75rem 0.1rem 3.6rem;
    font-size: 0.75rem;
    color: var(--env-muted);
  }
  .msg-op-status.is-done {
    color: var(--env-ok, var(--env-muted));
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
