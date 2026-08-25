// Shared mailbox-operation signal.
//
// The message list stays mounted while the reader swaps the third column, so
// when the reader archives, trashes, deletes, or flags the open message the
// list must learn about it without a page reload. BulkToolbar already reports
// its operations to the layout through an `onoperated` prop; the reader is a
// nested route with no prop channel, so it bumps this runes-backed singleton
// and the layout re-runs the same refresh it runs for bulk operations.
// Mirrors `read-state.svelte.ts` / `selection.svelte.ts` / `live.svelte.ts`.

export class MailboxOpsStore {
  version = $state(0);

  /** Announce that a mailbox mutation completed; listeners re-fetch. */
  operated(): void {
    this.version += 1;
  }
}

let singleton: MailboxOpsStore | null = null;

export function getMailboxOpsStore(): MailboxOpsStore {
  if (!singleton) singleton = new MailboxOpsStore();
  return singleton;
}

/** Reset the singleton (tests / teardown). */
export function __resetMailboxOpsStore(): void {
  singleton = null;
}
