<script lang="ts">
  // Contextual bulk-action toolbar. Hidden at 0 selected; shows count + actions.
  // Uses bulkClient() as the fallback until a server bulk endpoint ships.
  // Partial-failure UX: "14 archived, 2 failed" + retry-failed affordance.
  import type { SelectionStore } from '$lib/selection.svelte';
  import type { BulkItem, FolderStats } from '$lib/api';
  import { bulkClient } from '$lib/api';
  import Modal from './Modal.svelte';
  import Toast from './Toast.svelte';

  type ToastItem = { id: number; variant: 'ok' | 'warn' | 'danger'; text: string; retryItems?: BulkItem[]; retryOp?: 'archive' | 'delete' | 'spam' | 'read' | 'unread' | 'star' | 'unstar' };

  let {
    selection,
    folder = 'INBOX',
    folders = [],
    onoperated,
  }: {
    selection: SelectionStore;
    folder?: string;
    folders?: FolderStats[];
    onoperated?: () => void;
  } = $props();

  let toastSeq = $state(0);
  let toasts = $state<ToastItem[]>([]);

  let deleteConfirmOpen = $state(false);
  let deleteTyped = $state('');

  // Folder picker for Move action
  let moveFolderPickerOpen = $state(false);
  let selectedMoveFolder = $state('');

  // Whether a bulk op is running
  let opRunning = $state(false);
  let opProgress = $state<{ done: number; total: number } | null>(null);

  function selectedItems(): BulkItem[] {
    return Array.from(selection.selected).map((key) => {
      const [accountId, uid] = key.split(':');
      return { accountId, uid: Number(uid) };
    });
  }

  function addToast(variant: ToastItem['variant'], text: string, retryItems?: BulkItem[], retryOp?: ToastItem['retryOp']) {
    const id = ++toastSeq;
    toasts = [...toasts, { id, variant, text, retryItems, retryOp }];
    // Auto-dismiss ok toasts
    if (variant === 'ok') {
      setTimeout(() => dismissToast(id), 5000);
    }
  }

  function dismissToast(id: number) {
    toasts = toasts.filter((t) => t.id !== id);
  }

  function toastMessage(succeeded: number, failed: BulkItem[], action: string): ToastItem['variant'] {
    if (failed.length === 0) return 'ok';
    if (succeeded === 0) return 'danger';
    return 'warn';
  }

  async function runBulkOp(
    op: Parameters<typeof bulkClient>[0],
    items: BulkItem[],
    actionLabel: string,
    retryOp: ToastItem['retryOp']
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
    const variant = toastMessage(succeeded, failedItems, actionLabel);
    const text =
      result.failed.length === 0
        ? `${succeeded} ${actionLabel}`
        : `${succeeded} ${actionLabel}, ${result.failed.length} failed`;

    addToast(variant, text, failedItems.length > 0 ? failedItems : undefined, retryOp);
    if (result.failed.length === 0) {
      selection.deselectAll();
    }
    onoperated?.();
  }

  async function archive() {
    const items = selectedItems();
    await runBulkOp(
      { type: 'move', folder, to_folder: 'Archive' },
      items,
      items.length === 1 ? 'archived' : `archived`,
      'archive'
    );
  }

  async function markRead() {
    const items = selectedItems();
    await runBulkOp(
      { type: 'flags', folder, add: ['\\Seen'], remove: [] },
      items,
      'marked read',
      'read'
    );
  }

  async function markUnread() {
    const items = selectedItems();
    await runBulkOp(
      { type: 'flags', folder, add: [], remove: ['\\Seen'] },
      items,
      'marked unread',
      'unread'
    );
  }

  async function star() {
    const items = selectedItems();
    await runBulkOp(
      { type: 'flags', folder, add: ['\\Flagged'], remove: [] },
      items,
      'starred',
      'star'
    );
  }

  async function unstar() {
    const items = selectedItems();
    await runBulkOp(
      { type: 'flags', folder, add: [], remove: ['\\Flagged'] },
      items,
      'unstarred',
      'unstar'
    );
  }

  async function spam() {
    const items = selectedItems();
    await runBulkOp(
      { type: 'move', folder, to_folder: 'Junk' },
      items,
      'moved to spam',
      'spam'
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

  const NEEDS_TYPED_CONFIRM = 10;
  const simpleConfirm = $derived(selection.count <= NEEDS_TYPED_CONFIRM);

  async function confirmDelete() {
    if (!simpleConfirm && deleteTyped !== String(selection.count)) return;
    closeDeleteConfirm();
    const items = selectedItems();
    await runBulkOp({ type: 'delete', folder }, items, 'deleted', 'delete');
  }

  function openMoveFolder() {
    selectedMoveFolder = '';
    moveFolderPickerOpen = true;
  }

  async function confirmMove() {
    if (!selectedMoveFolder) return;
    moveFolderPickerOpen = false;
    const items = selectedItems();
    await runBulkOp(
      { type: 'move', folder, to_folder: selectedMoveFolder },
      items,
      `moved to ${selectedMoveFolder}`,
      undefined
    );
  }

  async function retryFailed(items: BulkItem[], op: ToastItem['retryOp']) {
    if (!op) return;
    if (op === 'archive') {
      await runBulkOp({ type: 'move', folder, to_folder: 'Archive' }, items, 'archived (retry)', 'archive');
    } else if (op === 'delete') {
      await runBulkOp({ type: 'delete', folder }, items, 'deleted (retry)', 'delete');
    } else if (op === 'spam') {
      await runBulkOp({ type: 'move', folder, to_folder: 'Junk' }, items, 'moved to spam (retry)', 'spam');
    } else if (op === 'read') {
      await runBulkOp({ type: 'flags', folder, add: ['\\Seen'] }, items, 'marked read (retry)', 'read');
    } else if (op === 'unread') {
      await runBulkOp({ type: 'flags', folder, remove: ['\\Seen'] }, items, 'marked unread (retry)', 'unread');
    } else if (op === 'star') {
      await runBulkOp({ type: 'flags', folder, add: ['\\Flagged'] }, items, 'starred (retry)', 'star');
    } else if (op === 'unstar') {
      await runBulkOp({ type: 'flags', folder, remove: ['\\Flagged'] }, items, 'unstarred (retry)', 'unstar');
    }
  }
</script>

{#if selection.count > 0}
  <div id="bulk-toolbar" class="bulk-toolbar" role="toolbar" aria-label="Bulk actions">
    <span class="bulk-count">{selection.count} selected</span>

    <div class="bulk-actions">
      <button type="button" class="bulk-btn" onclick={archive} disabled={opRunning}>Archive</button>
      <button type="button" class="bulk-btn" onclick={openMoveFolder} disabled={opRunning}>Move…</button>
      <button type="button" class="bulk-btn" onclick={markRead} disabled={opRunning}>Mark read</button>
      <button type="button" class="bulk-btn" onclick={markUnread} disabled={opRunning}>Mark unread</button>
      <button type="button" class="bulk-btn" onclick={star} disabled={opRunning}>Star</button>
      <button type="button" class="bulk-btn" onclick={unstar} disabled={opRunning}>Unstar</button>
      <button type="button" class="bulk-btn" onclick={spam} disabled={opRunning}>Spam</button>
      <button type="button" class="bulk-btn bulk-btn-danger" onclick={openDeleteConfirm} disabled={opRunning}>Delete</button>
    </div>

    {#if opRunning && opProgress}
      <span class="bulk-progress" aria-live="polite">
        {opProgress.done}/{opProgress.total}
      </span>
    {/if}

    <button type="button" class="bulk-deselect" onclick={() => selection.deselectAll()} aria-label="Clear selection">×</button>
  </div>
{/if}

<!-- Toast region -->
<div class="toast-region" aria-live="polite" aria-label="Notifications">
  {#each toasts as t (t.id)}
    <Toast variant={t.variant} onclose={() => dismissToast(t.id)}>
      {t.text}
      {#if t.retryItems && t.retryOp}
        <button type="button" class="toast-retry" onclick={() => retryFailed(t.retryItems!, t.retryOp)}>Retry failed</button>
      {/if}
    </Toast>
  {/each}
</div>

<!-- Delete confirm modal -->
<Modal
  open={deleteConfirmOpen}
  title={simpleConfirm ? `Delete ${selection.count} message${selection.count === 1 ? '' : 's'}?` : 'Confirm bulk delete'}
  onclose={closeDeleteConfirm}
>
  {#if simpleConfirm}
    This will permanently delete {selection.count} message{selection.count === 1 ? '' : 's'}. You can't undo this.
  {:else}
    <p class="delete-warn">You are about to permanently delete {selection.count} messages. This cannot be undone.</p>
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
    <button
      type="button"
      class="modal-confirm"
      onclick={confirmMove}
      disabled={!selectedMoveFolder}
    >
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
  }
  .bulk-count {
    font-size: 0.8125rem;
    font-weight: 600;
    color: var(--env-accent);
    margin-right: 0.25rem;
    flex-shrink: 0;
  }
  .bulk-actions {
    display: flex;
    gap: 0.25rem;
    flex-wrap: wrap;
    flex: 1;
  }
  .bulk-btn {
    font-size: 0.75rem;
    padding: 0.2rem 0.6rem;
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
    padding: 0;
    margin-left: auto;
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
  .move-hint {
    margin: 0;
    font-size: 0.875rem;
    color: var(--env-muted);
    line-height: 1.4;
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
</style>
