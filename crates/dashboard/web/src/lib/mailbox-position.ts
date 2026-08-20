// Where a message sits in the loaded unified list, and when that list is
// worth re-fetching.

/** The subset of an account entry these decisions read. */
type AccountFreshness = { freshness?: unknown };

/** The subset of the unified response these decisions read. */
type UnifiedShape = {
  freshness: string;
  accounts: readonly unknown[];
  messages: readonly unknown[];
};

function cacheIsStale(account: unknown): boolean {
  if (!account || typeof account !== 'object') return false;
  const freshness = (account as AccountFreshness).freshness;
  return freshness === 'stale' || freshness === 'expired';
}

/**
 * Whether the painted unified inbox should be re-fetched from IMAP.
 *
 * Two triggers, and the second exists because of a real outage:
 *
 * 1. **Something is stale.** A stale/expired cache is known-lagging, so refresh
 *    it. Top-level `partial` alone is NOT a trigger — it is the steady state
 *    when a couple of accounts are permanently unreachable, and refreshing on
 *    it would re-IMAP the whole fleet on every single page open.
 *
 * 2. **The list is empty but accounts exist.** An account whose index row
 *    carries a `last_error` reports zero messages no matter how many rows are
 *    actually indexed behind it (`message_count` is forced to 0 while the error
 *    is active). When every account carries one, the dashboard renders "Inbox
 *    is empty" over a full index and — before this — never retried, because
 *    those accounts report `unavailable`, which trigger 1 deliberately ignores.
 *    An empty list with connected accounts is the one shape worth one IMAP
 *    round-trip to disprove.
 *
 * Trigger 2 cannot loop: a successful refresh either produces messages or
 * proves the mailbox is genuinely empty, and the caller runs this once per
 * load rather than per render.
 */
export function unifiedNeedsRefresh(res: UnifiedShape): boolean {
  if (res.freshness === 'stale' || res.freshness === 'expired') return true;
  if (Array.isArray(res.accounts) && res.accounts.some(cacheIsStale)) return true;

  const blankedOut =
    res.messages.length === 0 &&
    Array.isArray(res.accounts) &&
    res.accounts.length > 0 &&
    // A fleet that all reports a healthy empty mailbox really is empty.
    !res.accounts.every(
      (a) =>
        a && typeof a === 'object' && (a as AccountFreshness).freshness === 'empty'
    );
  return blankedOut;
}

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
