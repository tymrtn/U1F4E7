// Mailbox layout sync behavior (#171): cached first paint, one scoped sync
// per open, Sync now / Retry feedback, and views that never regress.
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { tick } from 'svelte';

import { page as pageState } from '$app/state';

const { apiMock, readerApiMock, liveMock } = vi.hoisted(() => {
  const handlers: Record<string, Array<() => void>> = { new_mail: [], lagged: [] };
  return {
    apiMock: {
      listAccounts: vi.fn(),
      cockpit: vi.fn(),
      stats: vi.fn(),
      folders: vi.fn(),
      unifiedInbox: vi.fn(),
      refreshUnifiedInbox: vi.fn(),
      sentInbox: vi.fn(),
      refreshSentInbox: vi.fn(),
      searchMessages: vi.fn(),
      message: vi.fn()
    },
    readerApiMock: { fetchMessageDetail: vi.fn(), fetchThread: vi.fn(), postFlags: vi.fn() },
    liveMock: {
      handlers,
      store: {
        connection: 'open',
        degraded: false,
        laggedTicks: 0,
        on: (_types: string[], h: () => void) => {
          handlers.new_mail.push(h);
          return () => {};
        },
        onLagged: (h: () => void) => {
          handlers.lagged.push(h);
          return () => {};
        }
      }
    }
  };
});
vi.mock('$lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/api')>();
  return { ...actual, api: apiMock };
});
vi.mock('$lib/reader-api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/reader-api')>();
  return { ...actual, ...readerApiMock };
});
vi.mock('$lib/live.svelte', () => ({ getLiveStore: () => liveMock.store }));

import MailLayout from '../../routes/mail/[box]/+layout.svelte';
import { createRawSnippet } from 'svelte';
const emptyChildren = createRawSnippet(() => ({ render: () => '<span></span>' }));

const NOW = Date.parse('2026-09-29T10:00:00Z');
const iso = (offsetMs: number) => new Date(NOW + offsetMs).toISOString();

const ACCT = { id: 'acct-a', name: 'A', username: 'a@example.com', domain: 'example.com', smtp_host: 's', smtp_port: 1, imap_host: 'i', imap_port: 1, display_name: 'A' };

type Row = ReturnType<typeof row>;
function row(account: string, uid: number, subject: string, opts: { unread?: boolean; uidvalidity?: number; folder?: string } = {}) {
  return {
    uid, message_id: `<${uid}@x>`, from_addr: 'p@example.com', to_addr: 'me@example.com',
    subject, date: '2026-09-29T09:00:00Z', flags: opts.unread ? [] : ['\\Seen'], size: 10,
    unread: opts.unread ?? false, account_id: account, account_username: `${account}@example.com`,
    account_display_name: null, folder: opts.folder ?? 'INBOX', uidvalidity: opts.uidvalidity ?? 1,
    snippet: null, thread_id: null, indexed_at: iso(0), index_freshness: 'fresh', date_epoch: 1_790_000_000 - uid
  };
}

function acct(id: string, indexedAt: string | null, error: string | null = null) {
  return {
    account_id: id, account_username: `${id}@example.com`, ok: error === null && indexedAt !== null,
    freshness: error ? 'stale' : indexedAt ? 'fresh' : 'unavailable', indexed_at: indexedAt, error
  };
}

function viewOf(rows: Row[], opts: { generatedAt?: string; accounts?: ReturnType<typeof acct>[]; sync?: unknown; scope?: string } = {}) {
  const accounts = opts.accounts ?? [acct('acct-a', iso(-10 * 60_000)), acct('acct-b', iso(-10 * 60_000))];
  return {
    scope: opts.scope ?? 'unified_inbox', status: 'ok', folder: 'INBOX', limit: 50, unread_count: 0,
    freshness: 'fresh', accounts,
    errors: accounts.filter((a) => a.error).map((a) => ({ account_id: a.account_id, account_username: a.account_username, account_display_name: null, folder: 'INBOX', error: a.error })),
    messages: rows, next_cursor: null, generated_at: opts.generatedAt ?? iso(0),
    ...(opts.sync ? { sync: opts.sync } : {})
  };
}

function syncReport(status: 'ok' | 'partial' | 'error', accounts: Array<[string, string | null]>, accountId: string | null = null, target = 'inbox') {
  return {
    target, account_id: accountId, status, started_at: iso(1000), finished_at: iso(2000),
    accounts: accounts.map(([id, error]) => ({ account_id: id, account_username: `${id}@example.com`, ok: error === null, joined: false, error }))
  };
}

function deferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

/** The sync status line (a polite live region). Spinners are also
 *  role=status, so it is addressed by id. */
const syncStatus = () => document.querySelector('#sync-status') as HTMLElement;
const listText = () => document.querySelector('#msg-list-pane')?.textContent ?? '';

beforeEach(() => {
  liveMock.handlers.new_mail.length = 0;
  liveMock.handlers.lagged.length = 0;
  vi.stubGlobal('EventSource', class {});
  Element.prototype.scrollIntoView = vi.fn();
  pageState.params = { box: 'unified' };
  pageState.url = new URL('http://localhost/v2/mail/unified') as typeof pageState.url;
  apiMock.listAccounts.mockResolvedValue({ accounts: [ACCT] });
  apiMock.cockpit.mockResolvedValue({ auth: { items: [] }, actions: { failed: [] } });
  apiMock.stats.mockResolvedValue({ accounts: 1, snoozed: 0, drafts: 0 });
  apiMock.folders.mockResolvedValue({ folders: [] });
  readerApiMock.fetchThread.mockResolvedValue(null);
});
afterEach(() => {
  vi.clearAllMocks();
  vi.unstubAllGlobals();
});

describe('open: cached first paint, then one scoped sync', () => {
  it('cold open paints the cache immediately and syncs without waiting for IMAP to paint', async () => {
    apiMock.unifiedInbox.mockResolvedValue(
      viewOf([], { accounts: [acct('acct-a', null), acct('acct-b', null)] })
    );
    const slow = deferred<unknown>();
    apiMock.refreshUnifiedInbox.mockReturnValue(slow.promise);

    render(MailLayout, { children: emptyChildren });

    await waitFor(() => expect(apiMock.refreshUnifiedInbox).toHaveBeenCalledTimes(1));
    expect(apiMock.refreshUnifiedInbox).toHaveBeenCalledWith(50);
    // Painted and honest while the provider is still working: not "empty".
    await waitFor(() => expect(syncStatus().textContent).toBe('Syncing…'));
    expect(listText()).not.toContain('Inbox is empty');
    expect(screen.getByRole('button', { name: 'Syncing…' })).toBeDisabled();

    slow.resolve(viewOf([row('acct-a', 7, 'arrived-by-sync', { unread: true })], {
      generatedAt: iso(3000), accounts: [acct('acct-a', iso(2000)), acct('acct-b', iso(2000))],
      sync: syncReport('ok', [['acct-a', null], ['acct-b', null]])
    }));
    await waitFor(() => expect(screen.getByText('arrived-by-sync')).toBeInTheDocument());
    expect(syncStatus().textContent).toMatch(/^Synced \d/);
  });

  it('cached open shows cached rows first and still syncs when the cache only looks fresh', async () => {
    apiMock.unifiedInbox.mockResolvedValue(viewOf([row('acct-a', 1, 'cached-row')]));
    apiMock.refreshUnifiedInbox.mockReturnValue(new Promise(() => {}));

    render(MailLayout, { children: emptyChildren });

    await waitFor(() => expect(screen.getByText('cached-row')).toBeInTheDocument());
    await waitFor(() => expect(apiMock.refreshUnifiedInbox).toHaveBeenCalledTimes(1));
  });

  it('already-fresh open (every account synced moments ago, e.g. by another tab) schedules no sync', async () => {
    apiMock.unifiedInbox.mockResolvedValue(
      viewOf([row('acct-a', 1, 'fresh-row')], {
        generatedAt: iso(5_000),
        accounts: [acct('acct-a', iso(0)), acct('acct-b', iso(0))]
      })
    );

    render(MailLayout, { children: emptyChildren });

    await waitFor(() => expect(screen.getByText('fresh-row')).toBeInTheDocument());
    await tick();
    expect(apiMock.refreshUnifiedInbox).not.toHaveBeenCalled();
    expect(syncStatus().textContent).toMatch(/^Last synced \d/);
  });

  it('a reader deep link syncs the list once; opening other messages re-syncs nothing', async () => {
    pageState.params = { box: 'unified', account: 'acct-a', uid: '1' };
    apiMock.unifiedInbox.mockResolvedValue(viewOf([row('acct-a', 1, 'one'), row('acct-a', 2, 'two')]));
    apiMock.refreshUnifiedInbox.mockResolvedValue(
      viewOf([row('acct-a', 1, 'one'), row('acct-a', 2, 'two')], {
        generatedAt: iso(3000), sync: syncReport('ok', [['acct-a', null], ['acct-b', null]])
      })
    );

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(apiMock.refreshUnifiedInbox).toHaveBeenCalledTimes(1));

    pageState.params = { box: 'unified', account: 'acct-a', uid: '2' };
    await tick();
    pageState.params = { box: 'unified' };
    await tick();
    await tick();
    expect(apiMock.unifiedInbox).toHaveBeenCalledTimes(1);
    expect(apiMock.refreshUnifiedInbox).toHaveBeenCalledTimes(1);
  });

  it('switching mailboxes syncs the new scope, and a late Inbox sync never paints over Sent', async () => {
    apiMock.unifiedInbox.mockResolvedValue(viewOf([row('acct-a', 1, 'inbox-cached')]));
    const inboxSync = deferred<unknown>();
    apiMock.refreshUnifiedInbox.mockReturnValue(inboxSync.promise);
    apiMock.sentInbox.mockResolvedValue(
      viewOf([row('acct-a', 9, 'sent-cached', { folder: 'Sent' })], { scope: 'sent' })
    );
    apiMock.refreshSentInbox.mockReturnValue(new Promise(() => {}));

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(apiMock.refreshUnifiedInbox).toHaveBeenCalledTimes(1));

    pageState.params = { box: 'sent' };
    pageState.url = new URL('http://localhost/v2/mail/sent') as typeof pageState.url;
    await waitFor(() => expect(screen.getByText('sent-cached')).toBeInTheDocument());
    await waitFor(() => expect(apiMock.refreshSentInbox).toHaveBeenCalledTimes(1));
    expect(apiMock.refreshSentInbox).toHaveBeenCalledWith(50);

    inboxSync.resolve(viewOf([row('acct-a', 5, 'late-inbox-row')], {
      generatedAt: iso(9000), sync: syncReport('ok', [['acct-a', null], ['acct-b', null]])
    }));
    await tick();
    await tick();
    expect(screen.getByText('sent-cached')).toBeInTheDocument();
    expect(screen.queryByText('late-inbox-row')).not.toBeInTheDocument();
  });

  it('route back to a mailbox synced moments ago reuses that sync', async () => {
    apiMock.unifiedInbox
      .mockResolvedValueOnce(viewOf([row('acct-a', 1, 'first-open')]))
      .mockResolvedValueOnce(
        viewOf([row('acct-a', 1, 'first-open')], {
          generatedAt: iso(4000), accounts: [acct('acct-a', iso(2000)), acct('acct-b', iso(2000))]
        })
      );
    apiMock.refreshUnifiedInbox.mockResolvedValue(
      viewOf([row('acct-a', 1, 'first-open')], {
        generatedAt: iso(3000), accounts: [acct('acct-a', iso(2000)), acct('acct-b', iso(2000))],
        sync: syncReport('ok', [['acct-a', null], ['acct-b', null]])
      })
    );
    apiMock.sentInbox.mockResolvedValue(viewOf([], { scope: 'sent', accounts: [] }));

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(apiMock.refreshUnifiedInbox).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(syncStatus().textContent).toMatch(/^Synced/));

    pageState.params = { box: 'sent' };
    await waitFor(() => expect(apiMock.sentInbox).toHaveBeenCalled());
    pageState.params = { box: 'unified' };
    await waitFor(() => expect(apiMock.unifiedInbox).toHaveBeenCalledTimes(2));
    await tick();
    expect(apiMock.refreshUnifiedInbox).toHaveBeenCalledTimes(1);
  });
});

describe('Sync now: honest progress, success, failure and retry', () => {
  it('coalesces repeated clicks while a run is in progress', async () => {
    apiMock.unifiedInbox.mockResolvedValue(
      viewOf([row('acct-a', 1, 'row')], { generatedAt: iso(1000), accounts: [acct('acct-a', iso(0))] })
    );
    const run = deferred<unknown>();
    apiMock.refreshUnifiedInbox.mockReturnValue(run.promise);

    render(MailLayout, { children: emptyChildren });
    const button = await screen.findByRole('button', { name: 'Sync now' });
    expect(apiMock.refreshUnifiedInbox).not.toHaveBeenCalled();

    await fireEvent.click(button);
    await fireEvent.click(button);
    expect(apiMock.refreshUnifiedInbox).toHaveBeenCalledTimes(1);
    const busy = screen.getByRole('button', { name: 'Syncing…' });
    expect(busy).toBeDisabled();
    expect(busy.getAttribute('aria-busy')).toBe('true');
    expect(syncStatus().getAttribute('aria-live')).toBe('polite');

    run.resolve(viewOf([row('acct-a', 1, 'row')], {
      generatedAt: iso(3000), accounts: [acct('acct-a', iso(2000))],
      sync: syncReport('ok', [['acct-a', null]])
    }));
    await waitFor(() => expect(syncStatus().textContent).toMatch(/^Synced \d/));
    expect(screen.getByRole('button', { name: 'Sync now' })).toBeEnabled();
  });

  it('a failed sync request keeps the cached rows, says so, and offers Retry', async () => {
    apiMock.unifiedInbox.mockResolvedValue(viewOf([row('acct-a', 1, 'kept-row')]));
    const { EnvelopeApiError } = await import('$lib/api');
    apiMock.refreshUnifiedInbox
      .mockRejectedValueOnce(new EnvelopeApiError(502, 'http_502', 'request failed (502).', null))
      .mockResolvedValueOnce(
        viewOf([row('acct-a', 1, 'kept-row')], {
          generatedAt: iso(3000), sync: syncReport('ok', [['acct-a', null], ['acct-b', null]])
        })
      );

    render(MailLayout, { children: emptyChildren });
    await waitFor(() =>
      expect(syncStatus().textContent).toBe(
        'Sync failed: request failed (502). Showing cached mail.'
      )
    );
    expect(screen.getByText('kept-row')).toBeInTheDocument();

    await fireEvent.click(screen.getByRole('button', { name: 'Retry sync' }));
    await waitFor(() => expect(syncStatus().textContent).toMatch(/^Synced/));
    expect(apiMock.refreshUnifiedInbox).toHaveBeenCalledTimes(2);
  });

  it('partial failure keeps the failed account rows, names it, and retries only that account', async () => {
    apiMock.unifiedInbox.mockResolvedValue(
      viewOf([row('acct-a', 1, 'a-row'), row('acct-b', 2, 'b-row')])
    );
    apiMock.refreshUnifiedInbox
      .mockResolvedValueOnce(
        viewOf([row('acct-a', 1, 'a-row'), row('acct-b', 2, 'b-row')], {
          generatedAt: iso(3000),
          accounts: [acct('acct-a', iso(2000)), acct('acct-b', iso(-10 * 60_000), 'IMAP: auth failed')],
          sync: syncReport('partial', [['acct-a', null], ['acct-b', 'IMAP: auth failed']])
        })
      )
      .mockResolvedValueOnce(
        viewOf([row('acct-a', 1, 'a-row'), row('acct-b', 2, 'b-row')], {
          generatedAt: iso(6000),
          accounts: [acct('acct-a', iso(2000)), acct('acct-b', iso(5000))],
          sync: syncReport('ok', [['acct-b', null]], 'acct-b')
        })
      );

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(syncStatus().textContent).toMatch(/^Partial sync — 1 account not synced/));
    // Cached rows for the failed account are still there.
    expect(screen.getByText('b-row')).toBeInTheDocument();
    const failures = screen.getByRole('list', { name: 'Accounts that did not sync' });
    expect(failures.textContent).toContain('acct-b@example.com');
    expect(failures.textContent).toContain('IMAP: auth failed');

    await fireEvent.click(screen.getByRole('button', { name: 'Retry sync for acct-b@example.com' }));
    await waitFor(() => expect(syncStatus().textContent).toMatch(/^Synced/));
    expect(apiMock.refreshUnifiedInbox).toHaveBeenLastCalledWith(50, { accountId: 'acct-b' });
    expect(screen.queryByRole('list', { name: 'Accounts that did not sync' })).not.toBeInTheDocument();
  });
});

describe('SSE and view ordering', () => {
  it('"Live" is the stream state only — it is never shown as Synced without a provider sync', async () => {
    apiMock.unifiedInbox.mockResolvedValue(
      viewOf([], { accounts: [acct('acct-a', null)] })
    );
    apiMock.refreshUnifiedInbox.mockReturnValue(new Promise(() => {}));

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(document.querySelector('#live-indicator')?.textContent).toContain('Live'));
    expect(listText()).not.toMatch(/Synced/);
  });

  it('new_mail and reconnect (lagged) reload the cached view but never start a provider sync', async () => {
    apiMock.unifiedInbox.mockResolvedValue(
      viewOf([row('acct-a', 1, 'row')], { generatedAt: iso(1000), accounts: [acct('acct-a', iso(0))] })
    );

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(apiMock.unifiedInbox).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(liveMock.handlers.new_mail.length).toBeGreaterThan(0));

    liveMock.handlers.new_mail.forEach((h) => h());
    liveMock.handlers.lagged.forEach((h) => h());
    await waitFor(() => expect(apiMock.unifiedInbox).toHaveBeenCalledTimes(3));
    expect(apiMock.refreshUnifiedInbox).not.toHaveBeenCalled();
  });

  it('an older cached response that lands after a sync never overwrites the newer view', async () => {
    apiMock.unifiedInbox.mockResolvedValueOnce(viewOf([row('acct-a', 1, 'cached-row')]));
    const sync = deferred<unknown>();
    apiMock.refreshUnifiedInbox.mockReturnValue(sync.promise);
    const slowGet = deferred<unknown>();
    apiMock.unifiedInbox.mockReturnValueOnce(slowGet.promise);

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(apiMock.refreshUnifiedInbox).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(liveMock.handlers.new_mail.length).toBeGreaterThan(0));
    // A cached reload starts (SSE) and is read before the sync finishes…
    liveMock.handlers.new_mail.forEach((h) => h());

    sync.resolve(viewOf([row('acct-a', 2, 'synced-row')], {
      generatedAt: iso(5000), sync: syncReport('ok', [['acct-a', null], ['acct-b', null]])
    }));
    await waitFor(() => expect(screen.getByText('synced-row')).toBeInTheDocument());

    // …but its response arrives last, stamped earlier.
    slowGet.resolve(viewOf([row('acct-a', 1, 'older-row')], { generatedAt: iso(2000) }));
    await tick();
    await tick();
    expect(screen.getByText('synced-row')).toBeInTheDocument();
    expect(screen.queryByText('older-row')).not.toBeInTheDocument();
  });
});

describe('sync keeps user context and drops stale handles', () => {
  it('keeps the selection of rows that survive and drops rows that were deleted', async () => {
    apiMock.unifiedInbox.mockResolvedValue(
      viewOf([row('acct-a', 1, 'survivor'), row('acct-a', 2, 'deleted-on-server')], {
        generatedAt: iso(1000), accounts: [acct('acct-a', iso(0))]
      })
    );
    const run = deferred<unknown>();
    apiMock.refreshUnifiedInbox.mockReturnValue(run.promise);

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('deleted-on-server')).toBeInTheDocument());
    const boxes = screen.getAllByRole('checkbox', { name: 'Select message' });
    await fireEvent.click(boxes[0]);
    await fireEvent.click(boxes[1]);
    expect(boxes.filter((b) => b.getAttribute('aria-checked') === 'true')).toHaveLength(2);

    await fireEvent.click(screen.getByRole('button', { name: 'Sync now' }));
    run.resolve(viewOf([row('acct-a', 1, 'survivor'), row('acct-a', 3, 'new-arrival', { unread: true })], {
      generatedAt: iso(3000), accounts: [acct('acct-a', iso(2000))],
      sync: syncReport('ok', [['acct-a', null]])
    }));
    await waitFor(() => expect(screen.getByText('new-arrival')).toBeInTheDocument());

    const after = screen.getAllByRole('checkbox', { name: 'Select message' });
    const checked = after.filter((b) => b.getAttribute('aria-checked') === 'true');
    expect(checked).toHaveLength(1);
    expect(checked[0].closest('li')?.textContent).toContain('survivor');
  });

  it('drops a selection whose account UIDVALIDITY reset even though the UID came back', async () => {
    apiMock.unifiedInbox.mockResolvedValue(
      viewOf([row('acct-a', 1, 'before-reset')], { generatedAt: iso(1000), accounts: [acct('acct-a', iso(0))] })
    );
    const run = deferred<unknown>();
    apiMock.refreshUnifiedInbox.mockReturnValue(run.promise);

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('before-reset')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('checkbox', { name: 'Select message' }));

    await fireEvent.click(screen.getByRole('button', { name: 'Sync now' }));
    run.resolve(viewOf([row('acct-a', 1, 'after-reset', { uidvalidity: 2 })], {
      generatedAt: iso(3000), accounts: [acct('acct-a', iso(2000))],
      sync: syncReport('ok', [['acct-a', null]])
    }));
    await waitFor(() => expect(screen.getByText('after-reset')).toBeInTheDocument());
    expect(screen.getByRole('checkbox', { name: 'Select message' }).getAttribute('aria-checked')).toBe('false');
    // Exactly one row: no duplicate from the old UIDVALIDITY.
    expect(document.querySelectorAll('#unified-msg-list > li')).toHaveLength(1);
  });
});

describe('right-click menu (#172)', () => {
  it('closes when the route changes', async () => {
    const { getContextMenu } = await import('$lib/context-menu.svelte');
    apiMock.unifiedInbox.mockResolvedValue(viewOf([row('acct-a', 1, 'one')]));
    apiMock.refreshUnifiedInbox.mockResolvedValue(viewOf([row('acct-a', 1, 'one')]));
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(listText()).toContain('one'));
    const link = document.querySelector('[data-msg-key="acct-a:1"] a.msg-body')!;
    link.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: 50, clientY: 50 }));
    await waitFor(() => expect(document.querySelector('.msg-context-menu')).toBeTruthy());
    pageState.params = { box: 'unified', account: 'acct-a', uid: '1' };
    pageState.url = new URL('http://localhost/v2/mail/unified/acct-a/1') as typeof pageState.url;
    await waitFor(() => expect(document.querySelector('.msg-context-menu')).toBeNull());
    expect(getContextMenu().current).toBeNull();
  });
});
