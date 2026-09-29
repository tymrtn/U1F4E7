<script lang="ts">
  // "More actions" for one message: a visible button (never hover-only) that
  // opens a menu listing exactly what `availableActions()` says can run, and
  // dispatches through the shared action model. Remind / Follow up appear
  // with the reason they are unavailable instead of as working actions.
  //
  // Keyboard: the trigger opens with Enter/Space/ArrowDown; arrows move,
  // Home/End jump, Escape closes and returns focus to the trigger, Tab closes.
  //
  // Context mode (#172): with `at` set there is no trigger. The row renders
  // this component only while its right-click menu is open, positioned at the
  // pointer and kept inside the viewport. It adds an Open link, can carry a
  // row-vs-selection note, and closes on Escape, outside click, another
  // right-click, scroll, or a chosen action; `onclose` tells the row.
  import {
    availableActions,
    getMessageActions,
    type ActionCommand,
    type ActionContext,
    type ActionDescriptor,
    type ActionOutcome,
    type ActionTarget,
    type CommandKind
  } from '$lib/message-actions.svelte';
  import { formatExactReturn, parseCustomSnooze, snoozeOptions, type SnoozeOption } from '$lib/snooze-options';
  import Icon from './Icon.svelte';

  let {
    target,
    context,
    label = 'More actions',
    onresult,
    at = null,
    openHref,
    scopeNote,
    returnFocus = null,
    onclose
  }: {
    target: ActionTarget;
    context: ActionContext;
    label?: string;
    /** Called after a dispatch settles (reader uses it to leave after a move). */
    onresult?: (kind: CommandKind, outcome: ActionOutcome) => void;
    /** Context mode: open at this viewport point, with no trigger button. */
    at?: { x: number; y: number } | null;
    /** Context mode: link for the Open item. */
    openHref?: string;
    /** Context mode: says whether the menu acts on the row or a selection. */
    scopeNote?: string | null;
    /** Context mode: focus goes back here on Escape. */
    returnFocus?: HTMLElement | null;
    onclose?: () => void;
  } = $props();

  const contextMode = $derived(at !== null);

  const actions = getMessageActions();
  const busy = $derived(actions.isBusy(target));
  const items = $derived(availableActions(context));

  // svelte-ignore state_referenced_locally
  let open = $state(at !== null);
  let view = $state<'main' | 'snooze'>('main');
  let presets = $state<SnoozeOption[]>([]);
  let customValue = $state('');
  let customError = $state<string | null>(null);
  let root = $state<HTMLDivElement | null>(null);
  let trigger = $state<HTMLButtonElement | null>(null);
  let menu = $state<HTMLDivElement | null>(null);

  const menuId = `msg-actions-${Math.random().toString(36).slice(2, 9)}`;

  function menuItems(): HTMLElement[] {
    return menu ? Array.from(menu.querySelectorAll<HTMLElement>('[role="menuitem"]')) : [];
  }

  function focusItem(index: number) {
    const list = menuItems();
    if (list.length === 0) return;
    // preventScroll: a focus-driven scroll would close a context menu.
    list[(index + list.length) % list.length].focus({ preventScroll: true });
  }

  async function openMenu() {
    if (busy) return;
    view = 'main';
    open = true;
    await Promise.resolve();
    focusItem(0);
  }

  function closeMenu(restoreFocus = true) {
    open = false;
    view = 'main';
    customError = null;
    if (contextMode) {
      if (restoreFocus) returnFocus?.focus();
      onclose?.();
      return;
    }
    if (restoreFocus) trigger?.focus();
  }

  // Context mode: focus the first item on open and keep the menu on screen.
  let pos = $state({ left: 0, top: 0 });
  function place() {
    if (!at) return;
    const r = menu?.getBoundingClientRect();
    const w = r?.width ?? 0;
    const h = r?.height ?? 0;
    const margin = 8;
    pos = {
      left: Math.max(margin, Math.min(at.x, window.innerWidth - w - margin)),
      top: Math.max(margin, Math.min(at.y, window.innerHeight - h - margin))
    };
  }
  $effect(() => {
    if (!at || !menu) return;
    place();
  });
  $effect(() => {
    if (!contextMode) return;
    void Promise.resolve().then(() => focusItem(0));
  });

  async function showSnooze() {
    presets = snoozeOptions(new Date());
    customValue = '';
    customError = null;
    view = 'snooze';
    await Promise.resolve();
    focusItem(0);
  }

  async function run(command: ActionCommand) {
    closeMenu();
    const outcome = await actions.dispatch(target, command);
    onresult?.(command.kind, outcome);
  }

  function pick(item: ActionDescriptor) {
    if (!item.available) return;
    switch (item.id) {
      case 'snooze':
        void showSnooze();
        return;
      case 'unsnooze':
        if (context.snoozeId) void run({ kind: 'unsnooze', snoozeId: context.snoozeId });
        return;
      case 'flag':
      case 'unflag':
      case 'mark-read':
      case 'mark-unread':
      case 'junk':
      case 'not-junk':
      case 'archive':
      case 'trash':
        void run({ kind: item.id });
        return;
    }
  }

  function snoozeCustom() {
    const at = parseCustomSnooze(customValue, new Date());
    if (!at) {
      customError = 'Pick a time in the future.';
      return;
    }
    void run({ kind: 'snooze', returnAt: at });
  }

  function onMenuKeydown(e: KeyboardEvent) {
    const list = menuItems();
    const i = list.indexOf(document.activeElement as HTMLElement);
    // The custom date field needs its own arrow keys.
    const inField = (e.target as HTMLElement | null)?.tagName === 'INPUT';
    if (inField && e.key !== 'Escape' && e.key !== 'Tab') {
      e.stopPropagation();
      return;
    }
    switch (e.key) {
      case 'ArrowDown':
        e.preventDefault();
        focusItem(i + 1);
        break;
      case 'ArrowUp':
        e.preventDefault();
        focusItem(i - 1);
        break;
      case 'Home':
        e.preventDefault();
        focusItem(0);
        break;
      case 'End':
        e.preventDefault();
        focusItem(list.length - 1);
        break;
      case 'Escape':
        e.preventDefault();
        e.stopPropagation();
        if (view === 'snooze') {
          view = 'main';
          void Promise.resolve().then(() => focusItem(0));
        } else {
          closeMenu();
        }
        break;
      case 'Tab':
        closeMenu(false);
        break;
    }
    // Row-level single-key shortcuts must not fire from inside the menu.
    e.stopPropagation();
  }

  function onTriggerKeydown(e: KeyboardEvent) {
    if (e.key === 'ArrowDown' && !open) {
      e.preventDefault();
      e.stopPropagation();
      void openMenu();
    }
  }

  function onWindowPointer(e: MouseEvent) {
    if (open && root && !root.contains(e.target as Node)) closeMenu(false);
  }

  // A right-click a row claimed (preventDefault) retargets through the row;
  // any other right-click closes the menu and leaves the browser's own.
  function onWindowContextMenu(e: MouseEvent) {
    if (contextMode && e.defaultPrevented) return;
    onWindowPointer(e);
  }

  function onWindowScroll() {
    if (contextMode && open) closeMenu(false);
  }
</script>

<svelte:window
  onclick={onWindowPointer}
  oncontextmenu={onWindowContextMenu}
  onresize={place}
  onscrollcapture={onWindowScroll}
/>

<div class="msg-actions" class:is-context={contextMode} bind:this={root}>
  {#if !contextMode}
  <button
    bind:this={trigger}
    class="msg-actions-trigger"
    type="button"
    aria-label={label}
    title={label}
    aria-haspopup="menu"
    aria-expanded={open}
    aria-controls={open ? menuId : undefined}
    aria-busy={busy}
    aria-disabled={busy}
    onclick={(e) => {
      e.preventDefault();
      e.stopPropagation();
      if (open) closeMenu();
      else void openMenu();
    }}
    onkeydown={onTriggerKeydown}
  >
    <Icon name="ellipsis" size={16} />
  </button>
  {/if}

  {#if open}
    <!-- svelte-ignore a11y_click_events_have_key_events -->
    <div
      bind:this={menu}
      id={menuId}
      class="msg-actions-menu"
      class:msg-context-menu={contextMode}
      style={contextMode ? `left: ${pos.left}px; top: ${pos.top}px` : undefined}
      role="menu"
      tabindex="-1"
      aria-label={view === 'snooze' ? 'Snooze until' : 'Message actions'}
      onkeydown={onMenuKeydown}
      onclick={(e) => e.stopPropagation()}
    >
      {#if view === 'main'}
        {#if contextMode && scopeNote}
          <p class="msg-actions-scope">{scopeNote}</p>
        {/if}
        {#if contextMode && openHref}
          <a
            role="menuitem"
            class="msg-actions-item"
            data-action="open"
            href={openHref}
            onclick={() => closeMenu(false)}
          >
            Open
          </a>
        {/if}
        {#each items as item (item.id)}
          {#if item.available}
            <button
              type="button"
              role="menuitem"
              class="msg-actions-item"
              data-action={item.id}
              onclick={() => pick(item)}
            >
              {item.label}
            </button>
          {:else}
            <div
              role="menuitem"
              class="msg-actions-item is-unavailable"
              data-action={item.id}
              aria-disabled="true"
              aria-describedby="{menuId}-{item.id}-why"
              tabindex="-1"
            >
              <span>{item.label}</span>
              <span class="msg-actions-why" id="{menuId}-{item.id}-why">{item.unavailableReason}</span>
            </div>
          {/if}
        {/each}
      {:else}
        {#each presets as opt (opt.key)}
          <button
            type="button"
            role="menuitem"
            class="msg-actions-item"
            data-snooze={opt.key}
            onclick={() => void run({ kind: 'snooze', returnAt: opt.at })}
          >
            <span>{opt.label}</span>
            <span class="msg-actions-when">{formatExactReturn(opt.at)}</span>
          </button>
        {/each}
        <div class="msg-actions-custom">
          <label class="msg-actions-custom-label" for="{menuId}-custom">Pick a date and time</label>
          <input
            id="{menuId}-custom"
            class="msg-actions-custom-input"
            type="datetime-local"
            bind:value={customValue}
            onkeydown={(e) => {
              if (e.key === 'Enter') {
                e.preventDefault();
                snoozeCustom();
              }
            }}
          />
          <button
            type="button"
            role="menuitem"
            class="msg-actions-item msg-actions-go"
            disabled={!customValue}
            onclick={snoozeCustom}
          >
            Snooze until then
          </button>
          {#if customError}
            <p class="msg-actions-err" role="alert">{customError}</p>
          {/if}
        </div>
        <button
          type="button"
          role="menuitem"
          class="msg-actions-item msg-actions-back"
          onclick={() => {
            view = 'main';
            void Promise.resolve().then(() => focusItem(0));
          }}
        >
          Back
        </button>
      {/if}
    </div>
  {/if}
</div>

<style>
  .msg-actions {
    position: relative;
    display: inline-flex;
  }
  .msg-actions-trigger {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 28px;
    height: 28px;
    border: 1px solid transparent;
    background: none;
    border-radius: var(--radius-sm, 3px);
    color: var(--env-muted);
    cursor: pointer;
  }
  .msg-actions-trigger:hover:not([aria-disabled='true']),
  .msg-actions-trigger[aria-expanded='true'] {
    background: var(--env-surface);
    border-color: var(--env-rule);
    color: var(--env-ink);
  }
  .msg-actions-trigger:focus-visible {
    outline: 2px solid color-mix(in srgb, var(--env-accent) 62%, white);
    outline-offset: 1px;
  }
  .msg-actions-trigger[aria-disabled='true'] {
    cursor: progress;
    opacity: 0.5;
  }
  .msg-actions-menu {
    position: absolute;
    top: calc(100% + 4px);
    right: 0;
    z-index: 40;
    width: 17rem;
    max-width: calc(100vw - 2rem);
    background: var(--env-surface);
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-md, 5px);
    box-shadow: 0 6px 20px rgba(10, 10, 10, 0.14);
    padding: 0.25rem;
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
  }
  .msg-actions-menu.msg-context-menu {
    position: fixed;
    right: auto;
    z-index: 60;
  }
  .msg-actions-scope {
    margin: 0;
    padding: 0.35rem 0.55rem 0.4rem;
    font-size: 0.6875rem;
    line-height: 1.35;
    color: var(--env-muted);
    border-bottom: 1px solid var(--env-rule);
  }
  a.msg-actions-item {
    text-decoration: none;
  }
  .msg-actions-item {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    flex-wrap: wrap;
    gap: 0.2rem 0.75rem;
    min-height: 36px;
    padding: 0.45rem 0.55rem;
    border: none;
    background: none;
    border-radius: var(--radius-sm, 3px);
    font: inherit;
    font-size: 0.8125rem;
    color: var(--env-ink);
    cursor: pointer;
    text-align: left;
  }
  .msg-actions-item:hover:not(.is-unavailable):not(:disabled),
  .msg-actions-item:focus-visible {
    background: var(--env-accent-soft);
    outline: none;
  }
  .msg-actions-item.is-unavailable {
    cursor: default;
    color: var(--env-muted);
    flex-direction: column;
    align-items: flex-start;
  }
  .msg-actions-item.is-unavailable:focus-visible {
    outline: 2px solid color-mix(in srgb, var(--env-accent) 50%, white);
  }
  .msg-actions-why {
    font-size: 0.6875rem;
    line-height: 1.35;
  }
  .msg-actions-when {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    color: var(--env-muted);
  }
  .msg-actions-custom {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    padding: 0.4rem 0.55rem;
    border-top: 1px solid var(--env-rule);
    margin-top: 0.15rem;
  }
  .msg-actions-custom-label {
    font-size: 0.6875rem;
    color: var(--env-muted);
  }
  .msg-actions-custom-input {
    font: inherit;
    font-size: 0.8125rem;
    padding: 0.3rem 0.4rem;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-sm, 3px);
    background: var(--env-paper);
    color: var(--env-ink);
  }
  .msg-actions-go:disabled {
    cursor: not-allowed;
    opacity: 0.5;
  }
  .msg-actions-back {
    color: var(--env-muted);
  }
  .msg-actions-err {
    margin: 0;
    font-size: 0.75rem;
    color: var(--env-warn);
  }
</style>
