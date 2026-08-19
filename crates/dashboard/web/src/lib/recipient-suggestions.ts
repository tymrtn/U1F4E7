// Recipient autocomplete fetching for the compose surfaces.
//
// Two problems this solves that a bare `await api.addressSuggestions(...)` in
// a keydown handler does not:
//
//   • Stale responses. Typing "a", "ad", "ada" starts three requests. Whichever
//     the network returns last would win and repaint the dropdown with results
//     for a prefix the operator has already moved past. Every search carries a
//     sequence number and the loser resolves to `null` — the caller then knows
//     to leave the UI alone entirely, rather than guessing.
//   • Re-fetching what we just fetched. Backspacing through a word walks back
//     over prefixes already answered, so a small LRU makes those keystrokes
//     render with no request at all.
//
// The cache is per-session and per-(account, query), and holds ONLY non-empty
// answers. A miss is the one answer the backend routinely stops giving: a send
// folds its recipients into address history the moment it is durable, so the
// prefix that matched nothing a minute ago is exactly the prefix that now
// matches the person you just wrote to. Caching that miss would hide them
// until the tab was reloaded — the case autocomplete exists for. Re-asking on
// a miss costs one local SQLite read and needs no invalidation plumbing.
//
// A non-empty answer goes stale the same way, just less visibly: the send that
// teaches the backend a new address also lands under prefixes that already
// matched somebody, and that list stays one send behind. So entries expire
// too. The window is short because the endpoint is a local SQLite read rather
// than a network round trip — re-asking is cheap, and holding a stale list is
// the expensive mistake.

import { api, type AddressSuggestion } from '$lib/api';

/** Distinct (account, query) pairs held before the oldest is evicted. */
const CACHE_LIMIT = 128;

/**
 * How long a non-empty answer may be reused before it must be asked again.
 *
 * Long enough to absorb the burst of keystrokes the cache exists for — typing
 * and backspacing through one word — and short enough that a recipient learned
 * from a send appears without the operator wondering why they have to reload.
 */
export const SUGGESTION_TTL_MS = 2000;

interface CacheEntry {
  rows: AddressSuggestion[];
  /** `Date.now()` past which these rows must not be served. */
  expiresAt: number;
}

const cache = new Map<string, CacheEntry>();

function cacheKey(accountId: string, query: string): string {
  // Both halves are free-form, so the key has to record where one ends and the
  // other begins. JSON does that in printable characters; a bare separator
  // character lets ("acc", "a b") and ("acc a", "b") collide.
  return JSON.stringify([accountId, query.toLowerCase()]);
}

/** Rows for this key, dropping the entry instead if its window has closed. */
function unexpired(key: string): AddressSuggestion[] | undefined {
  const entry = cache.get(key);
  if (entry === undefined) return undefined;
  if (Date.now() >= entry.expiresAt) {
    cache.delete(key);
    return undefined;
  }
  return entry.rows;
}

function remember(key: string, rows: AddressSuggestion[]): void {
  if (cache.size >= CACHE_LIMIT) {
    const oldest = cache.keys().next().value;
    if (oldest !== undefined) cache.delete(oldest);
  }
  cache.set(key, { rows, expiresAt: Date.now() + SUGGESTION_TTL_MS });
}

/** Drop every cached answer. Tests and account teardown. */
export function resetSuggestionCache(): void {
  cache.clear();
}

/** How a suggester talks to the backend. Injectable so tests stay offline. */
export type SuggestionFetcher = (
  accountId: string,
  query: string,
  limit: number,
  signal: AbortSignal
) => Promise<AddressSuggestion[]>;

const defaultFetcher: SuggestionFetcher = async (accountId, query, limit, signal) => {
  const res = await api.addressSuggestions(accountId, query, limit, { signal });
  return res.suggestions;
};

export interface Suggester {
  /**
   * Cached rows for this prefix, or `undefined` when there is no cached
   * answer — never asked, last answered with nothing, or answered longer than
   * `SUGGESTION_TTL_MS` ago.
   */
  cached(accountId: string, query: string): AddressSuggestion[] | undefined;
  /**
   * Rows for this prefix, or `null` when a newer search has already started —
   * a `null` result must not touch any UI state, including loading flags.
   */
  search(accountId: string, query: string, limit?: number): Promise<AddressSuggestion[] | null>;
  /** Abandon the in-flight search (field blurred, dropdown closed). */
  cancel(): void;
}

/**
 * Create an independent suggester. One per recipient field, so To, Cc, and Bcc
 * never cancel each other's requests.
 */
export function createSuggester(fetcher: SuggestionFetcher = defaultFetcher): Suggester {
  let controller: AbortController | null = null;
  let latest = 0;

  return {
    cached(accountId, query) {
      return unexpired(cacheKey(accountId, query));
    },

    async search(accountId, query, limit = 8) {
      const id = ++latest;
      controller?.abort();
      const current = new AbortController();
      controller = current;

      const key = cacheKey(accountId, query);
      const hit = unexpired(key);
      if (hit) return id === latest ? hit : null;

      try {
        const rows = await fetcher(accountId, query, limit, current.signal);
        if (id !== latest) return null;
        // A miss is never remembered, so the next send makes its recipients
        // reachable from this same prefix. See the note at the top.
        if (rows.length > 0) remember(key, rows);
        return rows;
      } catch (e) {
        // An abort is this module superseding itself, not a failure worth
        // surfacing. Anything else is a real error and belongs to the caller.
        if (id !== latest || current.signal.aborted) return null;
        throw e;
      }
    },

    cancel() {
      latest += 1;
      controller?.abort();
      controller = null;
    }
  };
}
