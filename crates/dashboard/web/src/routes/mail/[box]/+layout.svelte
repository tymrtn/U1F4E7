<script lang="ts">
  // v2 mailbox layout: rail + message list (selection, search, bulk ops) + reader outlet.
  // The list stays mounted while the reader (nested [account]/[uid] route) swaps the third column.
  // This layout also owns: SSE live-update wiring, Composer drawer launch points (header button
  // + keyboard 'c'), UndoToast for queued sends, and the rail-footer connection indicator.
  import { base } from '$app/paths';
  import { page } from '$app/state';
  import { onMount } from 'svelte';
  import type { Snippet } from 'svelte';
  import { Rail, Spinner, EmptyState, MonoTag } from '$lib/components';
  import MessageRow from '$lib/components/MessageRow.svelte';
  import BulkToolbar from '$lib/components/BulkToolbar.svelte';
  import SearchBar from '$lib/components/SearchBar.svelte';
  import ComposerDrawer from '$lib/components/ComposerDrawer.svelte';
  import UndoToast from '$lib/components/UndoToast.svelte';
  import { mailboxBySlug } from '$lib/mailboxes';
  import { unifiedNeedsRefresh, positionOf } from '$lib/mailbox-position';
  import { folderHints } from '$lib/folder-hints.svelte';
  import { SelectionStore } from '$lib/selection.svelte';
  import { readState } from '$lib/read-state.svelte';
  import { getLiveStore } from '$lib/live.svelte';
  import { getComposerStore } from '$lib/composer.svelte';
  import { getMailboxOpsStore } from '$lib/mailbox-ops.svelte';
  import {
    api,
    EnvelopeApiError,
    type UnifiedInboxMessage,
    type UnifiedInboxError,
    type Draft,
    type SnoozedItem,
    type SearchMessageSummary,
    type FolderStats,
    type ComposeResponse,
    type Account,
  } from '$lib/api';

  let { children }: { children: Snippet } = $props();

  const box = $derived(mailboxBySlug(page.params.box ?? 'unified'));
  const selectedUid = $derived(page.params.uid ? Number(page.params.uid) : null);
  const selectedAccount = $derived(page.params.account ?? null);

  const selection = new SelectionStore();

  // ── Mailbox-ops signal ────────────────────────────────────────────
  // The reader (nested route, no prop channel) announces archive / trash /
  // delete / star through this shared store. Re-run the same refresh
  // BulkToolbar triggers via `onoperated`, so the moved row disappears from the
  // mounted list. Version-compare so a route change alone never re-fetches.
  const mailboxOps = getMailboxOpsStore();
  let seenOpsVersion = 0;
  $effect(() => {
    const v = mailboxOps.version;
    if (v === seenOpsVersion) return;
    seenOpsVersion = v;
    void handleOperated();
  });

  // ── SSE live store ────────────────────────────────────────────────
  // Started once on mount (onMount guard prevents SSR). When the stream is
  // open, the live ticks replace the polling timer. When degraded, polling
  // continues as before (all existing fetch paths stay intact).
  let live = $state<ReturnType<typeof getLiveStore> | null>(null);
  let pollTimer: ReturnType<typeof setInterval> | null = null;

  // ── Accounts cache (for composer from-select) ─────────────────────
  let allAccounts = $state<Account[]>([]);

  // ── Composer store ────────────────────────────────────────────────
  const composer = getComposerStore();

  // ── Undo toast ────────────────────────────────────────────────────
  let undoToast = $state<{ res: ComposeResponse; accountId: string } | null>(null);

  let unifiedMessages = $state<UnifiedInboxMessage[]>([]);
  let drafts = $state<Draft[]>([]);
  let snoozed = $state<SnoozedItem[]>([]);
  let listErrors = $state<UnifiedInboxError[]>([]);
  let loading = $state(false);
  let error = $state<{ code: string; message: string } | null>(null);
  let loadedBox = $state<string | null>(null);
  let folders = $state<FolderStats[]>([]);

  /** A search hit tagged with the account and folder it was actually found in
   *  — the per-account search endpoint returns neither, so both are attached
   *  here as results are merged. Bulk actions and navigation both need the real
   *  identity, never a placeholder. */
  type SearchHit = SearchMessageSummary & { account_id: string; folder: string };

  /** The mailbox `runSearch` searches. Tagged onto every hit so links and bulk
   *  dispatch name the folder the UIDs are actually scoped to, rather than
   *  assuming INBOX at each use site. */
  const SEARCH_FOLDER = 'INBOX';

  /** BulkToolbar's `messageIndex` entry shape — the message's real identity
   *  (account/uid) plus the context junk-rules/snooze need. */
  type MsgIndexEntry = {
    accountId: string;
    uid: number;
    from: string;
    folder: string;
    message_id: string | null;
    subject: string | null;
  };

  const searchQuery = $derived(page.url.searchParams.get('q') ?? '');
  let searchResults = $state<SearchHit[]>([]);
  let searching = $state(false);
  let searchError = $state<string | null>(null);
  const isSearching = $derived(searchQuery.length > 0);

  let starOverrides = $state<Map<string, boolean>>(new Map());
  let unifiedLoadGen = 0;
  let unifiedRefreshInFlight = false;

  // ── Where the open message sits in the list ───────────────────────
  // The unified list is a flat merge of every account, so "which one am I
  // reading, and where is it?" is not answerable from the reader alone. This
  // drives both the "4 of 50" readout and the scroll-into-view below; it is
  // null when the deep link names a message older than the loaded page.
  const selectedPosition = $derived(
    box?.slug === 'unified' && !isSearching
      ? positionOf(unifiedMessages, selectedAccount, selectedUid)
      : null
  );

  // Bring the open message into view when a deep link lands on a row that is
  // scrolled out of sight. Keyed on the row's identity, so re-running the
  // effect for an unrelated list update does not yank the viewport around.
  let lastScrolledKey: string | null = null;
  $effect(() => {
    const key =
      selectedAccount !== null && selectedUid !== null
        ? `${selectedAccount}:${selectedUid}`
        : null;
    if (!key || selectedPosition === null) {
      if (key === null) lastScrolledKey = null;
      return;
    }
    if (key === lastScrolledKey) return;
    const row = document.querySelector(
      `#unified-msg-list [data-msg-key="${CSS.escape(key)}"]`
    );
    if (!row) return;
    lastScrolledKey = key;
    row.scrollIntoView({ block: 'nearest' });
  });

  function applyUnified(res: { messages: UnifiedInboxMessage[]; errors?: UnifiedInboxError[] }) {
    unifiedMessages = res.messages;
    listErrors = res.errors ?? [];
    // Record the mailbox each row came from, so a reader link that lost its
    // `?folder=` can still resolve one instead of guessing INBOX.
    folderHints.remember(res.messages);
  }

  async function refreshStaleUnified(gen: number) {
    if (unifiedRefreshInFlight) return;
    unifiedRefreshInFlight = true;
    try {
      const refreshed = await api.refreshUnifiedInbox(50);
      if (gen !== unifiedLoadGen) return;
      applyUnified(refreshed);
    } catch {
      // Keep the painted cache. A failed refresh must not blank the list.
    } finally {
      unifiedRefreshInFlight = false;
    }
  }

  async function loadUnified() {
    const gen = ++unifiedLoadGen;
    loading = unifiedMessages.length === 0;
    error = null;
    try {
      const res = await api.unifiedInbox(50);
      if (gen !== unifiedLoadGen) return;
      applyUnified(res);
      loading = false;
      if (unifiedNeedsRefresh(res)) {
        await refreshStaleUnified(gen);
      }
    } catch (e) {
      if (gen !== unifiedLoadGen) return;
      const err = e as EnvelopeApiError;
      error = { code: err.code ?? 'unknown', message: err.message ?? 'Failed to load messages.' };
    } finally {
      if (gen === unifiedLoadGen) loading = false;
    }
  }

  async function loadDrafts() {
    loading = true;
    error = null;
    try {
      const { accounts } = await api.listAccounts();
      const allDrafts: Draft[] = [];
      await Promise.all(
        accounts.map(async (acct) => {
          try {
            const res = await api.drafts(acct.id);
            allDrafts.push(...res.drafts);
          } catch {
            // best-effort
          }
        })
      );
      drafts = allDrafts;
    } catch (e) {
      const err = e as EnvelopeApiError;
      error = { code: err.code ?? 'unknown', message: err.message ?? 'Failed to load drafts.' };
    } finally {
      loading = false;
    }
  }

  async function loadSnoozed() {
    loading = true;
    error = null;
    try {
      const { accounts } = await api.listAccounts();
      const allSnoozed: SnoozedItem[] = [];
      await Promise.all(
        accounts.map(async (acct) => {
          try {
            const res = await api.snoozed(acct.id);
            allSnoozed.push(...res.snoozed);
          } catch {
            // best-effort
          }
        })
      );
      snoozed = allSnoozed;
    } catch (e) {
      const err = e as EnvelopeApiError;
      error = { code: err.code ?? 'unknown', message: err.message ?? 'Failed to load snoozed.' };
    } finally {
      loading = false;
    }
  }

  async function loadFolders() {
    try {
      const { accounts } = await api.listAccounts();
      if (accounts.length === 0) return;
      const res = await api.folders(accounts[0].id);
      folders = res.folders ?? [];
    } catch {
      // non-fatal
    }
  }

  $effect(() => {
    const slug = page.params.box ?? 'unified';
    if (box?.wired && loadedBox !== slug) {
      loadedBox = slug;
      selection.clear();
      starOverrides = new Map();
      if (slug === 'unified') {
        loadUnified();
        loadFolders();
      } else if (slug === 'drafts') {
        loadDrafts();
      } else if (slug === 'snoozed') {
        loadSnoozed();
      }
    }
  });

  $effect(() => {
    const q = searchQuery;
    // A search query change swaps the result set under any selection —
    // stale hidden selections from the prior list/query must never persist
    // to act against messages the operator can no longer see.
    selection.clear();
    if (!q) {
      searchResults = [];
      searchError = null;
      return;
    }
    runSearch(q);
  });

  async function runSearch(q: string) {
    searching = true;
    searchError = null;
    try {
      const { accounts } = await api.listAccounts();
      const merged: SearchHit[] = [];
      await Promise.all(
        accounts.map(async (acct) => {
          try {
            const res = await api.searchMessages(acct.id, q, SEARCH_FOLDER);
            // Tag each hit with the account and folder it actually came from —
            // the per-account search response carries neither, and a search
            // spanning accounts must not blur which account owns which hit.
            merged.push(
              ...res.messages.map((m) => ({ ...m, account_id: acct.id, folder: SEARCH_FOLDER }))
            );
          } catch {
            // partial ok
          }
        })
      );
      searchResults = merged;
    } catch (e) {
      const err = e as EnvelopeApiError;
      searchError = err.message ?? 'Search failed.';
    } finally {
      searching = false;
    }
  }

  async function handleStar(uid: number, accountId: string, star: boolean) {
    const key = `${accountId}:${uid}`;
    const prev = starOverrides.has(key)
      ? starOverrides.get(key)!
      : (unifiedMessages.find((m) => m.uid === uid && m.account_id === accountId)?.flags ?? [])
          .some((f) => f.toLowerCase().includes('flagged'));
    starOverrides = new Map(starOverrides).set(key, star);
    try {
      await api.messageFlags(accountId, uid, {
        folder: 'INBOX',
        add: star ? ['\\Flagged'] : [],
        remove: star ? [] : ['\\Flagged'],
      });
    } catch {
      starOverrides = new Map(starOverrides).set(key, prev);
    }
  }

  function isStarred(uid: number, accountId: string, flags: string[]): boolean {
    const key = `${accountId}:${uid}`;
    if (starOverrides.has(key)) return starOverrides.get(key)!;
    return flags.some((f) => f.toLowerCase().includes('flagged'));
  }

  function senderLabel(m: UnifiedInboxMessage): string {
    return m.from_addr || m.account_username;
  }

  const orderedUnifiedKeys = $derived(
    unifiedMessages.map((m) => `${m.account_id}:${m.uid}`)
  );
  const orderedSearchKeys = $derived(
    searchResults.map((m) => `search:${m.account_id}:${m.uid}`)
  );

  const currentFolder = $derived(
    page.params.box === 'unified' ? 'INBOX' : 'INBOX'
  );
  // Retry payloads are valid only inside the mailbox/search context that
  // created them. Loading within the same context keeps the component mounted;
  // changing mailbox or query remounts it and drops stale toasts/caches.
  const toolbarContextKey = $derived(`${page.params.box ?? 'unified'}\0${searchQuery}`);

  // Per-message context the toolbar needs for truthful junk-rules (exact
  // sender), snooze (message-id/subject for the round-trip), and per-item folder
  // dispatch. The unified inbox carries each row's real source folder, so use it
  // rather than assuming the route folder — a unified surface can span mailboxes.
  const unifiedMessageIndex = $derived.by(() => {
    const idx: Record<string, MsgIndexEntry> = {};
    for (const m of unifiedMessages) {
      idx[`${m.account_id}:${m.uid}`] = {
        accountId: m.account_id,
        uid: m.uid,
        from: m.from_addr ?? '',
        folder: m.folder ?? 'INBOX',
        message_id: m.message_id ?? null,
        subject: m.subject ?? null,
      };
    }
    return idx;
  });

  /** Search hits carry their own real account and the folder the search ran
   *  against (both tagged in `runSearch`): search fans out across every account
   *  while staying inside `SEARCH_FOLDER`, so bulk actions must dispatch against
   *  each hit's actual identity. */
  const searchMessageIndex = $derived.by(() => {
    const idx: Record<string, MsgIndexEntry> = {};
    for (const m of searchResults) {
      idx[`search:${m.account_id}:${m.uid}`] = {
        accountId: m.account_id,
        uid: m.uid,
        from: m.from_addr ?? '',
        folder: m.folder,
        message_id: m.message_id ?? null,
        subject: m.subject ?? null,
      };
    }
    return idx;
  });

  /** A snoozed message physically resides in `snoozed_folder` until the sweep
   *  returns it — bulk actions (archive/flag/move/etc.) must target that real
   *  current location, not `original_folder` (where it isn't right now). */
  const snoozedMessageIndex = $derived.by(() => {
    const idx: Record<string, MsgIndexEntry> = {};
    for (const s of snoozed) {
      idx[`snoozed:${s.account_id}:${s.uid}`] = {
        accountId: s.account_id,
        uid: s.uid,
        from: s.from_addr ?? '',
        folder: s.snoozed_folder,
        message_id: s.message_id ?? null,
        subject: s.subject ?? null,
      };
    }
    return idx;
  });

  async function handleOperated() {
    const slug = page.params.box ?? 'unified';
    if (slug === 'unified') await loadUnified();
    else if (slug === 'drafts') await loadDrafts();
    else if (slug === 'snoozed') await loadSnoozed();
  }

  // ── Accounts load (for composer from-select) ──────────────────────
  async function loadAccounts() {
    try {
      const res = await api.listAccounts();
      allAccounts = res.accounts;
    } catch {
      // non-fatal; composer gracefully shows empty select
    }
  }

  // ── Composer helpers ──────────────────────────────────────────────
  function openCompose() {
    // Open with first account as default; user can change via the select.
    composer.open('compose', { accountId: allAccounts[0]?.id ?? '' });
  }

  function handleGlobalKey(e: KeyboardEvent) {
    // 'c' opens compose unless an input/textarea/select is focused.
    if (e.key === 'c' || e.key === 'C') {
      const tag = (document.activeElement?.tagName ?? '').toLowerCase();
      if (tag === 'input' || tag === 'textarea' || tag === 'select') return;
      e.preventDefault();
      openCompose();
    }
  }

  // ── Send / undo toast ─────────────────────────────────────────────
  function handleSent(res: ComposeResponse, fromAccountId: string) {
    // Only show undo if the cooldown is meaningful (> 0 seconds).
    if (res.cooldown_seconds > 0) {
      undoToast = { res, accountId: fromAccountId };
    }
    // Refresh the list so a queued draft appears in /drafts.
    const slug = page.params.box ?? 'unified';
    if (slug === 'drafts') loadDrafts();
  }

  // ── SSE wiring ────────────────────────────────────────────────────
  // Guards with onMount so the SSE client never opens during SSR or unit tests
  // that do not call onMount.
  onMount(() => {
    // Load accounts for the composer from-select.
    loadAccounts();

    // Start the shared live store (idempotent). Guard against jsdom / SSR
    // environments where EventSource is not defined.
    let offNewMail: (() => void) | null = null;
    let offLagged: (() => void) | null = null;

    if (typeof EventSource !== 'undefined') {
      live = getLiveStore();

      // When the stream is live and a new_mail event arrives, refresh the
      // current box. When degraded, the existing polling paths take over.
      offNewMail = live.on(['new_mail'], () => {
        if (!live?.degraded) {
          const slug = page.params.box ?? 'unified';
          if (slug === 'unified') loadUnified();
        }
      });

      // Server said we fell behind the event stream: re-poll for exactness.
      offLagged = live.onLagged(() => {
        const slug = page.params.box ?? 'unified';
        if (slug === 'unified') loadUnified();
        else if (slug === 'drafts') loadDrafts();
        else if (slug === 'snoozed') loadSnoozed();
      });
    }

    return () => {
      offNewMail?.();
      offLagged?.();
      if (pollTimer !== null) {
        clearInterval(pollTimer);
        pollTimer = null;
      }
    };
  });

  // Reactive effect: when laggedTicks increments (lagged control frame
  // arrived), refresh unified inbox for exactness.
  $effect(() => {
    if (live && live.laggedTicks > 0) {
      const slug = page.params.box ?? 'unified';
      if (slug === 'unified') loadUnified();
    }
  });

  // Connection state for the rail footer indicator.
  const connectionState = $derived(live?.connection ?? 'closed');
  const isDegraded = $derived(live?.degraded ?? false);
</script>

<svelte:window onkeydown={handleGlobalKey} />

<div class="mail-shell" class:is-reading={selectedUid !== null}>
  <Rail activeAccountId={selectedAccount} />

  <section id="msg-list-pane" class="list" aria-label="Message list">
    <header class="pane-head">
      <span class="pane-title">{box?.label ?? 'Mailbox'}</span>
      <div class="pane-head-right">
        {#if box?.wired}
          <span class="pane-count">
            {#if selectedPosition}
              <MonoTag>{selectedPosition.position} of {selectedPosition.total}</MonoTag>
            {:else}
              <MonoTag>{isSearching ? searchResults.length : (box.slug === 'unified' ? unifiedMessages.length : box.slug === 'drafts' ? drafts.length : snoozed.length)}</MonoTag>
            {/if}
          </span>
          <SearchBar
            hint="Search {box.label}…"
            onsubmit={(q) => runSearch(q)}
            onreset={() => { searchResults = []; searchError = null; }}
          />
        {/if}
        <button
          id="compose-btn"
          class="compose-btn"
          type="button"
          aria-label="Compose new message"
          title="Compose (c)"
          onclick={openCompose}
        >Compose</button>
      </div>
    </header>

    <!-- Connection indicator (rail footer) -->
    <div id="live-indicator" class="live-indicator" aria-label="Connection status">
      {#if connectionState === 'open' && !isDegraded}
        <span class="live-dot live-dot-ok" aria-hidden="true"></span>
        <span class="live-label">Live</span>
      {:else if isDegraded}
        <span class="live-dot live-dot-degraded" aria-hidden="true"></span>
        <span class="live-label">Polling</span>
      {:else if connectionState === 'connecting' || connectionState === 'reconnecting'}
        <span class="live-dot live-dot-pending" aria-hidden="true"></span>
        <span class="live-label">Connecting</span>
      {/if}
    </div>

    <!-- Drafts have no real IMAP identity (no account/folder/UID — they live
         in the drafts store, not a mailbox), so mailbox bulk actions (move,
         flag, junk, delete…) are never exposed there. Search and snoozed DO
         have a real identity (via searchMessageIndex/snoozedMessageIndex
         below) and get the toolbar like the unified inbox. -->
    {#if box?.wired && box.slug !== 'drafts'}
      {#key toolbarContextKey}
        <BulkToolbar
          {selection}
          folder={currentFolder}
          {folders}
          messageIndex={isSearching
            ? searchMessageIndex
            : box.slug === 'snoozed'
              ? snoozedMessageIndex
              : unifiedMessageIndex}
          onoperated={handleOperated}
          {loading}
        />
      {/key}
    {/if}

    {#if !box}
      <EmptyState title="Unknown mailbox" hint="This mailbox slug isn't recognized." />
    {:else if !box.wired}
      <EmptyState
        title="{box.label} has no messages to show here"
        hint="This smart mailbox doesn't load its own list yet. Unified Inbox has your mail."
      >
        {#snippet action()}
          <a class="empty-link" href="{base}/mail/unified">Go to Unified Inbox</a>
        {/snippet}
      </EmptyState>
    {:else if loading}
      <div class="list-loading"><Spinner label="Loading messages" /> <span>Loading messages…</span></div>
    {:else if error}
      <div class="list-error" role="alert">
        <p class="list-error-msg">Couldn't load messages.</p>
        <p class="list-error-detail">{error.message}</p>
        <p><MonoTag>{error.code}</MonoTag></p>
        <button class="list-retry" type="button" onclick={() => {
          const slug = page.params.box ?? 'unified';
          if (slug === 'unified') loadUnified();
          else if (slug === 'drafts') loadDrafts();
          else if (slug === 'snoozed') loadSnoozed();
        }}>Retry</button>
      </div>

    {:else if isSearching}
      {#if searching}
        <div class="list-loading"><Spinner label="Searching" /> <span>Searching…</span></div>
      {:else if searchError}
        <div class="list-error" role="alert">
          <p class="list-error-msg">Search failed.</p>
          <p class="list-error-detail">{searchError}</p>
        </div>
      {:else if searchResults.length === 0}
        <EmptyState title="No results" hint="No messages matched your search." />
      {:else}
        <ul id="search-results-list" class="msg-list">
          {#each searchResults as m (`search:${m.account_id}:${m.uid}`)}
            {@const key = `search:${m.account_id}:${m.uid}`}
            <li>
              <MessageRow
                message={{
                  key,
                  uid: m.uid,
                  accountId: m.account_id,
                  subject: m.subject,
                  from: m.from_addr,
                  date: m.date,
                  snippet: null,
                  unread: readState.isUnread(m.account_id, m.folder, m.uid, m.unread),
                  starred: isStarred(m.uid, m.account_id, m.flags),
                  href: `${base}/mail/unified/${encodeURIComponent(m.account_id)}/${m.uid}?folder=${encodeURIComponent(m.folder)}`,
                }}
                {selection}
                orderedKeys={orderedSearchKeys}
              />
            </li>
          {/each}
        </ul>
      {/if}

    {:else if box.slug === 'unified'}
      {#if listErrors.length > 0}
        <p class="list-partial" role="status">
          {listErrors.length} account(s) couldn't be reached; showing what loaded.
        </p>
      {/if}
      {#if unifiedMessages.length === 0}
        <EmptyState
          title="Inbox is empty"
          hint="No messages across your connected accounts. New mail appears here."
        />
      {:else}
        <ul id="unified-msg-list" class="msg-list">
          {#each unifiedMessages as m (`${m.account_id}:${m.uid}`)}
            {@const key = `${m.account_id}:${m.uid}`}
            {@const active = selectedUid === m.uid && selectedAccount === m.account_id}
            <li>
              <MessageRow
                message={{
                  key,
                  uid: m.uid,
                  accountId: m.account_id,
                  subject: m.subject,
                  from: senderLabel(m),
                  date: m.date,
                  snippet: m.snippet,
                  unread: readState.isUnread(m.account_id, m.folder, m.uid, m.unread),
                  starred: isStarred(m.uid, m.account_id, m.flags),
                  accountChip: m.account_display_name || m.account_username,
                  href: `${base}/mail/unified/${encodeURIComponent(m.account_id)}/${m.uid}?folder=${encodeURIComponent(m.folder)}`,
                }}
                {selection}
                orderedKeys={orderedUnifiedKeys}
                {active}
                onstar={handleStar}
              />
            </li>
          {/each}
        </ul>
      {/if}

    {:else if box.slug === 'drafts'}
      {#if drafts.length === 0}
        <EmptyState title="No drafts" hint="Drafts waiting to be sent appear here." />
      {:else}
        <ul id="drafts-msg-list" class="msg-list">
          {#each drafts as d (d.id)}
            {@const key = `draft:${d.account_id}:${d.id}`}
            <li>
              <MessageRow
                message={{
                  key,
                  uid: d.imap_uid ?? 0,
                  accountId: d.account_id,
                  subject: d.subject ?? '(no subject)',
                  from: d.to_addr,
                  date: d.created_at,
                  snippet: d.text_content ? d.text_content.slice(0, 80) : null,
                  unread: false,
                  starred: false,
                  accountChip: d.account_id,
                  href: `${base}/accounts/${encodeURIComponent(d.account_id)}/drafts/${encodeURIComponent(d.id)}`,
                }}
                {selection}
                orderedKeys={drafts.map((x) => `draft:${x.account_id}:${x.id}`)}
              />
            </li>
          {/each}
        </ul>
      {/if}

    {:else if box.slug === 'snoozed'}
      {#if snoozed.length === 0}
        <EmptyState title="Nothing snoozed" hint="Messages you snooze reappear here until their wake time." />
      {:else}
        <ul id="snoozed-msg-list" class="msg-list">
          {#each snoozed as s (s.id)}
            {@const key = `snoozed:${s.account_id}:${s.uid}`}
            <li>
              <MessageRow
                message={{
                  key,
                  uid: s.uid,
                  accountId: s.account_id,
                  subject: s.subject ?? '(no subject)',
                  from: s.from_addr ?? s.account_id,
                  date: s.snooze_until,
                  snippet: null,
                  unread: readState.isUnread(s.account_id, s.snoozed_folder, s.uid, false),
                  starred: false,
                  accountChip: s.account_id,
                  href: `${base}/mail/snoozed/${encodeURIComponent(s.account_id)}/${s.uid}?folder=${encodeURIComponent(s.snoozed_folder)}`,
                }}
                {selection}
                orderedKeys={snoozed.map((x) => `snoozed:${x.account_id}:${x.uid}`)}
              />
            </li>
          {/each}
        </ul>
      {/if}

    {:else}
      <EmptyState
        title="{box.label} has no messages to show here"
        hint="This smart mailbox doesn't load its own list yet. Unified Inbox has your mail."
      >
        {#snippet action()}
          <a class="empty-link" href="{base}/mail/unified">Go to Unified Inbox</a>
        {/snippet}
      </EmptyState>
    {/if}
  </section>

  <section id="reader-pane" class="reader" aria-label="Reader">
    {@render children()}
  </section>
</div>

<!-- Composer drawer: mounts globally for keyboard 'c' and rail button. -->
<ComposerDrawer
  accounts={allAccounts}
  onsent={(res, accountId) => handleSent(res, accountId)}
/>

<!-- Undo toast: shown only when a compose queued with cooldown. -->
{#if undoToast && undoToast.res.cooldown_seconds > 0}
  <UndoToast
    draftId={undoToast.res.draft_id}
    accountId={undoToast.accountId}
    seconds={undoToast.res.cooldown_seconds}
    ondismiss={() => (undoToast = null)}
  />
{/if}

<style>
  .mail-shell {
    flex: 1;
    min-height: 0;
    display: grid;
    grid-template-columns: 240px minmax(360px, 1fr) minmax(360px, 42vw);
    gap: 1px;
    background: var(--env-rule);
    overflow: hidden;
  }
  .list {
    display: flex;
    flex-direction: column;
    min-height: 0;
    overflow-y: auto;
    background: var(--env-surface);
  }
  .reader {
    display: flex;
    flex-direction: column;
    min-height: 0;
    overflow-y: auto;
    background: var(--env-surface);
  }
  .pane-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    padding: 0.5rem 0.75rem;
    position: sticky;
    top: 0;
    min-height: 3.25rem;
    background: var(--env-soft);
    z-index: 2;
    border-bottom: 1px solid var(--env-rule);
  }
  .pane-title {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    text-transform: uppercase;
    letter-spacing: 0.12em;
    color: var(--env-muted);
    flex-shrink: 0;
  }
  .pane-head-right {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    flex: 1;
    min-width: 0;
    justify-content: flex-end;
  }
  .pane-count {
    flex-shrink: 0;
  }
  .list-loading {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 1rem;
    font-size: 0.8125rem;
    color: var(--env-muted);
  }
  .list-error {
    padding: 1rem;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }
  .list-error-msg {
    margin: 0;
    font-weight: 600;
    color: var(--env-warn);
  }
  .list-error-detail {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--env-muted);
  }
  .list-retry {
    align-self: flex-start;
    font-size: 0.8125rem;
    color: var(--env-accent);
    background: none;
    border: none;
    padding: 0;
    cursor: pointer;
    text-decoration: underline;
  }
  .list-partial {
    margin: 0;
    padding: 0.4rem 1rem;
    font-size: 0.75rem;
    color: var(--env-pending);
    background: var(--env-pending-soft);
  }
  .empty-link {
    font-size: 0.8125rem;
    color: var(--env-accent);
  }
  .msg-list {
    list-style: none;
    margin: 0;
    padding: 0;
    flex: 1;
  }
  .msg-list li {
    display: block;
  }
  .compose-btn {
    flex-shrink: 0;
    font-family: var(--font-sans);
    font-size: 0.8125rem;
    font-weight: 600;
    padding: 0.3rem 0.65rem;
    background: var(--env-ink);
    color: #fff;
    border: none;
    border-radius: var(--radius-sm, 3px);
    cursor: pointer;
    line-height: 1.2;
  }
  .compose-btn:hover {
    background: #262626;
  }
  .live-indicator {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    padding: 0.25rem 0.75rem;
    border-bottom: 1px solid var(--env-rule);
    background: var(--env-paper);
  }
  .live-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .live-dot-ok {
    background: var(--env-accent);
  }
  .live-dot-degraded {
    background: var(--env-pending, #c98a00);
  }
  .live-dot-pending {
    background: var(--env-muted);
  }
  .live-label {
    font-family: var(--font-mono);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--env-muted);
  }
  @media (max-width: 1100px) {
    .mail-shell {
      grid-template-columns: 220px minmax(340px, 1fr) minmax(340px, 38vw);
    }
  }
  @media (max-width: 760px) {
    .mail-shell {
      grid-template-columns: minmax(0, 1fr);
      min-height: calc(100vh - 126px);
      overflow: visible;
    }
    .mail-shell :global(.rail),
    .reader {
      display: none;
    }
    .mail-shell.is-reading .list {
      display: none;
    }
    .mail-shell.is-reading .reader {
      display: flex;
    }
    .pane-head {
      align-items: stretch;
      flex-direction: column;
    }
    .pane-head-right {
      justify-content: stretch;
    }
  }
</style>
