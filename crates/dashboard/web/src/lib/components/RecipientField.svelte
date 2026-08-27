<script lang="ts">
  // Recipient token field with address autocomplete — the single To/Cc/Bcc
  // control for every compose surface.
  //
  // Contract with its parents: `value` stays an ordinary RFC5322 header string,
  // exactly what `api.compose` and the draft edit payload already carry. Chips
  // are a rendering of that string, not a new data shape, so nothing downstream
  // of the composer had to learn about them.
  //
  // Half-typed input is part of `value` on purpose. If the input held text the
  // parent could not see, an operator who typed a full address and hit Send
  // without pressing Enter would either lose that recipient or send while the
  // parent believed the field was empty. Keeping it in `value` means the
  // existing validateAddrs/optionalAddrsValid gates judge what is actually on
  // screen.
  //
  // Autocomplete is a convenience, never a gate: any valid address can be typed
  // and committed whether or not the address book has ever seen it.

  import { untrack } from 'svelte';
  import { addrKey, addrLabel, formatAddr, isValidEmail, parseAddrs } from '$lib/addresses';
  import { createSuggester, type Suggester } from '$lib/recipient-suggestions';
  import type { AddressSuggestion } from '$lib/api';

  let {
    id,
    label,
    value = $bindable(''),
    accountId = '',
    disabled = false,
    /** Addresses already used in the sibling fields — never offered again. */
    exclude = [],
    placeholder = '',
    invalid = false,
    limit = 8,
    /** Injectable for tests; production uses the shared fetcher. */
    suggester = createSuggester()
  }: {
    id: string;
    label: string;
    value?: string;
    accountId?: string;
    disabled?: boolean;
    exclude?: string[];
    placeholder?: string;
    invalid?: boolean;
    limit?: number;
    suggester?: Suggester;
  } = $props();

  const listId = $derived(`${id}-listbox`);

  let chips = $state<string[]>([]);
  let text = $state('');
  let open = $state(false);
  let activeIndex = $state(-1);
  let suggestions = $state<AddressSuggestion[]>([]);
  let loading = $state(false);
  let failed = $state(false);
  let input = $state<HTMLInputElement | null>(null);

  // Plain (non-reactive) mirrors of the last value/account this component read
  // or wrote. Without them the outbound write below re-triggers the inbound
  // sync and the two fight over the field on every keystroke.
  let synced = '';
  let syncedAccount = untrack(() => accountId);

  $effect(() => {
    if (value === synced) return;
    synced = value;
    chips = parseAddrs(value);
    text = '';
    close();
  });

  // Suggestions are scoped to one account server-side, so rows fetched for
  // account A are not offerable while composing from B. Closing the dropdown
  // does not discard them — ArrowDown would reopen the same list — so a From
  // change clears them outright and abandons anything still in flight.
  $effect(() => {
    if (accountId === syncedAccount) return;
    syncedAccount = accountId;
    untrack(() => {
      suggestions = [];
      loading = false;
      failed = false;
      close();
    });
  });

  /** Addresses that may not be added again, across this field and its siblings. */
  const taken = $derived(
    new Set([...chips.map(addrKey), ...exclude.map(addrKey)].filter(Boolean))
  );

  const visible = $derived(suggestions.filter((row) => !taken.has(row.email.toLowerCase())));

  /** Push chips + in-progress text back up as one header string. */
  function publish() {
    const next = [...chips, text.trim()].filter(Boolean).join(', ');
    synced = next;
    value = next;
  }

  function close() {
    open = false;
    activeIndex = -1;
    suggester.cancel();
  }

  function addChip(entry: string) {
    const key = addrKey(entry);
    if (key && !taken.has(key)) {
      chips = [...chips, entry];
    }
    text = '';
    publish();
    close();
  }

  function removeChip(index: number) {
    chips = chips.filter((_, i) => i !== index);
    publish();
    input?.focus();
  }

  /**
   * Commit every usable address in the input, each as its own chip.
   *
   * The input is not one address: a paste from a spreadsheet or another mail
   * client arrives as a whole recipient list, and treating it as a single
   * malformed entry would leave the operator to split it by hand. Anything
   * that is not a usable address stays in the input, visible to the parent's
   * validation gate — never silently dropped. Returns true when at least one
   * entry was consumed.
   */
  function commitTyped(): boolean {
    const entries = parseAddrs(text);
    if (entries.length === 0) return false;

    // Duplicate exclusion runs across this field and its siblings, and
    // accumulates within the paste itself.
    const keys = new Set(taken);
    const added: string[] = [];
    const leftover: string[] = [];
    let consumed = false;

    for (const entry of entries) {
      if (!isValidEmail(entry)) {
        leftover.push(entry);
        continue;
      }
      consumed = true;
      const key = addrKey(entry);
      if (key && !keys.has(key)) {
        keys.add(key);
        added.push(entry);
      }
    }

    if (!consumed) return false;
    chips = [...chips, ...added];
    text = leftover.join(', ');
    publish();
    close();
    return true;
  }

  /**
   * A pasted recipient list is committed on arrival rather than sitting in the
   * input as one unusable blob. Single addresses fall through to the browser's
   * own paste so the caret behaves normally.
   */
  function onPaste(event: ClipboardEvent) {
    const raw = event.clipboardData?.getData('text') ?? '';
    if (!/[,;\r\n]/.test(raw)) return;

    event.preventDefault();
    // Mail clients and spreadsheets separate recipients by line as readily as
    // by comma; a header value never does, so this normalizing is paste-local.
    const pasted = raw.replace(/[\r\n]+/g, ', ');
    const start = input?.selectionStart ?? text.length;
    const end = input?.selectionEnd ?? text.length;
    text = `${text.slice(0, start)}${pasted}${text.slice(end)}`;

    if (!commitTyped()) publish();
    void requestSuggestions(text);
  }

  function accept(row: AddressSuggestion) {
    addChip(formatAddr(row.email, row.name));
  }

  async function requestSuggestions(term: string) {
    const query = term.trim();
    if (!accountId || query === '') {
      suggestions = [];
      close();
      return;
    }

    const hit = suggester.cached(accountId, query);
    if (hit) {
      // Already answered this prefix — render it in the same frame so
      // backspacing through a word never flickers through a loading state.
      //
      // Cancel first. A cache hit is still a newer request than whatever is in
      // flight for the prefix before it; without this the older search stays
      // the suggester's latest sequence and repaints the dropdown when it
      // finally lands.
      suggester.cancel();
      apply(hit);
      return;
    }

    loading = true;
    failed = false;
    open = true;
    try {
      const rows = await suggester.search(accountId, query, limit);
      // `null` means a newer keystroke owns the dropdown. Touching anything
      // here — including `loading` — would repaint it with a stale prefix.
      if (rows === null) return;
      apply(rows);
    } catch {
      // The composer must stay usable when suggestions are unavailable, so
      // this reports rather than throws — and says so in the dropdown instead
      // of failing silently.
      failed = true;
      suggestions = [];
      loading = false;
      open = true;
    }
  }

  function apply(rows: AddressSuggestion[]) {
    suggestions = rows;
    failed = false;
    loading = false;
    open = true;
    // Index against what will actually render: a row already used in a sibling
    // field is filtered out, and pre-selecting it would arm Enter on nothing.
    activeIndex = rows.some((row) => !taken.has(row.email.toLowerCase())) ? 0 : -1;
  }

  function move(delta: number) {
    if (!open || visible.length === 0) return;
    const next = activeIndex + delta;
    activeIndex = next < 0 ? visible.length - 1 : next >= visible.length ? 0 : next;
  }

  function onInput(event: Event) {
    text = (event.currentTarget as HTMLInputElement).value;
    publish();
    void requestSuggestions(text);
  }

  function onKeydown(event: KeyboardEvent) {
    const active = open ? visible[activeIndex] : undefined;

    switch (event.key) {
      case 'ArrowDown':
        event.preventDefault();
        if (!open && visible.length > 0) open = true;
        move(1);
        return;
      case 'ArrowUp':
        event.preventDefault();
        if (!open && visible.length > 0) open = true;
        move(-1);
        return;
      case 'Escape':
        // Only swallow Escape while the dropdown owns it — otherwise the key
        // has to keep reaching the drawer that wraps this field.
        if (open) {
          event.preventDefault();
          event.stopPropagation();
          close();
        }
        return;
      case 'Enter':
        if (active) {
          event.preventDefault();
          accept(active);
          return;
        }
        if (text.trim() !== '') {
          // Never let a form submit ride on the same Enter that finishes an
          // address, valid or not.
          event.preventDefault();
          commitTyped();
        }
        return;
      case 'Tab':
        if (active) {
          event.preventDefault();
          accept(active);
          return;
        }
        // Commit on the way out, but let focus move — that is what Tab is for.
        commitTyped();
        return;
      case ',':
      case ';':
        if (commitTyped()) event.preventDefault();
        return;
      case 'Backspace':
        if (text === '' && chips.length > 0) {
          event.preventDefault();
          removeChip(chips.length - 1);
        }
        return;
    }
  }

  function onBlur() {
    // A committed-looking address left in the input is committed on the way
    // out; anything unfinished stays visible (and keeps the field invalid)
    // rather than vanishing.
    commitTyped();
    close();
  }
</script>

<div class="recipient-field" class:is-invalid={invalid} class:is-disabled={disabled}>
  <label class="recipient-label" for={id}>{label}</label>

  <div class="recipient-box">
    {#each chips as chip, index (`${chip}:${index}`)}
      <span class="recipient-chip" title={chip}>
        <span class="recipient-chip-label">{addrLabel(chip)}</span>
        <button
          type="button"
          class="recipient-chip-remove"
          aria-label={`Remove ${addrLabel(chip)}`}
          {disabled}
          onclick={() => removeChip(index)}>×</button
        >
      </span>
    {/each}

    <input
      {id}
      bind:this={input}
      class="recipient-input"
      type="text"
      inputmode="email"
      autocomplete="off"
      autocapitalize="off"
      spellcheck="false"
      role="combobox"
      aria-expanded={open}
      aria-controls={listId}
      aria-autocomplete="list"
      aria-activedescendant={activeIndex >= 0 && visible[activeIndex]
        ? `${listId}-option-${activeIndex}`
        : undefined}
      aria-invalid={invalid}
      placeholder={chips.length === 0 ? placeholder : ''}
      value={text}
      {disabled}
      oninput={onInput}
      onkeydown={onKeydown}
      onpaste={onPaste}
      onblur={onBlur}
    />
  </div>

  <div class="recipient-dropdown" class:is-open={open}>
    <!-- The listbox holds options and nothing else: a status line rendered as a
         child of role="listbox" is announced as a selectable choice. Notes live
         beside it in their own live region. -->
    <ul class="recipient-list" id={listId} role="listbox" aria-label={`${label} suggestions`}>
      {#if open}
        {#each visible as row, index (row.email)}
          <li
            class="recipient-option"
            class:is-active={index === activeIndex}
            id={`${listId}-option-${index}`}
            role="option"
            aria-selected={index === activeIndex}
            onmousedown={(event) => {
              // Take the pointer before blur can close the list out from under
              // the click.
              event.preventDefault();
              accept(row);
            }}
            onmouseenter={() => (activeIndex = index)}
          >
            <span class="recipient-option-name">{row.name ?? row.email}</span>
            {#if row.name}<span class="recipient-option-email">{row.email}</span>{/if}
          </li>
        {/each}
      {/if}
    </ul>

    {#if open && visible.length === 0}
      <p class="recipient-note" class:is-warn={failed} role="status">
        {#if loading}
          Searching contacts…
        {:else if failed}
          Suggestions unavailable — you can still type an address.
        {:else}
          No saved contacts match. Type a full address to add it.
        {/if}
      </p>
    {/if}
  </div>
</div>

<style>
  .recipient-field {
    position: relative;
    min-height: 2.625rem;
    display: grid;
    grid-template-columns: 4.75rem minmax(0, 1fr);
    align-items: start;
    gap: 0.75rem;
    border-bottom: 1px solid var(--env-rule);
  }
  .recipient-label {
    margin: 0;
    padding-top: 0.7rem;
    color: var(--env-muted);
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    letter-spacing: 0.12em;
    text-transform: uppercase;
  }
  .recipient-box {
    min-width: 0;
    min-height: 2.625rem;
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.3rem;
    padding: 0.35rem 0;
  }
  .recipient-field:focus-within {
    box-shadow: inset 2px 0 0 var(--env-accent);
  }
  .recipient-field.is-invalid {
    box-shadow: inset 2px 0 0 var(--env-warn);
  }

  /* ── Chips ── */
  .recipient-chip {
    max-width: 100%;
    min-height: 1.75rem;
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    padding: 0 0.15rem 0 0.45rem;
    border: 1px solid var(--env-rule);
    background: var(--env-paper);
    color: var(--env-ink);
    font-family: var(--font-mono);
    font-size: 0.75rem;
  }
  .recipient-chip-label {
    overflow: hidden;
    max-width: 16rem;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .recipient-chip-remove {
    min-width: 1.5rem;
    min-height: 1.5rem;
    border: 0;
    background: transparent;
    color: var(--env-muted);
    cursor: pointer;
    font-size: 0.9375rem;
    line-height: 1;
  }
  .recipient-chip-remove:hover:not(:disabled) {
    color: var(--env-warn);
  }
  .recipient-chip-remove:focus-visible {
    outline: 2px solid var(--env-accent);
    outline-offset: -2px;
  }

  /* ── Input ── */
  .recipient-input {
    flex: 1 1 8rem;
    min-width: 6rem;
    min-height: 1.75rem;
    border: 0;
    outline: 0;
    background: transparent;
    color: var(--env-ink);
    font-family: var(--font-mono);
    font-size: 0.8125rem;
  }
  .recipient-input::placeholder {
    color: #aaa69e;
  }
  .recipient-input:disabled {
    color: var(--env-muted);
    -webkit-text-fill-color: var(--env-muted);
    opacity: 1;
  }

  /* ── Listbox ── */
  .recipient-dropdown {
    position: absolute;
    z-index: 20;
    top: 100%;
    left: 4.75rem;
    right: 0;
    max-height: 17rem;
    overflow-y: auto;
    border: 1px solid var(--env-rule);
    background: var(--env-surface);
    box-shadow: 0 6px 18px rgb(0 0 0 / 12%);
  }
  .recipient-dropdown:not(.is-open) {
    display: none;
  }
  .recipient-list {
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .recipient-option {
    min-height: 2.5rem;
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.4rem 0.65rem;
    cursor: pointer;
  }
  .recipient-option.is-active {
    background: var(--env-accent-soft);
    box-shadow: inset 2px 0 0 var(--env-accent);
  }
  .recipient-option-name {
    overflow: hidden;
    color: var(--env-ink);
    font-size: 0.8125rem;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .recipient-option-email {
    overflow: hidden;
    margin-left: auto;
    color: var(--env-muted);
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .recipient-note {
    margin: 0;
    padding: 0.55rem 0.65rem;
    color: var(--env-muted);
    font-size: 0.75rem;
  }
  .recipient-note.is-warn {
    color: var(--env-warn);
  }

  @media (max-width: 760px) {
    .recipient-field {
      grid-template-columns: 3.75rem minmax(0, 1fr);
      gap: 0.5rem;
    }
    .recipient-dropdown {
      left: 0;
    }
    /* Comfortable touch targets: options and the chip remove button both clear
       the 44px guidance on a phone. */
    .recipient-option {
      min-height: 2.875rem;
    }
    .recipient-chip {
      min-height: 2rem;
    }
    .recipient-chip-remove {
      min-width: 1.875rem;
      min-height: 1.875rem;
    }
  }
</style>
