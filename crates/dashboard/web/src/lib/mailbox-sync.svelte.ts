// Mailbox sync state for the list views (#171).
//
// A mailbox view paints from the server's cached index first, then asks the
// server for one read-only provider sync of what is visible. This module owns
// the client half of that contract:
//
// - one in-flight sync per view: repeated clicks, and a per-account Retry
//   issued while the whole view is syncing, share the running request (the
//   server additionally coalesces across tabs);
// - truthful status: only a response carrying a sync report can say
//   "synced", a partial failure names its accounts, and a failed request
//   surfaces its message instead of being swallowed;
// - the pure rules the layout applies when a response lands: whether an open
//   should sync at all, whether a response is older than the view on screen,
//   and which selections point at messages that no longer exist.

import type { UnifiedInboxResponse } from './api';

export type SyncScope = 'unified' | 'sent';

/** An open within this long of every account's last successful sync joins
 *  that sync rather than starting another — another tab, or a back/forward
 *  hop, just did the work. Far shorter than the server's 5-minute "fresh",
 *  so an ordinary open still syncs even when the cache looks acceptable. */
export const AUTO_SYNC_RECENT_MS = 30_000;

export interface SyncFailure {
  account_id: string;
  account_username: string;
  error: string;
}

export interface ScopeSyncState {
  phase: 'idle' | 'syncing' | 'synced' | 'partial' | 'failed';
  /** When every account in the view last synced successfully (server clock);
   *  null when an account has never synced or its last sync failed. */
  lastSuccessAt: string | null;
  /** Accounts whose last sync failed; their cached rows are shown as stale. */
  failures: SyncFailure[];
  /** Request-level failure (the sync request itself did not complete). */
  error: string | null;
  /** The account a scoped retry is running for, if any. */
  retryingAccount: string | null;
}

export type SyncRequest = (
  scope: SyncScope,
  accountId?: string
) => Promise<UnifiedInboxResponse>;

function idle(): ScopeSyncState {
  return { phase: 'idle', lastSuccessAt: null, failures: [], error: null, retryingAccount: null };
}

function parse(ts: string | null | undefined): number | null {
  if (!ts) return null;
  const ms = Date.parse(ts);
  return Number.isNaN(ms) ? null : ms;
}

/** The oldest last-success across the view's accounts, or null when any
 *  account has not synced successfully. */
function viewLastSuccess(res: UnifiedInboxResponse): string | null {
  const accounts = res.accounts ?? [];
  if (accounts.length === 0) return null;
  let oldest: { ms: number; ts: string } | null = null;
  for (const a of accounts) {
    if (a.error) return null;
    const ms = parse(a.indexed_at);
    if (ms === null || !a.indexed_at) return null;
    if (!oldest || ms < oldest.ms) oldest = { ms, ts: a.indexed_at };
  }
  return oldest?.ts ?? null;
}

function failuresOf(res: UnifiedInboxResponse): SyncFailure[] {
  return (res.errors ?? []).map((e) => ({
    account_id: e.account_id,
    account_username: e.account_username,
    error: e.error
  }));
}

/**
 * Whether opening a view with this cached response should start a provider
 * sync. It should, unless every account in the view synced successfully
 * within `AUTO_SYNC_RECENT_MS` of the server reading the index (both
 * timestamps are server clock, so browser clock skew cannot suppress a sync).
 */
export function autoSyncDue(res: UnifiedInboxResponse): boolean {
  const accounts = res.accounts ?? [];
  if (accounts.length === 0) return false;
  const now = parse(res.generated_at);
  if (now === null) return true;
  return !accounts.every((a) => {
    const last = parse(a.indexed_at);
    return a.ok && !a.error && last !== null && now - last < AUTO_SYNC_RECENT_MS;
  });
}

/** True when `next` was read no earlier than the view already shown. */
export function isNewerView(shown: string | null, next: string | undefined): boolean {
  const a = parse(shown);
  const b = parse(next);
  if (a === null || b === null) return true;
  if (b !== a) return b > a;
  // Same millisecond: the server stamps microseconds in a fixed-width form.
  return (next ?? '') >= (shown ?? '');
}

type RowIdentity = { account_id: string; uid: number; uidvalidity: number };

/**
 * Selected keys that no longer name the same message after `next` replaced
 * `prev`: the row is gone (deleted, moved), or its account's UIDVALIDITY
 * changed, which makes every old UID for that account meaningless even when
 * the number reappears. Keys `prev` never produced are left alone.
 */
export function staleSelectionKeys(
  prev: readonly RowIdentity[],
  next: readonly RowIdentity[],
  selected: ReadonlySet<string>,
  keyOf: (m: RowIdentity) => string
): string[] {
  const nextByKey = new Map(next.map((m) => [keyOf(m), m]));
  const nextValidity = new Map<string, number>();
  for (const m of next) nextValidity.set(m.account_id, m.uidvalidity);
  const stale: string[] = [];
  for (const m of prev) {
    const key = keyOf(m);
    if (!selected.has(key)) continue;
    const validity = nextValidity.get(m.account_id);
    const reset = validity !== undefined && validity !== m.uidvalidity;
    if (reset || !nextByKey.has(key)) stale.push(key);
  }
  return stale;
}

export class MailboxSync {
  private states = $state<Record<SyncScope, ScopeSyncState>>({ unified: idle(), sent: idle() });
  private inflight = new Map<string, Promise<UnifiedInboxResponse | null>>();

  constructor(private readonly request: SyncRequest) {}

  state(scope: SyncScope): ScopeSyncState {
    return this.states[scope];
  }

  isSyncing(scope: SyncScope): boolean {
    return this.states[scope].phase === 'syncing';
  }

  private patch(scope: SyncScope, next: Partial<ScopeSyncState>) {
    this.states = { ...this.states, [scope]: { ...this.states[scope], ...next } };
  }

  /** Fold a cached (GET) response into the status. It never claims a sync
   *  happened; it reports the server's last-success time and stale accounts. */
  observe(scope: SyncScope, res: UnifiedInboxResponse) {
    if (this.isSyncing(scope)) return;
    this.patch(scope, { lastSuccessAt: viewLastSuccess(res), failures: failuresOf(res) });
  }

  /**
   * Start (or join) a read-only provider sync for the view, optionally scoped
   * to one account. Resolves to the refreshed view, or null when the request
   * failed — the failure is recorded in the state, never swallowed.
   */
  sync(scope: SyncScope, accountId?: string): Promise<UnifiedInboxResponse | null> {
    const whole = this.inflight.get(`${scope}:*`);
    if (whole) return whole;
    const key = `${scope}:${accountId ?? '*'}`;
    const running = this.inflight.get(key);
    if (running) return running;

    this.patch(scope, { phase: 'syncing', error: null, retryingAccount: accountId ?? null });
    const run = this.request(scope, accountId)
      .then((res) => {
        this.finish(scope, res);
        return res;
      })
      .catch((e: unknown) => {
        const message = e instanceof Error && e.message ? e.message : 'Sync request failed.';
        this.patch(scope, { phase: 'failed', error: message, retryingAccount: null });
        return null;
      })
      .finally(() => {
        this.inflight.delete(key);
      });
    this.inflight.set(key, run);
    return run;
  }

  private finish(scope: SyncScope, res: UnifiedInboxResponse) {
    const failures = failuresOf(res);
    const report = res.sync;
    const wholeViewOk = report?.status === 'ok' && !report.account_id;
    const lastSuccessAt = wholeViewOk ? report.finished_at : viewLastSuccess(res);
    let phase: ScopeSyncState['phase'];
    if (!report) phase = failures.length > 0 ? 'partial' : 'idle';
    else if (report.status === 'error' && !report.account_id) phase = 'failed';
    else phase = failures.length > 0 ? 'partial' : 'synced';
    this.patch(scope, {
      phase,
      failures,
      lastSuccessAt,
      error: phase === 'failed' ? 'No account could be synced' : null,
      retryingAccount: null
    });
  }
}
