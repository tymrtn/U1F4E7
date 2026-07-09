<script lang="ts">
  // Search bar for the message list header. Persists the query in the URL (?q=)
  // so links are shareable. Clearing restores the full box list. The parent
  // drives data fetching — this component only manages the input and URL sync.

  import { goto } from '$app/navigation';
  import { page } from '$app/state';

  let {
    hint = 'Search…',
    onsubmit,
    onreset,
  }: {
    hint?: string;
    onsubmit?: (q: string) => void;
    onreset?: () => void;
  } = $props();

  // Derive query from URL on mount and whenever the URL changes.
  const urlQuery = $derived(page.url.searchParams.get('q') ?? '');
  let inputValue = $state('');

  // Keep input in sync whenever the URL query param changes (e.g. back/forward).
  $effect(() => {
    inputValue = urlQuery;
  });

  function submit() {
    const q = inputValue.trim();
    if (!q) {
      reset();
      return;
    }
    const url = new URL(page.url.href);
    url.searchParams.set('q', q);
    goto(url.toString(), { replaceState: false, keepFocus: true });
    onsubmit?.(q);
  }

  function reset() {
    inputValue = '';
    const url = new URL(page.url.href);
    url.searchParams.delete('q');
    goto(url.toString(), { replaceState: false, keepFocus: true });
    onreset?.();
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter') {
      e.preventDefault();
      submit();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      reset();
    }
  }
</script>

<form class="search-bar" onsubmit={(e) => { e.preventDefault(); submit(); }}>
  <input
    id="search-input"
    class="search-input"
    type="search"
    bind:value={inputValue}
    placeholder={hint}
    aria-label="Search messages"
    onkeydown={handleKeydown}
    autocomplete="off"
    spellcheck={false}
  />
  {#if inputValue}
    <button type="button" class="search-clear" aria-label="Clear search" onclick={reset}>×</button>
  {:else}
    <span class="search-icon" aria-hidden="true">⌕</span>
  {/if}
</form>

<style>
  .search-bar {
    display: flex;
    align-items: center;
    gap: 0;
    background: var(--env-surface);
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-sm, 3px);
    flex: 1;
    min-width: 0;
  }
  .search-bar:focus-within {
    border-color: var(--env-accent);
    outline: 2px solid color-mix(in srgb, var(--env-accent) 20%, transparent);
    outline-offset: 1px;
  }
  .search-input {
    flex: 1;
    border: none;
    background: transparent;
    padding: 0.3rem 0.6rem;
    font-size: 0.8125rem;
    font-family: var(--font-sans);
    color: var(--env-ink);
    outline: none;
    min-width: 0;
  }
  .search-input::placeholder {
    color: var(--env-muted);
  }
  /* Remove the native clear button in webkit */
  .search-input::-webkit-search-cancel-button {
    display: none;
  }
  .search-clear,
  .search-icon {
    flex-shrink: 0;
    width: 28px;
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: 1rem;
    color: var(--env-muted);
    line-height: 1;
  }
  .search-clear {
    background: none;
    border: none;
    cursor: pointer;
    padding: 0;
  }
  .search-clear:hover {
    color: var(--env-ink);
  }
</style>
