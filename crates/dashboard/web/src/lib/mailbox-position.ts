// Where a message sits in the loaded unified list. When the list is worth
// syncing is decided in mailbox-sync.svelte.ts.

/** Where a message sits in the loaded page, 1-based for display. */
export type ListPosition = { index: number; position: number; total: number };

/**
 * Locate `accountId:uid` in the loaded list.
 *
 * Both halves are required: IMAP UIDs are mailbox-scoped, so uid 10 in one
 * account is unrelated to uid 10 in another and matching on uid alone would
 * highlight the wrong row. `null` when the message is not on the loaded page —
 * the list shows the newest N, and a deep link can name something older.
 */
export function positionOf(
  messages: readonly { account_id: string; uid: number }[],
  accountId: string | null,
  uid: number | null
): ListPosition | null {
  if (!accountId || uid === null || uid === undefined) return null;
  const index = messages.findIndex((m) => m.account_id === accountId && m.uid === uid);
  if (index === -1) return null;
  return { index, position: index + 1, total: messages.length };
}
