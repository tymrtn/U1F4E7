<script lang="ts">
  // Selection toolbar. Hidden at 0 selected; sticky above the list when N > 0.
  // Restrained icon+label actions (Archive · Snooze · Flag/Unflag · Junk · Delete),
  // with Mark read/unread + Move kept quiet on the side. Bulk ops fan out through
  // bulkClient() (concurrency-capped, partial-failure aware) until a server bulk
  // endpoint ships; snooze and junk-rule creation dispatch per item/sender.
  //
  // Safety: ordinary Delete is a reversible provider-aware Trash move; only a
  // Trash view exposes confirmed hard-delete. "Block sender" only ever writes
  // an EXACT-sender rule shown before creation and requires an explicit click.
  import type { SelectionStore } from '$lib/selection.svelte';
  import type { BulkItem, BulkProgress, FolderStats } from '$lib/api';
  import { api, bulkClient } from '$lib/api';
  import { rulesApi } from '$lib/rules-api';
  import { normalizedAddress, isValidEmail } from '$lib/addresses';
  import { readState } from '$lib/read-state.svelte';
  import Modal from './Modal.svelte';
  import Toast from './Toast.svelte';
  import MonoTag from './MonoTag.svelte';

  type RetryOp =
    | 'archive'
    | 'trash-move'
    | 'delete'
    | 'junk'
    | 'read'
    | 'unread'
    | 'flag'
    | 'unflag'
    | 'block-junk';
  type ToastItem = {
    id: number;
    variant: 'ok' | 'warn' | 'danger';
    text: string;
    retryItems?: BulkItem[];
    /** Retry payload for the compound 'block-junk' op — carries `from` too. */
    retryCompound?: DetailedItem[];
    retryOp?: RetryOp;
  };

  /**
   * Per-message context the list supplies so junk-rules, snooze, and every
   * mailbox action are truthful. `accountId`/`uid` are the message's REAL IMAP
   * identity — the toolbar reads them ONLY from here, never by inferring them
   * from the shape of the (opaque) selection key. Selection keys vary by
   * surface (`acct:uid` for unified, `search:acct:uid` for search hits,
   * `draft:acct:id` for drafts, `snoozed:acct:uid` for snoozed) and are not a
   * parseable contract — a key with no messageIndex entry has no derivable
   * real identity and is dropped rather than guessed.
   */
  type MsgInfo = {
    accountId: string;
    uid: number;
    from: string;
    folder?: string;
    message_id?: string | null;
    subject?: string | null;
  };

  let {
    selection,
    folder = 'INBOX',
    folders = [],
    messageIndex = {},
    onoperated,
    loading = false,
  }: {
    selection: SelectionStore;
    folder?: string;
    folders?: FolderStats[];
    messageIndex?: Record<string, MsgInfo>;
    onoperated?: () => void;
    /** True while the parent list is (re)loading data — mailbox actions stay
     *  disabled but the toolbar (and any in-flight retry state) stays mounted. */
    loading?: boolean;
  } = $props();

  // Canonical special-use MOVE sentinels. The dashboard `/move` boundary and the
  // rule engine resolve these to each account's real provider folder (Gmail's
  // Spam/All Mail/Trash, Outlook's Junk Email/Archive/Deleted Items, or the
  // detected special-use folder) — never a literal "Junk"/"Archive"/"Trash".
  const ARCHIVE_TARGET = '\\Archive';
  const JUNK_TARGET = '\\Junk';
  const TRASH_TARGET = '\\Trash';

  // True when the toolbar is acting on a Trash view: only then does Delete mean
  // a permanent, confirmed hard-delete. Everywhere else Delete moves to Trash.
  const inTrash = $derived(looksLikeTrash(folder));

  function looksLikeTrash(f: string): boolean {
    const leaf = (f ?? '').split(/[/.]/).pop()?.trim().toLowerCase() ?? '';
    return leaf === 'trash' || leaf === 'deleted items' || leaf === 'deleted messages';
  }

  let toastSeq = $state(0);
  let toasts = $state<ToastItem[]>([]);

  let deleteConfirmOpen = $state(false);
  let deleteTyped = $state('');

  let moveFolderPickerOpen = $state(false);
  let selectedMoveFolder = $state('');

  let junkMenuOpen = $state(false);
  let blockConfirmOpen = $state(false);

  let snoozeMenuOpen = $state(false);
  let customSnooze = $state('');

  let moreMenuOpen = $state(false);

  let opRunning = $state(false);
  let opProgress = $state<{ done: number; total: number } | null>(null);

  // ── Selection helpers ────────────────────────────────────────────────
  //
  // Selection keys are opaque and shaped differently per surface. The ONLY
  // trustworthy source for a message's account/uid is `messageIndex[key]`,
  // which every actionable (mailbox-backed) surface must populate. Production
  // code never derives identity by parsing the key itself — a key with no
  // messageIndex entry has no derivable real identity and is dropped.

  type ResolvedItem = { accountId: string; uid: number; folder: string };

  function resolveItem(key: string): ResolvedItem | null {
    const info = messageIndex[key];
    if (!info || typeof info.accountId !== 'string' || !info.accountId || typeof info.uid !== 'number' || !Number.isFinite(info.uid)) {
      return null;
    }
    return { accountId: info.accountId, uid: info.uid, folder: info.folder ?? folder };
  }

  function resolvedSelection(): ResolvedItem[] {
    return Array.from(selection.selected)
      .map(resolveItem)
      .filter((item): item is ResolvedItem => item !== null);
  }

  /** False when any selected key fails to resolve to a real account/uid — the
   *  mailbox actions must not run (or silently report zero-item "success")
   *  against a partially-resolvable selection. */
  const allSelectedResolve = $derived(resolvedSelection().length === selection.count);

  const controlsDisabled = $derived(opRunning || loading || !allSelectedResolve);

  function selectedItems(): BulkItem[] {
    // Dispatch each item with its own source folder (a unified selection can
    // span mailboxes); fall back to the toolbar's folder when unknown.
    return resolvedSelection().map(({ accountId, uid, folder }) => ({ accountId, uid, folder }));
  }

  function selectedDetailed() {
    return Array.from(selection.selected).flatMap((key) => {
      const resolved = resolveItem(key);
      if (!resolved) return [];
      const info = messageIndex[key];
      return [
        {
          key,
          accountId: resolved.accountId,
          uid: resolved.uid,
          from: info?.from ?? '',
          folder: resolved.folder,
          message_id: info?.message_id ?? null,
          subject: info?.subject ?? null,
        },
      ];
    });
  }

  type DetailedItem = ReturnType<typeof selectedDetailed>[number];

  const selectedAccounts = $derived.by(() => {
    const set = new Set<string>();
    for (const item of resolvedSelection()) set.add(item.accountId);
    return Array.from(set);
  });

  function accountScopeText(): string {
    const n = selectedAccounts.length;
    const noun = n === 1 ? 'account' : 'accounts';
    return `${n} ${noun} (${selectedAccounts.join(', ')})`;
  }

  /** account → distinct EXACT sender addresses, for block-sender rules. */
  function blockTargets(): Map<string, string[]> {
    const map = new Map<string, Set<string>>();
    for (const it of selectedDetailed()) {
      const email = normalizedAddress(it.from);
      if (!email || !isValidEmail(email)) continue;
      if (!map.has(it.accountId)) map.set(it.accountId, new Set());
      map.get(it.accountId)!.add(email);
    }
    return new Map(Array.from(map, ([acct, set]) => [acct, Array.from(set)]));
  }

  const blockSenders = $derived.by(() => {
    const set = new Set<string>();
    for (const senders of blockTargets().values()) for (const s of senders) set.add(s);
    return Array.from(set);
  });

  /** Selected items with no readable, valid sender address. These can never
   *  be blocked (there is nothing to write a rule against), so the compound
   *  Block & move confirm must refuse to run while any are present rather
   *  than silently treating them as "move only, nothing to block". */
  const invalidSenderItems = $derived.by(() =>
    selectedDetailed().filter((it) => {
      const email = normalizedAddress(it.from);
      return !email || !isValidEmail(email);
    })
  );
  const hasInvalidSender = $derived(invalidSenderItems.length > 0);

  // ── Toasts ───────────────────────────────────────────────────────────

  function addToast(
    variant: ToastItem['variant'],
    text: string,
    retryItems?: BulkItem[],
    retryOp?: RetryOp,
    retryCompound?: DetailedItem[]
  ) {
    const id = ++toastSeq;
    toasts = [...toasts, { id, variant, text, retryItems, retryOp, retryCompound }];
    if (variant === 'ok') setTimeout(() => dismissToast(id), 5000);
  }

  function dismissToast(id: number) {
    toasts = toasts.filter((t) => t.id !== id);
  }

  // ── Bulk op runner (move / flags / delete) ───────────────────────────

  async function runBulkOp(
    op: Parameters<typeof bulkClient>[0],
    items: BulkItem[],
    actionLabel: string,
    retryOp?: RetryOp
  ) {
    opRunning = true;
    opProgress = { done: 0, total: items.length };
    const result = await bulkClient(op, items, (p) => {
      opProgress = { done: p.done, total: p.total };
    });
    opRunning = false;
    opProgress = null;

    const failedItems = result.failed.map((f) => f.item);
    const succeeded = result.total - result.failed.length;
    const variant: ToastItem['variant'] =
      result.failed.length === 0 ? 'ok' : succeeded === 0 ? 'danger' : 'warn';
    const text =
      result.failed.length === 0
        ? `${succeeded} ${actionLabel}`
        : `${succeeded} ${actionLabel}, ${result.failed.length} failed`;

    addToast(variant, text, failedItems.length > 0 ? failedItems : undefined, retryOp);
    if (result.failed.length === 0) selection.deselectAll();
    onoperated?.();
    return result;
  }

  /** Defensive guard mirroring the disabled buttons: even if a click somehow
   *  reaches a handler while the selection isn't fully resolvable (e.g. a
   *  stray event before the disabled state paints), the op must never run
   *  against a partially-resolved selection nor report a false success. */
  function requireFullyResolved(): boolean {
    return allSelectedResolve;
  }

  async function archive() {
    if (!requireFullyResolved()) return;
    await runBulkOp({ type: 'move', folder, to_folder: ARCHIVE_TARGET }, selectedItems(), 'archived', 'archive');
  }

  function itemIdentity(item: BulkItem): string {
    return `${item.accountId}\0${item.folder ?? folder}\0${item.uid}`;
  }

  function applyConfirmedReadState(items: BulkItem[], result: BulkProgress, read: boolean) {
    const failed = new Set(result.failed.map(({ item }) => itemIdentity(item)));
    // Update only confirmed successes. Forcing the inverse state on failure is
    // not a rollback: a failed item may already have had either state.
    for (const item of items) {
      if (failed.has(itemIdentity(item))) continue;
      if (read) readState.markRead(item.accountId, item.folder ?? folder, item.uid);
      else readState.markUnread(item.accountId, item.folder ?? folder, item.uid);
    }
  }

  async function runReadStateOp(items: BulkItem[], read: boolean, label: string, retryOp: RetryOp) {
    const result = await runBulkOp(
      {
        type: 'flags',
        folder,
        add: read ? ['\\Seen'] : [],
        remove: read ? [] : ['\\Seen']
      },
      items,
      label,
      retryOp
    );
    applyConfirmedReadState(items, result, read);
    return result;
  }

  async function markRead() {
    moreMenuOpen = false;
    if (!requireFullyResolved()) return;
    await runReadStateOp(selectedItems(), true, 'marked read', 'read');
  }

  async function markUnread() {
    moreMenuOpen = false;
    if (!requireFullyResolved()) return;
    await runReadStateOp(selectedItems(), false, 'marked unread', 'unread');
  }

  async function flag() {
    if (!requireFullyResolved()) return;
    await runBulkOp({ type: 'flags', folder, add: ['\\Flagged'], remove: [] }, selectedItems(), 'flagged', 'flag');
  }

  async function unflag() {
    moreMenuOpen = false;
    if (!requireFullyResolved()) return;
    await runBulkOp({ type: 'flags', folder, add: [], remove: ['\\Flagged'] }, selectedItems(), 'unflagged', 'unflag');
  }

  // ── Junk (split: move-only vs move + block sender) ───────────────────

  async function junkOnly() {
    junkMenuOpen = false;
    if (!requireFullyResolved()) return;
    await runBulkOp({ type: 'move', folder, to_folder: JUNK_TARGET }, selectedItems(), 'moved to Junk', 'junk');
  }

  function openBlockConfirm() {
    junkMenuOpen = false;
    blockConfirmOpen = true;
  }

  /** Short, non-PII, collision-resistant suffix for auto-created rule names. */
  function blockRuleSuffix(): string {
    if (typeof crypto !== 'undefined' && 'randomUUID' in crypto) {
      return crypto.randomUUID().slice(0, 8);
    }
    return Math.random().toString(36).slice(2, 10);
  }

  function blockRuleKey(accountId: string, sender: string): string {
    return `${accountId}\0${sender}`;
  }

  /** Senders whose block rule has already been created successfully. Persists
   *  across a retry so a retry never creates a duplicate rule for a sender
   *  that already got one — only the still-failed pairs are retried. */
  let createdBlockRuleKeys = new Set<string>();

  /**
   * Block-sender + move-to-Junk is ONE compound operator request: rule
   * creation and the mailbox move are two separate network calls, so a
   * partial failure (rule ok / move failed, or rule failed / move never
   * attempted) must never be reported as a plain success, and the affected
   * items must stay selected so "Retry failed" can pick them back up —
   * without recreating a rule that already exists.
   */
  async function runBlockAndJunk(items: DetailedItem[]) {
    if (items.length === 0) return;
    opRunning = true;

    // Pass 1 — ensure a block rule exists for every distinct (account, exact
    // sender) pair in this batch, skipping pairs already confirmed created.
    const pairs = new Map<string, { accountId: string; sender: string }>();
    for (const it of items) {
      const email = normalizedAddress(it.from);
      if (!email || !isValidEmail(email)) continue;
      pairs.set(blockRuleKey(it.accountId, email), { accountId: it.accountId, sender: email });
    }
    const pairOutcome = new Map<string, boolean>();
    let created = 0;
    let ruleFailures = 0;
    for (const [key, { accountId, sender }] of pairs) {
      if (createdBlockRuleKeys.has(key)) {
        pairOutcome.set(key, true);
        continue;
      }
      try {
        await rulesApi.create(accountId, {
          // No sender address in the rule name: rule executors log the fired
          // rule's name on every match, so a raw address here would leak PII
          // into tracing/audit output on every future hit. `blockSenders`
          // already shows the exact address in the confirm modal before
          // creation — the name only needs to be a stable, non-PII label.
          name: `Junk: blocked sender (${blockRuleSuffix()})`,
          // `from_exact` is a literal (non-glob) comparison — never `from`,
          // whose `*`/`?` are wildcards. A local-part that happens to
          // contain those characters (a valid, if unusual, email address)
          // must match only that exact address, never broaden into every
          // sender at the domain.
          match_expr: { from_exact: sender },
          // Canonical semantic target: the rule engine resolves `\Junk` to the
          // account's real Spam/Junk folder per provider when it later runs —
          // never a stored literal "Junk" that mis-files on Gmail/Outlook.
          action: { move: JUNK_TARGET },
          enabled: true,
        });
        createdBlockRuleKeys.add(key);
        pairOutcome.set(key, true);
        created += 1;
      } catch {
        pairOutcome.set(key, false);
        ruleFailures += 1;
      }
    }

    // Pass 2 — only move items whose sender's block rule is confirmed in
    // place. An item whose rule failed is left unmoved and selected: the
    // compound op is not "done" for it, so it must stay retryable rather
    // than silently landing in Junk without the block rule the operator
    // asked for. An item with no readable/valid sender at all can never have
    // a rule — it must never be treated as rule-success or silently moved;
    // it stays selected (never deselected) so the operator can explicitly
    // deselect it or move it separately via plain "Move to Junk".
    const moveEligible: DetailedItem[] = [];
    const blockedByRuleFailure: DetailedItem[] = [];
    const skippedInvalidSender: DetailedItem[] = [];
    for (const it of items) {
      const email = normalizedAddress(it.from);
      if (!email || !isValidEmail(email)) {
        skippedInvalidSender.push(it);
        continue;
      }
      const key = blockRuleKey(it.accountId, email);
      const ruleOk = pairOutcome.get(key) !== false;
      (ruleOk ? moveEligible : blockedByRuleFailure).push(it);
    }

    opProgress = moveEligible.length ? { done: 0, total: moveEligible.length } : null;
    const moveResult = moveEligible.length
      ? await bulkClient(
          { type: 'move', folder, to_folder: JUNK_TARGET },
          moveEligible.map(({ accountId, uid, folder }) => ({ accountId, uid, folder })),
          (p) => {
            opProgress = { done: p.done, total: p.total };
          }
        )
      : { total: 0, done: 0, failed: [] as BulkProgress['failed'] };
    opRunning = false;
    opProgress = null;

    const moveFailedIds = new Set(
      moveResult.failed.map(({ item }) => itemIdentity(item))
    );
    const movedOk = moveEligible.filter((it) => !moveFailedIds.has(itemIdentity(it)));
    const moveFailed = moveEligible.filter((it) => moveFailedIds.has(itemIdentity(it)));

    // Only items that cleared BOTH the rule step and the move are done.
    selection.deselect(movedOk.map((it) => it.key));
    const retryable = [...blockedByRuleFailure, ...moveFailed];

    const parts: string[] = [];
    if (movedOk.length > 0) parts.push(`${movedOk.length} moved to Junk`);
    if (created > 0) parts.push(`${created} rule${created === 1 ? '' : 's'} added`);
    if (ruleFailures > 0) {
      parts.push(`${ruleFailures} rule${ruleFailures === 1 ? '' : 's'} failed (not moved)`);
    }
    if (moveFailed.length > 0) {
      parts.push(`${moveFailed.length} move${moveFailed.length === 1 ? '' : 's'} failed`);
    }
    if (skippedInvalidSender.length > 0) {
      parts.push(
        `${skippedInvalidSender.length} skipped (no valid sender)`
      );
    }
    const text = parts.length > 0 ? parts.join(', ') : 'nothing to do';
    const variant: ToastItem['variant'] =
      ruleFailures === 0 && moveFailed.length === 0 && skippedInvalidSender.length === 0
        ? 'ok'
        : movedOk.length === 0 && created === 0
          ? 'danger'
          : 'warn';

    addToast(
      variant,
      text,
      undefined,
      retryable.length > 0 ? 'block-junk' : undefined,
      retryable.length > 0 ? retryable : undefined
    );
    onoperated?.();
  }

  async function confirmBlockAndJunk() {
    blockConfirmOpen = false;
    if (!requireFullyResolved() || hasInvalidSender) return;
    await runBlockAndJunk(selectedDetailed());
  }

  // ── Snooze (presets + custom) ────────────────────────────────────────
  // Presets are built in the operator's local wall-clock, then sent as a UTC
  // instant (ISO `…Z`). The server stores UTC wall-clock so the existing
  // unsnooze sweep (which compares against UTC now) fires at the right time.

  function laterToday(): string {
    const d = new Date();
    d.setHours(d.getHours() + 3);
    return d.toISOString();
  }
  function tomorrow9(): string {
    const d = new Date();
    d.setDate(d.getDate() + 1);
    d.setHours(9, 0, 0, 0);
    return d.toISOString();
  }
  function nextWeekday9(target: number): string {
    // target: 1=Mon … 6=Sat. Always the NEXT such day (strictly future).
    const d = new Date();
    const add = ((target - d.getDay() + 7) % 7) || 7;
    d.setDate(d.getDate() + add);
    d.setHours(9, 0, 0, 0);
    return d.toISOString();
  }

  async function snoozeTo(returnAt: string) {
    snoozeMenuOpen = false;
    if (!requireFullyResolved()) return;
    const items = selectedDetailed();
    opRunning = true;
    opProgress = { done: 0, total: items.length };
    const failed: BulkItem[] = [];
    let done = 0;
    for (const it of items) {
      try {
        await api.snoozeMessage(it.accountId, it.uid, {
          folder: it.folder,
          return_at: returnAt,
          message_id: it.message_id,
          subject: it.subject,
        });
      } catch {
        failed.push({ accountId: it.accountId, uid: it.uid });
      }
      done += 1;
      opProgress = { done, total: items.length };
    }
    opRunning = false;
    opProgress = null;
    const succeeded = items.length - failed.length;
    const variant: ToastItem['variant'] = failed.length === 0 ? 'ok' : succeeded === 0 ? 'danger' : 'warn';
    addToast(variant, failed.length === 0 ? `${succeeded} snoozed` : `${succeeded} snoozed, ${failed.length} failed`);
    if (failed.length === 0) selection.deselectAll();
    onoperated?.();
  }

  function snoozeCustom() {
    if (!customSnooze) return;
    // <input type="datetime-local"> gives local `YYYY-MM-DDTHH:MM`; interpret
    // it in local time and send the resulting UTC instant.
    const when = new Date(customSnooze);
    if (Number.isNaN(when.getTime()) || when.getTime() <= Date.now()) {
      addToast('warn', 'Pick a time in the future to snooze');
      return;
    }
    snoozeTo(when.toISOString());
  }

  // ── Delete ────────────────────────────────────────────────────────────
  // From an ordinary mailbox, Delete moves to the provider-aware canonical Trash
  // and is reversible (recoverable from Trash) — no confirmation ceremony. Only
  // when the source IS the Trash view does Delete mean a permanent, confirmed
  // hard-delete (count + account scope), which invokes the backend DELETE.

  const NEEDS_TYPED_CONFIRM = 10;
  const simpleConfirm = $derived(selection.count <= NEEDS_TYPED_CONFIRM);

  function onDeleteClick() {
    if (inTrash) openDeleteConfirm();
    else deleteToTrash();
  }

  async function deleteToTrash() {
    if (!requireFullyResolved()) return;
    await runBulkOp(
      { type: 'move', folder, to_folder: TRASH_TARGET },
      selectedItems(),
      'moved to Trash',
      'trash-move'
    );
  }

  function openDeleteConfirm() {
    deleteTyped = '';
    deleteConfirmOpen = true;
  }
  function closeDeleteConfirm() {
    deleteConfirmOpen = false;
    deleteTyped = '';
  }
  async function confirmDelete() {
    if (!simpleConfirm && deleteTyped !== String(selection.count)) return;
    if (!requireFullyResolved()) return;
    closeDeleteConfirm();
    await runBulkOp({ type: 'delete', folder }, selectedItems(), 'permanently deleted', 'delete');
  }

  // ── Move to folder ───────────────────────────────────────────────────

  function openMoveFolder() {
    moreMenuOpen = false;
    selectedMoveFolder = '';
    moveFolderPickerOpen = true;
  }
  async function confirmMove() {
    if (!selectedMoveFolder) return;
    if (!requireFullyResolved()) return;
    moveFolderPickerOpen = false;
    await runBulkOp({ type: 'move', folder, to_folder: selectedMoveFolder }, selectedItems(), `moved to ${selectedMoveFolder}`);
  }

  // ── Retry ────────────────────────────────────────────────────────────

  function isCurrentRetryItem(item: BulkItem): boolean {
    return resolvedSelection().some((current) => itemIdentity(current) === itemIdentity(item));
  }

  function isCurrentCompoundRetryItem(item: DetailedItem): boolean {
    if (!selection.isSelected(item.key)) return false;
    const current = resolveItem(item.key);
    return current !== null && itemIdentity(current) === itemIdentity(item);
  }

  async function retryFailed(toastId: number, items: BulkItem[] | undefined, op?: RetryOp, compound?: DetailedItem[]) {
    // Dismiss the stale toast BEFORE dispatching the retry so its own
    // "Retry failed" control can never be clicked twice — the fresh outcome
    // toast produced below becomes the sole authoritative one.
    dismissToast(toastId);
    if (!op) return;
    if (op === 'block-junk') {
      const current = compound?.filter(isCurrentCompoundRetryItem) ?? [];
      if (current.length === 0) {
        addToast('warn', 'Retry expired after mailbox or search changed');
        return;
      }
      await runBlockAndJunk(current);
      return;
    }
    if (!items) return;
    const current = items.filter(isCurrentRetryItem);
    if (current.length === 0) {
      addToast('warn', 'Retry expired after mailbox or search changed');
      return;
    }
    const map: Record<Exclude<RetryOp, 'block-junk'>, () => Promise<unknown>> = {
      archive: () => runBulkOp({ type: 'move', folder, to_folder: ARCHIVE_TARGET }, current, 'archived (retry)', 'archive'),
      'trash-move': () => runBulkOp({ type: 'move', folder, to_folder: TRASH_TARGET }, current, 'moved to Trash (retry)', 'trash-move'),
      delete: () => runBulkOp({ type: 'delete', folder }, current, 'permanently deleted (retry)', 'delete'),
      junk: () => runBulkOp({ type: 'move', folder, to_folder: JUNK_TARGET }, current, 'moved to Junk (retry)', 'junk'),
      read: () => runReadStateOp(current, true, 'marked read (retry)', 'read'),
      unread: () => runReadStateOp(current, false, 'marked unread (retry)', 'unread'),
      flag: () => runBulkOp({ type: 'flags', folder, add: ['\\Flagged'] }, current, 'flagged (retry)', 'flag'),
      unflag: () => runBulkOp({ type: 'flags', folder, remove: ['\\Flagged'] }, current, 'unflagged (retry)', 'unflag'),
    };
    await map[op]();
  }
</script>

{#if selection.count > 0}
  <div id="bulk-toolbar" class="bulk-toolbar" role="toolbar" aria-label="Bulk actions">
    <div class="bulk-context">
      <span class="bulk-count">{selection.count} selected</span>
      <button type="button" class="bulk-deselect" onclick={() => selection.deselectAll()} aria-label="Clear selection" title="Clear selection">×</button>
    </div>

    <div class="bulk-actions">
      <button type="button" class="bulk-btn" aria-label="Archive" title="Archive" onclick={archive} disabled={controlsDisabled}>
        <svg class="bulk-ico" viewBox="0 0 16 16" aria-hidden="true"><rect x="2" y="3.5" width="12" height="3" rx="0.5"/><path d="M3 6.5v6.5h10V6.5"/><path d="M6.3 9.2h3.4"/></svg>
        <span class="bulk-label">Archive</span>
      </button>

      <div class="bulk-split">
        <button type="button" class="bulk-btn" aria-label="Snooze" title="Snooze" aria-haspopup="menu" aria-expanded={snoozeMenuOpen} onclick={() => (snoozeMenuOpen = !snoozeMenuOpen)} disabled={controlsDisabled}>
          <svg class="bulk-ico" viewBox="0 0 16 16" aria-hidden="true"><circle cx="8" cy="8.5" r="5.3"/><path d="M8 5.3V8.7l2.2 1.4"/></svg>
          <span class="bulk-label">Snooze</span>
          <span class="bulk-caret" aria-hidden="true">▾</span>
        </button>
        {#if snoozeMenuOpen}
          <div class="bulk-menu" role="menu" aria-label="Snooze until">
            <button type="button" role="menuitem" class="bulk-menuitem" onclick={() => snoozeTo(laterToday())}>Later today</button>
            <button type="button" role="menuitem" class="bulk-menuitem" onclick={() => snoozeTo(tomorrow9())}>Tomorrow 9am</button>
            <button type="button" role="menuitem" class="bulk-menuitem" onclick={() => snoozeTo(nextWeekday9(6))}>This weekend</button>
            <button type="button" role="menuitem" class="bulk-menuitem" onclick={() => snoozeTo(nextWeekday9(1))}>Next week</button>
            <div class="bulk-menu-custom">
              <label class="bulk-menu-label" for="snooze-custom">Custom</label>
              <input id="snooze-custom" class="bulk-menu-input" type="datetime-local" bind:value={customSnooze} />
              <button type="button" class="bulk-menu-go" onclick={snoozeCustom} disabled={!customSnooze}>Snooze</button>
            </div>
          </div>
        {/if}
      </div>

      <button type="button" class="bulk-btn" aria-label="Flag" title="Flag" onclick={flag} disabled={controlsDisabled}>
        <svg class="bulk-ico" viewBox="0 0 16 16" aria-hidden="true"><path d="M4 2.2v11.6"/><path d="M4 3h7.5l-1.6 2.6L11.5 8H4"/></svg>
        <span class="bulk-label">Flag</span>
      </button>

      <div class="bulk-split">
        <button type="button" class="bulk-btn" aria-label="Junk" title="Junk" aria-haspopup="menu" aria-expanded={junkMenuOpen} onclick={() => (junkMenuOpen = !junkMenuOpen)} disabled={controlsDisabled}>
          <svg class="bulk-ico" viewBox="0 0 16 16" aria-hidden="true"><circle cx="8" cy="8" r="5.5"/><path d="M4.1 4.1l7.8 7.8"/></svg>
          <span class="bulk-label">Junk</span>
          <span class="bulk-caret" aria-hidden="true">▾</span>
        </button>
        {#if junkMenuOpen}
          <div class="bulk-menu" role="menu" aria-label="Junk options">
            <button type="button" role="menuitem" class="bulk-menuitem" aria-label="Move to Junk" onclick={junkOnly}>Move to Junk</button>
            <button type="button" role="menuitem" class="bulk-menuitem" aria-label="Move to Junk and block sender" onclick={openBlockConfirm}>Move to Junk &amp; block sender…</button>
          </div>
        {/if}
      </div>

      <button
        type="button"
        class="bulk-btn bulk-btn-danger"
        aria-label={inTrash ? 'Delete permanently' : 'Delete'}
        title={inTrash ? 'Delete permanently' : 'Move to Trash'}
        onclick={onDeleteClick}
        disabled={controlsDisabled}
      >
        <svg class="bulk-ico" viewBox="0 0 16 16" aria-hidden="true"><path d="M3 4h10"/><path d="M5 4l.6 9h4.8L11 4"/><path d="M6.5 4V2.6h3V4"/></svg>
        <span class="bulk-label">{inTrash ? 'Delete permanently' : 'Delete'}</span>
      </button>

      <div class="bulk-split">
        <button type="button" class="bulk-btn" aria-label="More" title="More actions" aria-haspopup="menu" aria-expanded={moreMenuOpen} onclick={() => (moreMenuOpen = !moreMenuOpen)} disabled={controlsDisabled}>
          <svg class="bulk-ico" viewBox="0 0 16 16" aria-hidden="true"><circle cx="3.5" cy="8" r="1.1"/><circle cx="8" cy="8" r="1.1"/><circle cx="12.5" cy="8" r="1.1"/></svg>
          <span class="bulk-label">More</span>
          <span class="bulk-caret" aria-hidden="true">▾</span>
        </button>
        {#if moreMenuOpen}
          <div class="bulk-menu bulk-menu-end" role="menu" aria-label="More actions">
            <button type="button" role="menuitem" class="bulk-menuitem" onclick={markRead}>Mark read</button>
            <button type="button" role="menuitem" class="bulk-menuitem" onclick={markUnread}>Mark unread</button>
            <button type="button" role="menuitem" class="bulk-menuitem" onclick={unflag}>Unflag</button>
            <button type="button" role="menuitem" class="bulk-menuitem" onclick={openMoveFolder}>Move…</button>
          </div>
        {/if}
      </div>

      {#if opRunning && opProgress}
        <span class="bulk-progress" aria-live="polite">{opProgress.done}/{opProgress.total}</span>
      {/if}
    </div>
  </div>
{/if}

<!-- Toast region -->
<div class="toast-region" aria-live="polite" aria-label="Notifications">
  {#each toasts as t (t.id)}
    <Toast variant={t.variant} onclose={() => dismissToast(t.id)}>
      {t.text}
      {#if t.retryOp && (t.retryItems || t.retryCompound)}
        <button type="button" class="toast-retry" onclick={() => retryFailed(t.id, t.retryItems, t.retryOp, t.retryCompound)}>Retry failed</button>
      {/if}
    </Toast>
  {/each}
</div>

<!-- Delete confirm modal — always shown; states count + account scope -->
<Modal
  open={deleteConfirmOpen}
  title={`Delete ${selection.count} message${selection.count === 1 ? '' : 's'}?`}
  onclose={closeDeleteConfirm}
>
  <p class="delete-warn">
    This permanently deletes {selection.count} message{selection.count === 1 ? '' : 's'} across {accountScopeText()}. You can't undo this.
  </p>
  {#if !simpleConfirm}
    <label class="delete-label" for="delete-confirm-input">
      Type <strong>{selection.count}</strong> to confirm
    </label>
    <input
      id="delete-confirm-input"
      class="delete-input"
      type="text"
      bind:value={deleteTyped}
      placeholder={String(selection.count)}
      autocomplete="off"
    />
  {/if}
  {#snippet footer()}
    <button type="button" class="modal-cancel" onclick={closeDeleteConfirm}>Cancel</button>
    <button
      type="button"
      class="modal-delete"
      onclick={confirmDelete}
      disabled={!simpleConfirm && deleteTyped !== String(selection.count)}
    >
      Delete {selection.count === 1 ? 'message' : 'messages'}
    </button>
  {/snippet}
</Modal>

<!-- Block-sender confirm modal — EXACT senders shown before any rule is created -->
<Modal
  open={blockConfirmOpen}
  title="Block sender & move to Junk"
  onclose={() => (blockConfirmOpen = false)}
>
  {#if blockSenders.length === 0}
    <p class="block-hint">
      None of the selected messages have a readable sender address to block. Use "Move to Junk"
      instead, or deselect these messages.
    </p>
  {:else}
    <p class="block-intro">
      This moves the selected message{selection.count === 1 ? '' : 's'} to Junk and creates an
      exact-sender rule for {blockSenders.length === 1 ? 'this address' : 'these addresses'} —
      future mail from {blockSenders.length === 1 ? 'it' : 'them'} routes straight to Junk. No domain
      is blocked.
    </p>
    <ul class="block-senders">
      {#each blockSenders as s (s)}
        <li><MonoTag>{s}</MonoTag></li>
      {/each}
    </ul>
    {#if hasInvalidSender}
      <p class="block-hint block-hint-warn">
        {invalidSenderItems.length} selected message{invalidSenderItems.length === 1 ? '' : 's'}
        {invalidSenderItems.length === 1 ? 'has' : 'have'} no readable sender and can't be blocked.
        Deselect {invalidSenderItems.length === 1 ? 'it' : 'them'} or use "Move to Junk" instead.
      </p>
    {/if}
  {/if}
  {#snippet footer()}
    <button type="button" class="modal-cancel" onclick={() => (blockConfirmOpen = false)}>Cancel</button>
    <button type="button" class="modal-delete" onclick={confirmBlockAndJunk} disabled={hasInvalidSender}>
      Block &amp; move to Junk
    </button>
  {/snippet}
</Modal>

<!-- Move folder picker modal -->
<Modal
  open={moveFolderPickerOpen}
  title="Move to folder"
  onclose={() => (moveFolderPickerOpen = false)}
>
  {#if folders.length === 0}
    <p class="move-hint">No folders available. Select a mailbox with IMAP access to move messages.</p>
  {:else}
    <div class="folder-list">
      {#each folders as f (f.folder)}
        <label class="folder-option">
          <input type="radio" name="move-folder" value={f.folder} bind:group={selectedMoveFolder} />
          <span class="folder-option-label">{f.folder}</span>
        </label>
      {/each}
    </div>
  {/if}
  {#snippet footer()}
    <button type="button" class="modal-cancel" onclick={() => (moveFolderPickerOpen = false)}>Cancel</button>
    <button type="button" class="modal-confirm" onclick={confirmMove} disabled={!selectedMoveFolder}>
      Move
    </button>
  {/snippet}
</Modal>

<style>
  .bulk-toolbar {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.4rem 0.75rem;
    background: var(--env-accent-soft);
    border-bottom: 1px solid var(--env-rule);
    flex-wrap: wrap;
    position: sticky;
    top: 0;
    z-index: 5;
  }
  .bulk-context {
    display: flex;
    align-items: center;
    gap: 0.25rem;
    flex-shrink: 0;
  }
  .bulk-count {
    font-size: 0.8125rem;
    font-weight: 600;
    color: var(--env-accent);
    flex-shrink: 0;
  }
  .bulk-actions {
    display: flex;
    align-items: center;
    gap: 0.25rem;
    flex-wrap: wrap;
    flex: 1;
  }
  .bulk-btn {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    font-size: 0.75rem;
    padding: 0.2rem 0.55rem;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-xs, 2px);
    background: var(--env-surface);
    color: var(--env-ink);
    cursor: pointer;
    white-space: nowrap;
  }
  .bulk-btn:hover:not(:disabled) {
    background: var(--env-paper);
    border-color: var(--env-accent);
  }
  .bulk-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .bulk-btn-danger {
    color: var(--env-warn);
    border-color: var(--env-warn);
  }
  .bulk-btn-danger:hover:not(:disabled) {
    background: var(--env-warn-soft);
    border-color: var(--env-warn);
  }
  .bulk-ico {
    width: 14px;
    height: 14px;
    flex-shrink: 0;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.4;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  .bulk-caret {
    font-size: 0.6rem;
    opacity: 0.7;
  }
  .bulk-split {
    position: relative;
    display: inline-flex;
  }
  .bulk-menu {
    position: absolute;
    top: calc(100% + 3px);
    left: 0;
    z-index: 20;
    min-width: 11rem;
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
    padding: 0.3rem;
    background: var(--env-surface);
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-sm, 3px);
    box-shadow: 0 6px 20px rgba(10, 10, 10, 0.12);
  }
  /* The More menu sits at the right end of the action row; anchor it to the
     button's right edge so it never overflows past the list column. */
  .bulk-menu-end {
    left: auto;
    right: 0;
  }
  .bulk-menuitem {
    text-align: left;
    font-size: 0.8125rem;
    padding: 0.3rem 0.5rem;
    border: none;
    border-radius: var(--radius-xs, 2px);
    background: none;
    color: var(--env-ink);
    cursor: pointer;
    white-space: nowrap;
  }
  .bulk-menuitem:hover {
    background: var(--env-accent-soft);
    color: var(--env-accent);
  }
  .bulk-menu-custom {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    padding: 0.3rem 0.5rem 0.15rem;
    border-top: 1px solid var(--env-rule);
    margin-top: 0.15rem;
    flex-wrap: wrap;
  }
  .bulk-menu-label {
    font-family: var(--font-mono);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--env-muted);
  }
  .bulk-menu-input {
    font-size: 0.75rem;
    padding: 0.1rem 0.25rem;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-xs, 2px);
    background: var(--env-surface);
    color: var(--env-ink);
    min-width: 0;
    flex: 1;
  }
  .bulk-menu-go {
    font-size: 0.75rem;
    padding: 0.15rem 0.45rem;
    border: 1px solid var(--env-accent);
    border-radius: var(--radius-xs, 2px);
    background: var(--env-accent);
    color: #fff;
    cursor: pointer;
  }
  .bulk-menu-go:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .bulk-progress {
    font-size: 0.75rem;
    font-family: var(--font-mono);
    color: var(--env-muted);
    flex-shrink: 0;
  }
  .bulk-deselect {
    background: none;
    border: none;
    font-size: 1.1rem;
    color: var(--env-muted);
    cursor: pointer;
    padding: 0 0.25rem;
    flex-shrink: 0;
    line-height: 1;
  }
  .bulk-deselect:hover {
    color: var(--env-ink);
  }
  .toast-region {
    position: fixed;
    bottom: 1rem;
    right: 1rem;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    z-index: 100;
    pointer-events: none;
  }
  .toast-region :global(.env-toast) {
    pointer-events: auto;
  }
  .toast-retry {
    background: none;
    border: none;
    padding: 0;
    margin-left: 0.5rem;
    font-size: 0.8125rem;
    font-weight: 600;
    color: var(--env-accent);
    cursor: pointer;
    text-decoration: underline;
  }
  .delete-warn {
    margin: 0 0 0.75rem;
    font-size: 0.875rem;
    color: var(--env-warn);
    line-height: 1.4;
  }
  .delete-label {
    display: block;
    font-size: 0.8125rem;
    margin-bottom: 0.4rem;
  }
  .delete-input {
    width: 100%;
    padding: 0.35rem 0.5rem;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-sm, 3px);
    font-size: 0.8125rem;
    font-family: var(--font-mono);
    background: var(--env-surface);
    color: var(--env-ink);
    box-sizing: border-box;
  }
  .delete-input:focus {
    outline: 2px solid color-mix(in srgb, var(--env-accent) 40%, transparent);
    border-color: var(--env-accent);
  }
  .block-intro {
    margin: 0 0 0.6rem;
    font-size: 0.8125rem;
    line-height: 1.45;
  }
  .block-hint-warn {
    margin-top: 0.6rem;
    color: var(--env-warn);
  }
  .block-hint,
  .move-hint {
    margin: 0;
    font-size: 0.875rem;
    color: var(--env-muted);
    line-height: 1.4;
  }
  .block-senders {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    max-height: 12rem;
    overflow-y: auto;
  }
  .modal-cancel,
  .modal-delete,
  .modal-confirm {
    font-size: 0.875rem;
    padding: 0.4rem 0.9rem;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-sm, 3px);
    cursor: pointer;
    background: var(--env-surface);
    color: var(--env-ink);
  }
  .modal-delete {
    background: var(--env-warn);
    border-color: var(--env-warn);
    color: #fff;
  }
  .modal-delete:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .modal-confirm {
    background: var(--env-accent);
    border-color: var(--env-accent);
    color: #fff;
  }
  .modal-confirm:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .modal-cancel:hover {
    background: var(--env-paper);
  }
  .folder-list {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    max-height: 18rem;
    overflow-y: auto;
  }
  .folder-option {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    cursor: pointer;
    padding: 0.25rem 0;
  }
  .folder-option-label {
    font-size: 0.875rem;
    font-family: var(--font-mono);
    color: var(--env-ink);
  }
  /* Phone widths: count + dismiss get their own legible context row, then the
     primary actions form one clean icon row with practical touch targets.
     Labels collapse to icon-only but every action keeps its aria-label/title,
     and the secondary actions live behind More — no orphaned text, no overflow. */
  @media (max-width: 760px) {
    .bulk-toolbar {
      gap: 0.5rem;
    }
    .bulk-context {
      width: 100%;
      justify-content: space-between;
    }
    .bulk-count {
      font-size: 0.875rem;
    }
    .bulk-actions {
      width: 100%;
      gap: 0.4rem;
    }
    .bulk-label {
      display: none;
    }
    /* 44px touch targets (Apple/Material minimum) for every primary action. */
    .bulk-btn {
      min-width: 44px;
      min-height: 44px;
      justify-content: center;
      padding: 0 0.5rem;
    }
    .bulk-caret {
      display: none;
    }
    .bulk-ico {
      width: 18px;
      height: 18px;
    }
    .bulk-deselect {
      min-width: 44px;
      min-height: 44px;
      font-size: 1.35rem;
    }
    /* Comfortable tap rows inside the dropdown menus, too. */
    .bulk-menuitem {
      min-height: 40px;
      display: flex;
      align-items: center;
      font-size: 0.9375rem;
    }
  }
</style>
