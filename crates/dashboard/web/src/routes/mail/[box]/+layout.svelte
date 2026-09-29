<script lang="ts">
  // v2 mailbox layout: rail + message list (selection, search, bulk ops) + reader outlet.
  // The list stays mounted while the reader (nested [account]/[uid] route) swaps the third column.
  // This layout also owns: SSE live-update wiring, Composer drawer launch points (header button
  // + keyboard 'c'), UndoToast for queued sends, and the rail-footer connection indicator.
  import { base } from '$app/paths';
  import { page } from '$app/state';
  import { goto } from '$app/navigation';
  import { onMount } from 'svelte';
  import type { Snippet } from 'svelte';
  import { Rail, Spinner, EmptyState, MonoTag } from '$lib/components';
  import MessageRow from '$lib/components/MessageRow.svelte';
  import BulkToolbar from '$lib/components/BulkToolbar.svelte';
  import SearchBar from '$lib/components/SearchBar.svelte';
  import ComposerDrawer from '$lib/components/ComposerDrawer.svelte';
  import UndoToast from '$lib/components/UndoToast.svelte';
  import SyncControl from '$lib/components/SyncControl.svelte';
  import ActionReceipts from '$lib/components/ActionReceipts.svelte';
  import { getMessageActions } from '$lib/message-actions.svelte';
  import { hasFlag } from '$lib/flags';
  import { mailboxBySlug } from '$lib/mailboxes';
  import { positionOf } from '$lib/mailbox-position';
  import {
    MailboxSync,
    autoSyncDue,
    isNewerView,
    staleSelectionKeys,
    type SyncScope
  } from '$lib/mailbox-sync.svelte';
  import { folderHints } from '$lib/folder-hints.svelte';
  import { SelectionStore } from '$lib/selection.svelte';
  import { readState } from '$lib/read-state.svelte';
  import { getLiveStore } from '$lib/live.svelte';
  import { getComposerStore } from '$lib/composer.svelte';
  import { getMailboxOpsStore } from '$lib/mailbox-ops.svelte';
  import { parseSearchQuery } from '$lib/search-query';
  import {
    api,
    EnvelopeApiError,
    type UnifiedInboxMessage,
    type UnifiedNextCursor,
    type Draft,
    type SnoozedItem,
    type SearchMessageSummary,
    type FolderStats,
    type ComposeResponse,
    type Account,
    type UnifiedInboxResponse,
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
  // Start from the current version: the signal outlives this layout, so a
  // remount must not replay an operation that finished before it existed.
  let seenOpsVersion = mailboxOps.version;
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
  let unifiedNextCursor = $state<UnifiedNextCursor | null>(null);
  let unifiedLoadingMore = $state(false);
  let drafts = $state<Draft[]>([]);
  let snoozed = $state<SnoozedItem[]>([]);
  let sentMessages = $state<UnifiedInboxMessage[]>([]);
  let sentNextCursor = $state<UnifiedNextCursor | null>(null);
  let sentLoadingMore = $state(false);
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
    uidvalidity?: number | null;
  };

  const searchQuery = $derived(page.url.searchParams.get('q') ?? '');
  let searchResults = $state<SearchHit[]>([]);
  let searching = $state(false);
  let searchError = $state<string | null>(null);
  /** Accounts that failed or timed out in the current search run (usernames). */
  let searchFailures = $state<string[]>([]);
  /** 'all' or a single account id — narrows the fan-out. */
  let searchScope = $state<string>('all');
  let searchGen = 0;
  let searchAbort: AbortController | null = null;
  const isSearching = $derived(searchQuery.length > 0);

  let unifiedLoadGen = 0;

  // ── Mailbox sync (#171) ───────────────────────────────────────────
  // Opening Inbox or Sent paints the server's cached index, then asks for one
  // read-only provider sync of that view; Sync now / Retry drive the same
  // path. `*AsOf` is the server time of the view on screen, so a response
  // read earlier (a slow cached reload, a superseded sync) can never paint
  // over a newer one.
  const mailboxSync = new MailboxSync((scope, accountId) => {
    const opts = accountId ? { accountId } : undefined;
    if (scope === 'sent') {
      return opts ? api.refreshSentInbox(50, opts) : api.refreshSentInbox(50);
    }
    return opts ? api.refreshUnifiedInbox(50, opts) : api.refreshUnifiedInbox(50);
  });
  let unifiedAsOf: string | null = null;
  let sentAsOf: string | null = null;
  const syncScope = $derived<SyncScope | null>(
    box?.slug === 'unified' || box?.slug === 'sent' ? box.slug : null
  );

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

  /** Deselect rows a newer view no longer contains (deleted, moved, or
   *  renumbered by a UIDVALIDITY reset), so no action runs on a stale handle.
   *  Everything still present stays selected. */
  function dropStaleSelection(
    prev: UnifiedInboxMessage[],
    next: UnifiedInboxMessage[],
    keyOf: (m: { account_id: string; uid: number }) => string
  ) {
    const stale = staleSelectionKeys(prev, next, selection.selected, keyOf);
    if (stale.length > 0) selection.deselect(stale);
  }

  /** After a provider sync the server's flags are current; confirmed flag
   *  overrides for rows it returned have done their job. */
  function settleFlagOverrides(res: UnifiedInboxResponse) {
    if (!res.sync) return;
    messageActions.settleFlagged(
      res.messages.map((m) => ({ accountId: m.account_id, folder: m.folder, uid: m.uid }))
    );
  }

  const unifiedKey = (m: { account_id: string; uid: number }) => `${m.account_id}:${m.uid}`;
  const sentKey = (m: { account_id: string; uid: number }) => `sent:${m.account_id}:${m.uid}`;

  function applyUnified(res: UnifiedInboxResponse): boolean {
    if (!isNewerView(unifiedAsOf, res.generated_at)) return false;
    unifiedAsOf = res.generated_at ?? unifiedAsOf;
    dropStaleSelection(unifiedMessages, res.messages, unifiedKey);
    settleFlagOverrides(res);
    unifiedMessages = res.messages;
    unifiedNextCursor = res.next_cursor ?? null;
    // Record the mailbox each row came from, so a reader link that lost its
    // `?folder=` can still resolve one instead of guessing INBOX.
    folderHints.remember(res.messages);
    return true;
  }

  /** Start (or join) the read-only provider sync for a view and paint its
   *  result, unless the operator has since left that view — the index is
   *  updated server-side either way, so the next open reads it. */
  async function syncView(scope: SyncScope, accountId?: string) {
    const res = await mailboxSync.sync(scope, accountId);
    if (!res || (page.params.box ?? 'unified') !== scope) return;
    if (scope === 'unified') applyUnified(res);
    else applySent(res);
  }

  /** Fetch the next unified page with the keyset cursor and append it.
   *  Deduped by account:uid so an overlap between pages can never render a
   *  message twice; order is preserved (the server continues the same total
   *  order the current tail ends on). */
  async function loadMoreUnified() {
    const cursor = unifiedNextCursor;
    if (!cursor || unifiedLoadingMore) return;
    unifiedLoadingMore = true;
    try {
      const res = await api.unifiedInbox(50, cursor);
      const seen = new Set(unifiedMessages.map((m) => `${m.account_id}:${m.uid}`));
      const fresh = res.messages.filter((m) => !seen.has(`${m.account_id}:${m.uid}`));
      unifiedMessages = [...unifiedMessages, ...fresh];
      unifiedNextCursor = res.next_cursor ?? null;
      folderHints.remember(fresh);
    } catch {
      // Keep the loaded rows; the button stays for a retry.
    } finally {
      unifiedLoadingMore = false;
    }
  }

  /** Paint the cached Inbox. `open` marks a navigation to the view: it then
   *  schedules the view's sync unless every account synced moments ago.
   *  Reloads (SSE events, bulk operations) stay cache-only. */
  async function loadUnified(open = false) {
    const gen = ++unifiedLoadGen;
    loading = unifiedMessages.length === 0;
    error = null;
    try {
      const res = await api.unifiedInbox(50);
      if (gen !== unifiedLoadGen) return;
      if (applyUnified(res)) mailboxSync.observe('unified', res);
      loading = false;
      if (open && autoSyncDue(res)) void syncView('unified');
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

  function applySent(res: UnifiedInboxResponse): boolean {
    if (!isNewerView(sentAsOf, res.generated_at)) return false;
    sentAsOf = res.generated_at ?? sentAsOf;
    dropStaleSelection(sentMessages, res.messages, sentKey);
    settleFlagOverrides(res);
    sentMessages = res.messages;
    sentNextCursor = res.next_cursor ?? null;
    folderHints.remember(res.messages);
    return true;
  }

  /** Sent reads the server's local index (kept warm by an hourly sweep), so
   *  first paint never fans IMAP from the browser; an open then syncs the
   *  Sent view server-side, same contract as the Inbox. */
  async function loadSent(open = false) {
    loading = sentMessages.length === 0;
    error = null;
    try {
      const res = await api.sentInbox(50);
      if (applySent(res)) mailboxSync.observe('sent', res);
      loading = false;
      if (open && autoSyncDue(res)) void syncView('sent');
    } catch (e) {
      const err = e as EnvelopeApiError;
      error = { code: err.code ?? 'unknown', message: err.message ?? 'Failed to load sent messages.' };
    } finally {
      loading = false;
    }
  }

  /** Next Sent page via the keyset cursor, deduped by account:uid like the
   *  unified pager. */
  async function loadMoreSent() {
    const cursor = sentNextCursor;
    if (!cursor || sentLoadingMore) return;
    sentLoadingMore = true;
    try {
      const res = await api.sentInbox(50, cursor);
      const seen = new Set(sentMessages.map((m) => `${m.account_id}:${m.uid}`));
      const fresh = res.messages.filter((m) => !seen.has(`${m.account_id}:${m.uid}`));
      sentMessages = [...sentMessages, ...fresh];
      sentNextCursor = res.next_cursor ?? null;
      folderHints.remember(fresh);
    } catch {
      // Keep the loaded rows; the button stays for a retry.
    } finally {
      sentLoadingMore = false;
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
      if (slug === 'unified') {
        loadUnified(true);
        loadFolders();
      } else if (slug === 'drafts') {
        loadDrafts();
      } else if (slug === 'snoozed') {
        loadSnoozed();
      } else if (slug === 'sent') {
        loadSent(true);
      }
    }
  });

  $effect(() => {
    const q = searchQuery;
    const scope = searchScope;
    // A search query change swaps the result set under any selection —
    // stale hidden selections from the prior list/query must never persist
    // to act against messages the operator can no longer see.
    selection.clear();
    if (!q) {
      searchAbort?.abort();
      searchGen += 1;
      searching = false;
      searchResults = [];
      searchError = null;
      searchFailures = [];
      return;
    }
    runSearch(q, scope);
  });

  /** Per-account IMAP search timeout. An account that cannot answer within
   *  this window is reported as unreachable instead of pinning the spinner —
   *  the sweep found "Searching…" running 90–150s against slow providers. */
  const SEARCH_ACCOUNT_TIMEOUT_MS = 10_000;
  /** In-flight per-account searches at once. 25 simultaneous IMAP SELECTs
   *  saturated the server (post-search navigation timed out); a small pool
   *  keeps the box responsive while the fan-out drains. */
  const SEARCH_CONCURRENCY = 4;

  async function runSearch(q: string, scope: string) {
    const gen = ++searchGen;
    searchAbort?.abort();
    const abort = new AbortController();
    searchAbort = abort;

    searching = true;
    searchError = null;
    searchFailures = [];
    searchResults = [];

    const { imap } = parseSearchQuery(q);
    if (!imap) {
      searching = false;
      return;
    }

    try {
      const { accounts } = await api.listAccounts();
      if (gen !== searchGen) return;
      const targets = scope === 'all' ? accounts : accounts.filter((a) => a.id === scope);

      const seen = new Set<string>();
      const failures: string[] = [];

      const searchOne = async (acct: Account) => {
        let timer: ReturnType<typeof setTimeout> | null = null;
        try {
          const res = await Promise.race([
            api.searchMessages(acct.id, imap, SEARCH_FOLDER, 50, { signal: abort.signal }),
            new Promise<never>((_, reject) => {
              timer = setTimeout(() => {
                reject(new Error('timed out'));
              }, SEARCH_ACCOUNT_TIMEOUT_MS);
            })
          ]);
          if (gen !== searchGen) return;
          // Results render as each account lands; identity-keyed dedupe means a
          // hit can never appear twice no matter how responses interleave.
          const fresh = res.messages
            .map((m) => ({ ...m, account_id: acct.id, folder: SEARCH_FOLDER }))
            .filter((m) => {
              const key = `${m.account_id}:${m.folder}:${m.uid}`;
              if (seen.has(key)) return false;
              seen.add(key);
              return true;
            });
          if (fresh.length > 0) searchResults = [...searchResults, ...fresh];
        } catch {
          if (gen === searchGen) failures.push(acct.username || acct.name || acct.id);
        } finally {
          if (timer) clearTimeout(timer);
        }
      };

      const queue = [...targets];
      const workers = Array.from({ length: Math.min(SEARCH_CONCURRENCY, queue.length) }, async () => {
        while (queue.length > 0 && gen === searchGen) {
          const acct = queue.shift()!;
          await searchOne(acct);
        }
      });
      await Promise.all(workers);
      if (gen !== searchGen) return;
      searchFailures = failures;
    } catch (e) {
      if (gen !== searchGen) return;
      const err = e as EnvelopeApiError;
      searchError = err.message ?? 'Search failed.';
    } finally {
      if (gen === searchGen) searching = false;
    }
  }

  const messageActions = getMessageActions();

  /** Flag state for a row: a confirmed action result wins over the list's
   *  (possibly cached) flags. Keyed by account + folder + UID. */
  function isStarred(uid: number, accountId: string, folder: string, flags: string[]): boolean {
    return messageActions.isFlagged({ accountId, folder, uid }, hasFlag(flags, 'flagged'));
  }

  function senderLabel(m: UnifiedInboxMessage): string {
    return m.from_addr || m.account_username;
  }

  /** Unread count for the loaded Inbox rows, through the same read-state
   *  overrides the rows render, so a Mark read/unread moves it at once. */
  const unifiedUnread = $derived(
    unifiedMessages.filter((m) => readState.isUnread(m.account_id, m.folder, m.uid, m.unread))
      .length
  );

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
        folder: m.folder,
        message_id: m.message_id ?? null,
        subject: m.subject ?? null,
        uidvalidity: m.uidvalidity ?? null,
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

  /** Sent rows carry each account's real Sent folder from the server index,
   *  so bulk actions (move/flag/delete…) dispatch against each row's actual
   *  mailbox rather than a hardcoded folder name. */
  const sentMessageIndex = $derived.by(() => {
    const idx: Record<string, MsgIndexEntry> = {};
    for (const m of sentMessages) {
      idx[`sent:${m.account_id}:${m.uid}`] = {
        accountId: m.account_id,
        uid: m.uid,
        from: m.from_addr ?? '',
        folder: m.folder,
        message_id: m.message_id ?? null,
        subject: m.subject ?? null,
        uidvalidity: m.uidvalidity ?? null,
      };
    }
    return idx;
  });

  /** A snooze record's stored `uid` names the message in its ORIGINAL folder;
   *  inside the Snoozed folder it has a UID the list does not know. So a
   *  snoozed row is not a mailbox handle and gets no bulk actions (they would
   *  act on whatever message holds that number in Snoozed). Its one action,
   *  Unsnooze, goes by snooze id through the row menu. */
  const snoozedMessageIndex: Record<string, MsgIndexEntry> = {};

  async function handleOperated() {
    const slug = page.params.box ?? 'unified';
    if (slug === 'unified') await loadUnified();
    else if (slug === 'drafts') await loadDrafts();
    else if (slug === 'snoozed') await loadSnoozed();
    else if (slug === 'sent') await loadSent();
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
        else if (slug === 'sent') loadSent();
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
              <MonoTag>{isSearching ? searchResults.length : (box.slug === 'unified' ? unifiedMessages.length : box.slug === 'drafts' ? drafts.length : box.slug === 'sent' ? sentMessages.length : snoozed.length)}</MonoTag>
            {/if}
          </span>
          {#if box.slug === 'unified' && !isSearching && unifiedMessages.length > 0}
            <span class="pane-unread" id="pane-unread-count" aria-live="polite">
              <MonoTag>{unifiedUnread} unread</MonoTag>
            </span>
          {/if}
          <SearchBar
            hint="Search {box.label}… (from: to: subject: is:unread before:)"
            onreset={() => { searchResults = []; searchError = null; searchFailures = []; }}
          />
          <select
            class="search-scope"
            aria-label="Search scope"
            bind:value={searchScope}
          >
            <option value="all">All accounts</option>
            {#each allAccounts as acct (acct.id)}
              <option value={acct.id}>{acct.username || acct.name}</option>
            {/each}
          </select>
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

    <div id="list-status-bar" class="list-status-bar">
      <!-- Event-stream connection state. It is not a sync state: "Live" means
           the dashboard hears server events, never that mail was synced. -->
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
      {#if syncScope && !isSearching}
        <SyncControl
          sync={mailboxSync.state(syncScope)}
          onsync={() => syncScope && void syncView(syncScope)}
          onretry={(accountId) => syncScope && void syncView(syncScope, accountId)}
        />
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
              : box.slug === 'sent'
                ? sentMessageIndex
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
        hint="This smart mailbox doesn't load its own list yet. Inbox has your mail."
      >
        {#snippet action()}
          <a class="empty-link" href="{base}/mail/unified">Go to Inbox</a>
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
          if (slug === 'unified') loadUnified(true);
          else if (slug === 'drafts') loadDrafts();
          else if (slug === 'snoozed') loadSnoozed();
          else if (slug === 'sent') loadSent(true);
        }}>Retry</button>
      </div>

    {:else if isSearching}
      {#if searchFailures.length > 0}
        <p class="search-failures" role="status">
          {searchFailures.length} account{searchFailures.length === 1 ? '' : 's'} didn't respond: {searchFailures.join(', ')}
        </p>
      {/if}
      {#if searching && searchResults.length === 0}
        <div class="list-loading"><Spinner label="Searching" /> <span>Searching…</span></div>
      {:else if searchError}
        <div class="list-error" role="alert">
          <p class="list-error-msg">Search failed.</p>
          <p class="list-error-detail">{searchError}</p>
        </div>
      {:else if searchResults.length === 0 && !searching}
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
                  starred: isStarred(m.uid, m.account_id, m.folder, m.flags),
                  folder: m.folder,
                  messageId: m.message_id,
                  href: `${base}/mail/unified/${encodeURIComponent(m.account_id)}/${m.uid}?folder=${encodeURIComponent(m.folder)}`,
                }}
                {selection}
                orderedKeys={orderedSearchKeys}
                verbs
              />
            </li>
          {/each}
        </ul>
      {/if}

    {:else if box.slug === 'unified'}
      {#if unifiedMessages.length === 0 && mailboxSync.isSyncing('unified')}
        <div class="list-loading"><Spinner label="Syncing Inbox" /> <span>Syncing Inbox…</span></div>
      {:else if unifiedMessages.length === 0}
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
                  starred: isStarred(m.uid, m.account_id, m.folder, m.flags),
                  accountChip: m.account_display_name || m.account_username,
                  folder: m.folder,
                  uidvalidity: m.uidvalidity,
                  messageId: m.message_id,
                  href: `${base}/mail/unified/${encodeURIComponent(m.account_id)}/${m.uid}?folder=${encodeURIComponent(m.folder)}`,
                }}
                {selection}
                orderedKeys={orderedUnifiedKeys}
                {active}
                verbs
              />
            </li>
          {/each}
        </ul>
        {#if unifiedNextCursor}
          <div class="load-more-row">
            <button
              class="load-more-btn"
              type="button"
              disabled={unifiedLoadingMore}
              onclick={loadMoreUnified}
            >
              {unifiedLoadingMore ? 'Loading…' : 'Load more'}
            </button>
          </div>
        {/if}
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
            {@const key = `snoozed:${s.id}`}
            <li>
              <MessageRow
                message={{
                  key,
                  uid: s.uid,
                  accountId: s.account_id,
                  subject: s.subject ?? '(no subject)',
                  from: `Snoozed from ${s.original_folder}`,
                  date: s.created_at,
                  snippet: null,
                  unread: false,
                  starred: false,
                  folder: s.snoozed_folder,
                  accountChip:
                    allAccounts.find((a) => a.id === s.account_id)?.username ?? s.account_id,
                  snooze: { id: s.id, returnAt: s.return_at, status: s.status },
                }}
                {selection}
                orderedKeys={snoozed.map((x) => `snoozed:${x.id}`)}
              />
            </li>
          {/each}
        </ul>
      {/if}

    {:else if box.slug === 'sent'}
      {#if sentMessages.length === 0 && mailboxSync.isSyncing('sent')}
        <div class="list-loading"><Spinner label="Syncing Sent" /> <span>Syncing Sent…</span></div>
      {:else if sentMessages.length === 0}
        <EmptyState title="No sent messages" hint="Messages you send appear here, across all connected accounts." />
      {:else}
        <ul id="sent-msg-list" class="msg-list">
          {#each sentMessages as m (`sent:${m.account_id}:${m.uid}`)}
            {@const key = `sent:${m.account_id}:${m.uid}`}
            <li>
              <MessageRow
                message={{
                  key,
                  uid: m.uid,
                  accountId: m.account_id,
                  subject: m.subject || '(no subject)',
                  from: m.to_addr || m.from_addr,
                  date: m.date,
                  snippet: m.snippet,
                  unread: false,
                  starred: isStarred(m.uid, m.account_id, m.folder, m.flags),
                  accountChip: m.account_display_name || m.account_username,
                  folder: m.folder,
                  uidvalidity: m.uidvalidity,
                  messageId: m.message_id,
                  href: `${base}/mail/sent/${encodeURIComponent(m.account_id)}/${m.uid}?folder=${encodeURIComponent(m.folder)}`,
                }}
                {selection}
                orderedKeys={sentMessages.map((x) => `sent:${x.account_id}:${x.uid}`)}
                verbs
              />
            </li>
          {/each}
        </ul>
        {#if sentNextCursor}
          <div class="load-more-row">
            <button
              class="load-more-btn"
              type="button"
              disabled={sentLoadingMore}
              onclick={loadMoreSent}
            >
              {sentLoadingMore ? 'Loading…' : 'Load more'}
            </button>
          </div>
        {/if}
      {/if}

    {:else}
      <EmptyState
        title="{box.label} has no messages to show here"
        hint="This smart mailbox doesn't load its own list yet. Inbox has your mail."
      >
        {#snippet action()}
          <a class="empty-link" href="{base}/mail/unified">Go to Inbox</a>
        {/snippet}
      </EmptyState>
    {/if}
  </section>

  <section id="reader-pane" class="reader" aria-label="Reader">
    {@render children()}
  </section>
</div>

<!-- Durable receipts for single-message actions (Junk, Archive, Snooze…). -->
<ActionReceipts />

<!-- Composer drawer: mounts globally for keyboard 'c' and rail button. -->
<ComposerDrawer
  accounts={allAccounts}
  onsent={(res, accountId) => handleSent(res, accountId)}
  onsaved={(accountId, draftId) =>
    goto(`${base}/accounts/${encodeURIComponent(accountId)}/drafts/${encodeURIComponent(draftId)}`)}
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
  .list-status-bar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.5rem;
    min-height: 2.25rem;
    padding: 0.25rem 0.75rem;
    border-bottom: 1px solid var(--env-rule);
    background: var(--env-paper);
  }
  .live-indicator {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    flex-shrink: 0;
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
      /* One column, one scroller: the document. The desktop overflow-y: auto
         makes this pane the scroll container, and on iOS a touch that starts
         on the tall sandboxed message iframe belongs to the pane's scroller
         and never chains out — the end of a long HTML message becomes
         unreachable. The grid row grows with the content instead. */
      overflow: visible;
    }
    .pane-head {
      align-items: stretch;
      flex-direction: column;
    }
    .pane-head-right {
      justify-content: stretch;
    }
  }
  .search-scope {
    flex-shrink: 0;
    max-width: 11rem;
    font: inherit;
    font-size: 0.8125rem;
    color: var(--env-ink);
    background: var(--env-surface);
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-sm, 3px);
    padding: 0.3rem 0.4rem;
  }
  .search-failures {
    margin: 0.5rem 0.75rem;
    font-size: 0.75rem;
    color: var(--env-muted);
  }
  .load-more-row {
    display: flex;
    justify-content: center;
    padding: 0.6rem 0 1rem;
  }
  .load-more-btn {
    font: inherit;
    font-size: 0.8125rem;
    color: var(--env-ink);
    background: var(--env-surface);
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-sm, 3px);
    padding: 0.35rem 1.1rem;
    cursor: pointer;
  }
  .load-more-btn:disabled {
    opacity: 0.6;
    cursor: default;
  }
</style>
