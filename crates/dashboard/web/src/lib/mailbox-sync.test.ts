// Mailbox sync controller (#171): coalescing, truthful status, and the pure
// rules that keep a synced view from regressing or holding stale handles.
import { describe, expect, it, vi } from 'vitest';
import {
  MailboxSync,
  autoSyncDue,
  isNewerView,
  staleSelectionKeys,
  AUTO_SYNC_RECENT_MS
} from './mailbox-sync.svelte';
import type { UnifiedInboxResponse } from './api';

const T0 = '2026-09-29T10:00:00.000000Z';
const at = (ms: number) => new Date(Date.parse(T0) + ms).toISOString();

function view(over: Partial<UnifiedInboxResponse> = {}): UnifiedInboxResponse {
  return {
    scope: 'unified_inbox',
    status: 'ok',
    folder: 'INBOX',
    limit: 50,
    messages: [],
    accounts: [
      { account_id: 'a', account_username: 'a@x', ok: true, freshness: 'fresh', indexed_at: T0 },
      { account_id: 'b', account_username: 'b@x', ok: true, freshness: 'fresh', indexed_at: T0 }
    ],
    unread_count: 0,
    freshness: 'fresh',
    errors: [],
    next_cursor: null,
    generated_at: at(1000),
    ...over
  };
}

function synced(status: 'ok' | 'partial' | 'error', over: Partial<UnifiedInboxResponse> = {}) {
  const failed = status === 'ok' ? [] : status === 'partial' ? ['b'] : ['a', 'b'];
  return view({
    ...over,
    accounts: ['a', 'b'].map((id) => ({
      account_id: id,
      account_username: `${id}@x`,
      ok: !failed.includes(id),
      freshness: failed.includes(id) ? 'stale' : 'fresh',
      indexed_at: T0,
      error: failed.includes(id) ? 'IMAP: auth failed' : null
    })),
    errors: failed.map((id) => ({
      account_id: id,
      account_username: `${id}@x`,
      account_display_name: null,
      folder: 'INBOX',
      error: 'IMAP: auth failed'
    })),
    sync: {
      target: 'inbox',
      account_id: null,
      status,
      started_at: at(0),
      finished_at: at(2000),
      accounts: ['a', 'b'].map((id) => ({
        account_id: id,
        account_username: `${id}@x`,
        ok: !failed.includes(id),
        joined: false,
        error: failed.includes(id) ? 'IMAP: auth failed' : null
      }))
    }
  });
}

function deferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

describe('autoSyncDue — which opens start a provider sync', () => {
  it('cold open (never synced) is due', () => {
    expect(
      autoSyncDue(
        view({ accounts: [{ account_id: 'a', account_username: 'a', ok: false, freshness: 'unavailable', indexed_at: null }] })
      )
    ).toBe(true);
  });

  it('a cache the server calls fresh is still due once it is older than the recent window', () => {
    // 5 minutes old is "fresh" server-side; an open still syncs (#171).
    expect(autoSyncDue(view({ generated_at: at(5 * 60_000) }))).toBe(true);
  });

  it('an open right after a sync in any tab coalesces into it', () => {
    expect(autoSyncDue(view({ generated_at: at(AUTO_SYNC_RECENT_MS - 1) }))).toBe(false);
  });

  it('an account whose last sync failed keeps it due', () => {
    const res = view();
    res.accounts[1] = { ...res.accounts[1], ok: false, error: 'IMAP: auth', freshness: 'stale' };
    expect(autoSyncDue(res)).toBe(true);
  });

  it('no accounts means nothing to sync', () => {
    expect(autoSyncDue(view({ accounts: [] }))).toBe(false);
  });
});

describe('isNewerView — no old response overwrites a newer view', () => {
  it('accepts the first view and anything read later', () => {
    expect(isNewerView(null, at(0))).toBe(true);
    expect(isNewerView(at(0), at(1))).toBe(true);
    expect(isNewerView(at(5), at(5))).toBe(true);
  });
  it('rejects a response read before the one already shown', () => {
    expect(isNewerView(at(10), at(5))).toBe(false);
  });
  it('treats a response without a timestamp as not comparable (accept)', () => {
    expect(isNewerView(at(10), undefined)).toBe(true);
  });
});

describe('staleSelectionKeys — no stale action handles after a sync', () => {
  const row = (account_id: string, uid: number, uidvalidity = 1) => ({ account_id, uid, uidvalidity });
  const key = (m: { account_id: string; uid: number }) => `${m.account_id}:${m.uid}`;

  it('keeps selections that still exist', () => {
    const prev = [row('a', 1), row('a', 2)];
    expect(staleSelectionKeys(prev, prev, new Set(['a:1']), key)).toEqual([]);
  });

  it('drops selections of deleted or moved messages', () => {
    const prev = [row('a', 1), row('a', 2)];
    const next = [row('a', 2)];
    expect(staleSelectionKeys(prev, next, new Set(['a:1', 'a:2']), key)).toEqual(['a:1']);
  });

  it('drops every selection of an account whose UIDVALIDITY reset, even if the UID reappears', () => {
    const prev = [row('a', 1, 1), row('b', 1, 1)];
    const next = [row('a', 1, 2), row('b', 1, 1)];
    expect(staleSelectionKeys(prev, next, new Set(['a:1', 'b:1']), key)).toEqual(['a:1']);
  });

  it('never drops a selection whose row was not on the synced page for a reason other than removal', () => {
    // Keys it does not know about (another surface) are left alone.
    expect(staleSelectionKeys([row('a', 1)], [row('a', 1)], new Set(['search:a:9']), key)).toEqual([]);
  });
});

describe('MailboxSync controller', () => {
  it('coalesces repeated clicks into one in-flight sync per scope', async () => {
    const d = deferred<UnifiedInboxResponse>();
    const refresh = vi.fn(() => d.promise);
    const sync = new MailboxSync(refresh);

    const first = sync.sync('unified');
    const second = sync.sync('unified');
    expect(refresh).toHaveBeenCalledTimes(1);
    expect(sync.state('unified').phase).toBe('syncing');

    d.resolve(synced('ok'));
    expect(await first).toBe(await second);
    expect(sync.state('unified').phase).toBe('synced');
    expect(sync.state('unified').lastSuccessAt).toBe(at(2000));
  });

  it('a scoped retry joins a running full sync instead of starting another', async () => {
    const d = deferred<UnifiedInboxResponse>();
    const refresh = vi.fn(() => d.promise);
    const sync = new MailboxSync(refresh);
    sync.sync('unified');
    sync.sync('unified', 'b');
    expect(refresh).toHaveBeenCalledTimes(1);
    d.resolve(synced('ok'));
  });

  it('scopes are independent: Sent does not join an Inbox sync', () => {
    const refresh = vi.fn((_scope: string) => new Promise<UnifiedInboxResponse>(() => {}));
    const sync = new MailboxSync(refresh);
    sync.sync('unified');
    sync.sync('sent');
    expect(refresh.mock.calls.map((c) => c[0])).toEqual(['unified', 'sent']);
  });

  it('partial failure is reported per account, never as synced', async () => {
    const sync = new MailboxSync(vi.fn(async () => synced('partial')));
    await sync.sync('unified');
    const s = sync.state('unified');
    expect(s.phase).toBe('partial');
    expect(s.failures.map((f) => f.account_username)).toEqual(['b@x']);
    // The scope as a whole did not sync; no success time is claimed for it.
    expect(s.lastSuccessAt).toBeNull();
  });

  it('a request failure is surfaced with its message and clears the in-flight state', async () => {
    const refresh = vi
      .fn()
      .mockRejectedValueOnce(new Error('request failed (502)'))
      .mockResolvedValueOnce(synced('ok'));
    const sync = new MailboxSync(refresh);
    await expect(sync.sync('unified')).resolves.toBeNull();
    expect(sync.state('unified')).toMatchObject({ phase: 'failed', error: 'request failed (502)' });
    // Retry is a fresh run.
    await sync.sync('unified');
    expect(refresh).toHaveBeenCalledTimes(2);
    expect(sync.state('unified').phase).toBe('synced');
  });

  it('a successful scoped retry clears only that account and completes the scope', async () => {
    const refresh = vi
      .fn()
      .mockResolvedValueOnce(synced('partial'))
      .mockResolvedValueOnce(
        view({
          generated_at: at(9000),
          sync: {
            target: 'inbox',
            account_id: 'b',
            status: 'ok',
            started_at: at(8000),
            finished_at: at(9000),
            accounts: [{ account_id: 'b', account_username: 'b@x', ok: true, joined: false, error: null }]
          }
        })
      );
    const sync = new MailboxSync(refresh);
    await sync.sync('unified');
    await sync.sync('unified', 'b');
    expect(refresh.mock.calls[1]).toEqual(['unified', 'b']);
    expect(sync.state('unified').failures).toEqual([]);
    expect(sync.state('unified').phase).toBe('synced');
  });

  it('observing a cached GET reports last success from the server, but never claims a sync ran', () => {
    const sync = new MailboxSync(vi.fn());
    sync.observe('unified', view());
    const s = sync.state('unified');
    expect(s.phase).toBe('idle');
    expect(s.lastSuccessAt).toBe(T0);
  });

  it('observing cached errors lists them as stale accounts to retry', () => {
    const sync = new MailboxSync(vi.fn());
    sync.observe(
      'unified',
      view({
        errors: [{ account_id: 'b', account_username: 'b@x', account_display_name: null, folder: 'INBOX', error: 'IMAP: auth failed' }]
      })
    );
    expect(sync.state('unified').failures).toEqual([
      { account_id: 'b', account_username: 'b@x', error: 'IMAP: auth failed' }
    ]);
  });
});
