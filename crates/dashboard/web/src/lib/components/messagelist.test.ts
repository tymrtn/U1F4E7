// Tests for v2 message list: SelectionStore, BulkToolbar, MessageRow, SearchBar,
// star optimistic revert, and bulk client partial-failure + concurrency cap.

import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { SelectionStore } from '$lib/selection.svelte';
import { bulkClient, type BulkItem, EnvelopeApiError, resetCsrf } from '$lib/api';
import { readState, __resetReadState } from '$lib/read-state.svelte';

// ── Module mocks ──────────────────────────────────────────────────────
import { page as pageState } from '$app/state';

const { apiMock } = vi.hoisted(() => ({
  apiMock: {
    listAccounts: vi.fn(),
    cockpit: vi.fn(),
    stats: vi.fn(),
    unifiedInbox: vi.fn(),
    message: vi.fn(),
    verifyAccount: vi.fn(),
    deleteAccount: vi.fn(),
    snoozed: vi.fn(),
    drafts: vi.fn(),
    folders: vi.fn(),
    messageFlags: vi.fn(),
    messageMove: vi.fn(),
    messageDelete: vi.fn(),
    snoozeMessage: vi.fn(),
    searchMessages: vi.fn(),
  }
}));

const { rulesApiMock } = vi.hoisted(() => ({
  rulesApiMock: { create: vi.fn() }
}));

vi.mock('$lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/api')>();
  return { ...actual, api: apiMock };
});

vi.mock('$lib/rules-api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/rules-api')>();
  return { ...actual, rulesApi: rulesApiMock };
});

vi.mock('$app/navigation', () => ({ goto: vi.fn() }));

import MailLayout from '../../routes/mail/[box]/+layout.svelte';
import BulkToolbar from './BulkToolbar.svelte';
import { createRawSnippet } from 'svelte';

const emptyChildren = createRawSnippet(() => ({ render: () => '<span></span>' }));

/** Minimal Response stub for bulkClient's injected fetchImpl. */
function jsonResponse(body: unknown, init: { status?: number } = {}): Response {
  const status = init.status ?? 200;
  const payload = JSON.stringify(body);
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => JSON.parse(payload),
    clone() { return jsonResponse(body, init); }
  } as unknown as Response;
}

beforeEach(() => {
  resetCsrf();
  __resetReadState();
  pageState.params = { box: 'unified' };
  pageState.url = new URL('http://localhost/v2/mail/unified') as typeof pageState.url;
  apiMock.listAccounts.mockResolvedValue({ accounts: [] });
  apiMock.cockpit.mockResolvedValue({ auth: { items: [] }, actions: { failed: [] } });
  apiMock.stats.mockResolvedValue({ accounts: 0, snoozed: 0, drafts: 0 });
  apiMock.unifiedInbox.mockResolvedValue({
    scope: 'unified_inbox', status: 'empty', folder: 'INBOX', limit: 50,
    unread_count: 0, freshness: 'empty', accounts: [], errors: [], messages: []
  });
  apiMock.folders.mockResolvedValue({
    folders: [],
    snoozed_virtual: { folder: 'Snoozed', exists: 0, recent: 0, unseen: null, virtual: true }
  });
});

afterEach(() => {
  vi.clearAllMocks();
  vi.unstubAllGlobals();
});

/** A global-fetch stub that records calls and returns ok JSON (handles CSRF). */
function stubOkFetch(): ReturnType<typeof vi.fn> {
  const f = vi.fn(async (url: RequestInfo | URL) => {
    if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
    return jsonResponse({ ok: true });
  });
  vi.stubGlobal('fetch', f);
  return f;
}

// ── 1. SelectionStore — range selection ───────────────────────────────

describe('SelectionStore — range selection', () => {
  it('selects a single key on toggle', () => {
    const s = new SelectionStore();
    s.toggle('a:1');
    expect(s.isSelected('a:1')).toBe(true);
    expect(s.count).toBe(1);
  });

  it('deselects on second toggle', () => {
    const s = new SelectionStore();
    s.toggle('a:1');
    s.toggle('a:1');
    expect(s.isSelected('a:1')).toBe(false);
    expect(s.count).toBe(0);
  });

  it('selects an inclusive range on rangeSelect', () => {
    const s = new SelectionStore();
    const keys = ['a:1', 'a:2', 'a:3', 'a:4', 'a:5'];
    s.toggle('a:1');               // anchor
    s.rangeSelect('a:4', keys);    // extend to index 3
    expect(s.isSelected('a:1')).toBe(true);
    expect(s.isSelected('a:2')).toBe(true);
    expect(s.isSelected('a:3')).toBe(true);
    expect(s.isSelected('a:4')).toBe(true);
    expect(s.isSelected('a:5')).toBe(false);
  });

  it('handles reverse direction range select', () => {
    const s = new SelectionStore();
    const keys = ['a:1', 'a:2', 'a:3'];
    s.toggle('a:3');
    s.rangeSelect('a:1', keys);
    expect(['a:1', 'a:2', 'a:3'].every((k) => s.isSelected(k))).toBe(true);
  });

  it('clears all on deselectAll', () => {
    const s = new SelectionStore();
    s.toggle('a:1');
    s.toggle('a:2');
    s.deselectAll();
    expect(s.count).toBe(0);
  });

  it('keyToggle toggles the focused key', () => {
    const s = new SelectionStore();
    s.keyToggle('x:7');
    expect(s.isSelected('x:7')).toBe(true);
    s.keyToggle('x:7');
    expect(s.isSelected('x:7')).toBe(false);
  });

  it('isEmpty reflects selection state', () => {
    const s = new SelectionStore();
    expect(s.isEmpty).toBe(true);
    s.toggle('a:1');
    expect(s.isEmpty).toBe(false);
  });
});

// ── 2. BulkToolbar — hidden at 0, visible at N ────────────────────────

describe('BulkToolbar — visibility', () => {
  it('is hidden when nothing is selected', () => {
    const s = new SelectionStore();
    render(BulkToolbar, { selection: s });
    expect(screen.queryByRole('toolbar')).not.toBeInTheDocument();
  });

  it('shows with count and action buttons when rows are selected', () => {
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    s.toggle('acct-ok:102');
    render(BulkToolbar, { selection: s });
    expect(screen.getByRole('toolbar')).toBeInTheDocument();
    expect(screen.getByText('2 selected')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /archive/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /delete/i })).toBeInTheDocument();
  });
});

// ── 2b. BulkToolbar — action surface (icons, junk split, snooze, delete) ──

describe('BulkToolbar — action surface', () => {
  const idx = (over: Record<string, { from: string; folder?: string; message_id?: string; subject?: string }>) => over;

  it('exposes accessible names + visible labels for the primary actions and Snooze', () => {
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, { selection: s });
    // Primary row: Flag is the single primary flag control; its inverse (Unflag)
    // plus Mark read/unread and Move… live behind More — no orphaned text and no
    // contradictory Flag/Unflag twins in the row.
    for (const name of ['Archive', 'Snooze', 'Flag', 'Junk', 'Delete', 'More']) {
      const btn = screen.getByRole('button', { name });
      expect(btn).toBeInTheDocument();
      // Mobile compaction hides the label text but keeps an accessible name:
      // every primary button carries an explicit aria-label and a label span.
      expect(btn.getAttribute('aria-label')).toBeTruthy();
      expect(btn.querySelector('.bulk-label')).toBeTruthy();
    }
  });

  it('tucks Unflag, Mark read/unread and Move… into a single More menu', async () => {
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, { selection: s });
    // Collapsed by default — no orphaned secondary controls in the row.
    expect(screen.queryByRole('menuitem', { name: 'Unflag' })).not.toBeInTheDocument();
    await fireEvent.click(screen.getByRole('button', { name: 'More' }));
    for (const name of ['Mark read', 'Mark unread', 'Unflag', 'Move…']) {
      expect(screen.getByRole('menuitem', { name })).toBeInTheDocument();
    }
  });

  it('disables actions while a bulk op is running (no double-dispatch)', async () => {
    const fetch = stubOkFetch();
    // Never resolve the move so the running state is observable.
    fetch.mockImplementation(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      return new Promise(() => {}) as unknown as Response;
    });
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, { selection: s });
    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));
    await waitFor(() =>
      expect((screen.getByRole('button', { name: 'Delete' }) as HTMLButtonElement).disabled).toBe(true)
    );
  });

  it('Inbox Delete dispatches a reversible canonical Trash move (no confirm dialog)', async () => {
    // From an ordinary mailbox, Delete must move to the provider-aware canonical
    // Trash (\Trash sentinel), reversibly — never the permanent hard-delete, and
    // never a confirmation ceremony.
    const fetch = stubOkFetch();
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, { selection: s, folder: 'INBOX' });
    await fireEvent.click(screen.getByRole('button', { name: 'Delete' }));
    await waitFor(() => {
      const call = fetch.mock.calls.find(([u]) => String(u).includes('/messages/101/move'));
      expect(call).toBeTruthy();
      const body = JSON.parse(String((call![1] as RequestInit).body));
      expect(body).toMatchObject({ folder: 'INBOX', to_folder: '\\Trash' });
    });
    // No permanent-delete confirmation dialog for a reversible trash move.
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    // And the destructive hard-delete endpoint is never called from Inbox Delete.
    expect(fetch.mock.calls.some(([, init]) => (init as RequestInit)?.method === 'DELETE')).toBe(false);
  });

  it('Trash Delete confirms permanent deletion with count + account scope, then hard-deletes', async () => {
    const fetch = stubOkFetch();
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    s.toggle('acct-ok:102');
    // Source folder IS the Trash view → Delete means a permanent, confirmed
    // hard-delete.
    render(BulkToolbar, { selection: s, folder: 'Trash' });
    await fireEvent.click(screen.getByRole('button', { name: /delete permanently/i }));
    const dialog = screen.getByRole('dialog');
    expect(dialog.textContent).toMatch(/Delete 2 messages/i);
    expect(dialog.textContent).toMatch(/permanently/i);
    expect(dialog.textContent).toMatch(/1 account/i);
    // Nothing deleted until the operator confirms.
    expect(fetch.mock.calls.some(([, init]) => (init as RequestInit)?.method === 'DELETE')).toBe(false);
    await fireEvent.click(screen.getByRole('button', { name: /delete messages/i }));
    await waitFor(() =>
      expect(
        fetch.mock.calls.some(
          ([u, init]) =>
            (init as RequestInit)?.method === 'DELETE' && String(u).includes('/messages/101')
        )
      ).toBe(true)
    );
  });

  it('Trash Delete confirmation reports multi-account scope', async () => {
    const s = new SelectionStore();
    s.toggle('acct-a:1');
    s.toggle('acct-b:2');
    render(BulkToolbar, { selection: s, folder: 'Deleted Items' });
    await fireEvent.click(screen.getByRole('button', { name: /delete permanently/i }));
    expect(screen.getByRole('dialog').textContent).toMatch(/2 accounts/i);
  });

  it('Archive dispatches a canonical \\Archive move with the right account/folder/uid', async () => {
    const fetch = stubOkFetch();
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, { selection: s, folder: 'INBOX' });
    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));
    await waitFor(() => {
      const call = fetch.mock.calls.find(([u]) => String(u).includes('/messages/101/move'));
      expect(call).toBeTruthy();
      const body = JSON.parse(String((call![1] as RequestInit).body));
      // Canonical sentinel — the backend resolves it to the account's real
      // Archive/All-Mail folder; the toolbar never sends a literal "Archive".
      expect(body).toMatchObject({ folder: 'INBOX', to_folder: '\\Archive' });
      expect(String(call![0])).toBe('/api/accounts/acct-ok/messages/101/move');
    });
  });

  it('dispatches each item with its OWN source folder, not the route folder', async () => {
    // A unified selection can span mailboxes: UID 5 lives in INBOX, UID 6 in
    // Sent. Archiving both must move each from its real folder — never assume the
    // toolbar's route folder for every item.
    const fetch = stubOkFetch();
    const s = new SelectionStore();
    s.toggle('acct-ok:5');
    s.toggle('acct-ok:6');
    render(BulkToolbar, {
      selection: s,
      folder: 'INBOX',
      messageIndex: idx({
        'acct-ok:5': { from: 'a@x', folder: 'INBOX' },
        'acct-ok:6': { from: 'b@x', folder: 'Sent' }
      })
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));
    await waitFor(() => {
      const c5 = fetch.mock.calls.find(([u]) => String(u).includes('/messages/5/move'));
      const c6 = fetch.mock.calls.find(([u]) => String(u).includes('/messages/6/move'));
      expect(c5 && c6).toBeTruthy();
      expect(JSON.parse(String((c5![1] as RequestInit).body)).folder).toBe('INBOX');
      expect(JSON.parse(String((c6![1] as RequestInit).body)).folder).toBe('Sent');
    });
  });

  it('Flag dispatches an add of \\Flagged', async () => {
    const fetch = stubOkFetch();
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, { selection: s });
    await fireEvent.click(screen.getByRole('button', { name: 'Flag' }));
    await waitFor(() => {
      const call = fetch.mock.calls.find(([u]) => String(u).includes('/messages/101/flags'));
      expect(call).toBeTruthy();
      expect(JSON.parse(String((call![1] as RequestInit).body)).add).toEqual(['\\Flagged']);
    });
  });

  it('Junk → block sender defaults to the EXACT sender and needs an explicit confirm', async () => {
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, {
      selection: s,
      messageIndex: idx({ 'acct-ok:101': { from: 'Alice <alice@example.com>', folder: 'INBOX' } })
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Junk' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: /block sender/i }));

    // The exact sender scope is shown before anything is created.
    expect(screen.getByText('alice@example.com')).toBeInTheDocument();
    // No rule is created without an explicit operator action.
    expect(rulesApiMock.create).not.toHaveBeenCalled();

    stubOkFetch(); // the move goes through bulkClient
    rulesApiMock.create.mockResolvedValue({ rule: { id: 'r1' } });
    await fireEvent.click(screen.getByRole('button', { name: /block .*move to junk/i }));

    await waitFor(() => expect(rulesApiMock.create).toHaveBeenCalledTimes(1));
    const [acct, body] = rulesApiMock.create.mock.calls[0];
    expect(acct).toBe('acct-ok');
    expect(body.match_expr).toEqual({ from: 'alice@example.com' });
    expect(JSON.stringify(body.match_expr)).not.toContain('*@'); // never a domain rule
    // Canonical semantic target: the rule engine resolves \Junk to the account's
    // real Spam/Junk folder per provider at run time — not a stored literal.
    expect(body.action).toEqual({ move: '\\Junk' });
  });

  it('Junk → block sender creates ONE exact rule per distinct sender across accounts', async () => {
    const s = new SelectionStore();
    s.toggle('acct-a:1');
    s.toggle('acct-a:2'); // same sender as :1 → dedup
    s.toggle('acct-b:3');
    render(BulkToolbar, {
      selection: s,
      messageIndex: idx({
        'acct-a:1': { from: 'spam@x.com', folder: 'INBOX' },
        'acct-a:2': { from: 'spam@x.com', folder: 'INBOX' },
        'acct-b:3': { from: 'other@y.com', folder: 'INBOX' }
      })
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Junk' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: /block sender/i }));
    stubOkFetch();
    rulesApiMock.create.mockResolvedValue({ rule: { id: 'r' } });
    await fireEvent.click(screen.getByRole('button', { name: /block .*move to junk/i }));

    // spam@x.com (acct-a) + other@y.com (acct-b) = 2 rules, not 3.
    await waitFor(() => expect(rulesApiMock.create).toHaveBeenCalledTimes(2));
    const senders = rulesApiMock.create.mock.calls.map((c) => (c[1].match_expr as { from: string }).from);
    expect(new Set(senders)).toEqual(new Set(['spam@x.com', 'other@y.com']));
  });

  it('Snooze preset dispatches api.snoozeMessage with a future wall-clock return_at', async () => {
    apiMock.snoozeMessage.mockResolvedValue({ ok: true, uid: 101, return_at: 'x', snoozed_folder: 'Snoozed' });
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, {
      selection: s,
      messageIndex: idx({ 'acct-ok:101': { from: 'a@x', folder: 'INBOX', message_id: '<a@x>', subject: 'Hi' } })
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Snooze' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: /tomorrow/i }));

    await waitFor(() => expect(apiMock.snoozeMessage).toHaveBeenCalledTimes(1));
    const [acct, uid, opts] = apiMock.snoozeMessage.mock.calls[0];
    expect(acct).toBe('acct-ok');
    expect(uid).toBe(101);
    // Sent as a UTC instant so the server can store UTC wall-clock.
    expect(opts.return_at).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z$/);
    expect(new Date(opts.return_at).getTime()).toBeGreaterThan(Date.now());
    expect(opts.message_id).toBe('<a@x>');
  });

  it('partial failure keeps the selection and reports "N …, M failed"', async () => {
    let n = 0;
    const fetch = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      n += 1;
      if (n === 2) return jsonResponse({ code: 'imap_error', error: 'timeout' }, { status: 502 });
      return jsonResponse({ ok: true });
    });
    vi.stubGlobal('fetch', fetch);
    const s = new SelectionStore();
    s.toggle('acct-a:1');
    s.toggle('acct-b:2');
    render(BulkToolbar, { selection: s });
    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));

    await waitFor(() => expect(screen.getByText(/1 archived, 1 failed/i)).toBeInTheDocument());
    // Toolbar still present because selection was NOT cleared on partial failure.
    expect(screen.getByRole('toolbar')).toBeInTheDocument();
  });

  it('bulk read updates confirmed successes without forcing failed rows unread', async () => {
    const fetch = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      if (String(url).includes('/accounts/acct-b/')) {
        return jsonResponse({ code: 'imap_error', error: 'timeout' }, { status: 502 });
      }
      return jsonResponse({ ok: true });
    });
    vi.stubGlobal('fetch', fetch);

    const s = new SelectionStore();
    s.toggle('acct-a:1');
    s.toggle('acct-b:2');
    // acct-a starts unread; acct-b already starts read. A failed Mark read on
    // acct-b must preserve read, not manufacture an unread rollback.
    readState.markUnread('acct-a', 'INBOX', 1);
    readState.markRead('acct-b', 'INBOX', 2);
    render(BulkToolbar, {
      selection: s,
      messageIndex: idx({
        'acct-a:1': { from: 'a@example.com', folder: 'INBOX' },
        'acct-b:2': { from: 'b@example.com', folder: 'INBOX' }
      })
    });

    await fireEvent.click(screen.getByRole('button', { name: 'More' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: 'Mark read' }));
    await waitFor(() => expect(screen.getByText(/1 marked read, 1 failed/i)).toBeInTheDocument());

    expect(readState.isUnread('acct-a', 'INBOX', 1, true)).toBe(false);
    expect(readState.isUnread('acct-b', 'INBOX', 2, false)).toBe(false);
  });
});

// ── 3. bulkClient — partial failure aggregation + concurrency cap ──────
// bulkClient uses request() internally. We inject fetchImpl to mock HTTP.

describe('bulkClient — partial failure + concurrency', () => {
  it('returns all succeeded when all calls succeed', async () => {
    const fetchImpl = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      return jsonResponse({ ok: true });
    });
    const items: BulkItem[] = [
      { accountId: 'a', uid: 1 },
      { accountId: 'a', uid: 2 },
    ];
    const result = await bulkClient({ type: 'flags', add: ['\\Seen'] }, items, undefined, fetchImpl);
    expect(result.total).toBe(2);
    expect(result.failed).toHaveLength(0);
    expect(result.done).toBe(2);
  });

  it('aggregates partial failures without throwing', async () => {
    let callCount = 0;
    const fetchImpl = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      callCount += 1;
      if (callCount % 2 === 0) return jsonResponse({ code: 'imap_error', error: 'timeout' }, { status: 502 });
      return jsonResponse({ ok: true });
    });
    const items: BulkItem[] = Array.from({ length: 4 }, (_, i) => ({ accountId: 'a', uid: i + 1 }));
    const progressCalls: { done: number; total: number }[] = [];
    const result = await bulkClient(
      { type: 'flags', add: ['\\Seen'] }, items,
      (p) => progressCalls.push({ done: p.done, total: p.total }),
      fetchImpl
    );
    expect(result.total).toBe(4);
    expect(result.failed).toHaveLength(2);
    // Progress fires for every item
    expect(progressCalls).toHaveLength(4);
    expect(progressCalls[progressCalls.length - 1].done).toBe(4);
  });

  it('caps concurrency at 4 in-flight at once', async () => {
    let maxInFlight = 0;
    let currentInFlight = 0;
    const fetchImpl = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      currentInFlight += 1;
      maxInFlight = Math.max(maxInFlight, currentInFlight);
      await new Promise((r) => setTimeout(r, 10));
      currentInFlight -= 1;
      return jsonResponse({ ok: true });
    });
    // 8 items — if concurrency cap works, max in flight (excl. the shared CSRF call) is 4
    const items: BulkItem[] = Array.from({ length: 8 }, (_, i) => ({ accountId: 'a', uid: i + 1 }));
    await bulkClient({ type: 'flags', add: ['\\Seen'] }, items, undefined, fetchImpl);
    expect(maxInFlight).toBeLessThanOrEqual(4);
  });

  it('calls onProgress after each item', async () => {
    const fetchImpl = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      return jsonResponse({ ok: true });
    });
    const items: BulkItem[] = [{ accountId: 'a', uid: 1 }, { accountId: 'a', uid: 2 }];
    const progress: number[] = [];
    await bulkClient({ type: 'flags', add: ['\\Seen'] }, items, (p) => progress.push(p.done), fetchImpl);
    expect(progress).toEqual([1, 2]);
  });
});

// ── 4. Star optimistic revert ─────────────────────────────────────────

describe('Star optimistic revert', () => {
  it('calls messageFlags with \\Flagged add when starring', async () => {
    apiMock.unifiedInbox.mockResolvedValue({
      scope: 'unified_inbox', status: 'ok', folder: 'INBOX', limit: 50,
      unread_count: 1, freshness: 'fresh', accounts: [], errors: [],
      messages: [{
        uid: 101, message_id: '<a@x>', from_addr: 'alice@example.com',
        to_addr: 'work@example.com', subject: 'Hello', date: '2026-07-08T10:00:00Z',
        flags: [], size: 10, unread: true, account_id: 'acct-ok',
        account_username: 'work@example.com', account_display_name: 'Work Mail',
        folder: 'INBOX', uidvalidity: 1, snippet: null, thread_id: null,
        indexed_at: null, index_freshness: 'fresh'
      }]
    });
    apiMock.messageFlags.mockResolvedValue({ ok: true, uid: 101, added: ['\\Flagged'], removed: [] });

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Hello')).toBeInTheDocument());

    const starBtn = screen.getByRole('button', { name: /star message/i });
    await fireEvent.click(starBtn);

    await waitFor(() =>
      expect(apiMock.messageFlags).toHaveBeenCalledWith(
        'acct-ok', 101,
        expect.objectContaining({ add: ['\\Flagged'], remove: [] })
      )
    );
  });

  it('reverts star if messageFlags throws', async () => {
    apiMock.unifiedInbox.mockResolvedValue({
      scope: 'unified_inbox', status: 'ok', folder: 'INBOX', limit: 50,
      unread_count: 1, freshness: 'fresh', accounts: [], errors: [],
      messages: [{
        uid: 101, message_id: '<a@x>', from_addr: 'alice@example.com',
        to_addr: 'work@example.com', subject: 'Hello', date: '2026-07-08T10:00:00Z',
        flags: [], size: 10, unread: true, account_id: 'acct-ok',
        account_username: 'work@example.com', account_display_name: 'Work Mail',
        folder: 'INBOX', uidvalidity: 1, snippet: null, thread_id: null,
        indexed_at: null, index_freshness: 'fresh'
      }]
    });
    apiMock.messageFlags.mockRejectedValue(new EnvelopeApiError(502, 'imap_error', 'timeout', null));

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Hello')).toBeInTheDocument());

    const starBtn = screen.getByRole('button', { name: /star message/i });
    // Before clicking: unstarred state
    expect(starBtn.textContent?.trim()).toBe('☆');
    await fireEvent.click(starBtn);

    // After the failed API call, optimistic state reverts to unstarred
    await waitFor(() => expect(starBtn.textContent?.trim()).toBe('☆'));
  });
});

// ── 4b. Read-state bridge (list ↔ reader) ─────────────────────────────

describe('Read-state bridge', () => {
  const unifiedMsg = (uid: number, subject: string, unread: boolean) => ({
    uid, message_id: `<${uid}@x>`, from_addr: 'alice@example.com',
    to_addr: 'work@example.com', subject, date: '2026-07-08T10:00:00Z',
    flags: unread ? [] : ['\\Seen'], size: 10, unread, account_id: 'acct-ok',
    account_username: 'work@example.com', account_display_name: 'Work Mail',
    folder: 'INBOX', uidvalidity: 1, snippet: null, thread_id: null,
    indexed_at: null, index_freshness: 'fresh'
  });

  it('renders an unread row bold (is-unread) and a read row normal', async () => {
    apiMock.unifiedInbox.mockResolvedValue({
      scope: 'unified_inbox', status: 'ok', folder: 'INBOX', limit: 50,
      unread_count: 1, freshness: 'fresh', accounts: [], errors: [],
      messages: [unifiedMsg(101, 'Unread one', true), unifiedMsg(102, 'Read one', false)]
    });
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Unread one')).toBeInTheDocument());

    const rows = screen.getAllByRole('row');
    const unreadRow = rows.find((r) => r.textContent?.includes('Unread one'))!;
    const readRow = rows.find((r) => r.textContent?.includes('Read one'))!;
    expect(unreadRow.classList.contains('is-unread')).toBe(true);
    expect(readRow.classList.contains('is-unread')).toBe(false);
  });

  it('drops unread bold on the mounted row when the reader marks it read (no refetch)', async () => {
    apiMock.unifiedInbox.mockResolvedValue({
      scope: 'unified_inbox', status: 'ok', folder: 'INBOX', limit: 50,
      unread_count: 1, freshness: 'fresh', accounts: [], errors: [],
      messages: [unifiedMsg(101, 'Bridge subject', true)]
    });
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Bridge subject')).toBeInTheDocument());

    const row = screen.getAllByRole('row').find((r) => r.textContent?.includes('Bridge subject'))!;
    expect(row.classList.contains('is-unread')).toBe(true);

    // Simulate the reader marking the message read on open. No list refetch.
    apiMock.unifiedInbox.mockClear();
    readState.markRead('acct-ok', 'INBOX', 101);

    await waitFor(() => expect(row.classList.contains('is-unread')).toBe(false));
    expect(apiMock.unifiedInbox).not.toHaveBeenCalled();
  });
});

// ── 5. Search URL persistence ─────────────────────────────────────────

describe('Search URL persistence', () => {
  it('restores box list when search query is absent from URL', async () => {
    apiMock.unifiedInbox.mockResolvedValue({
      scope: 'unified_inbox', status: 'ok', folder: 'INBOX', limit: 50,
      unread_count: 1, freshness: 'fresh', accounts: [], errors: [],
      messages: [{
        uid: 101, message_id: '<a@x>', from_addr: 'alice@example.com',
        to_addr: 'work@example.com', subject: 'Full list message', date: '2026-07-09T10:00:00Z',
        flags: [], size: 10, unread: true, account_id: 'acct-ok',
        account_username: 'work@example.com', account_display_name: 'Work Mail',
        folder: 'INBOX', uidvalidity: 1, snippet: null, thread_id: null,
        indexed_at: null, index_freshness: 'fresh'
      }]
    });
    pageState.url = new URL('http://localhost/v2/mail/unified') as typeof pageState.url;
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Full list message')).toBeInTheDocument());
  });

  it('shows search results when ?q= is present in URL', async () => {
    apiMock.unifiedInbox.mockResolvedValue({
      scope: 'unified_inbox', status: 'ok', folder: 'INBOX', limit: 50,
      unread_count: 0, freshness: 'fresh', accounts: [], errors: [], messages: []
    });
    apiMock.searchMessages.mockResolvedValue({
      messages: [{
        uid: 200, message_id: '<b@x>', from_addr: 'carol@example.com',
        to_addr: 'work@example.com', subject: 'Search result subject',
        date: '2026-07-09T10:00:00Z', flags: [], size: 10, unread: true
      }],
      query: 'carol'
    });
    apiMock.listAccounts.mockResolvedValue({
      accounts: [{
        id: 'acct-ok', name: 'Work', username: 'work@example.com',
        domain: 'example.com', smtp_host: 'smtp.example.com', smtp_port: 465,
        imap_host: 'imap.example.com', imap_port: 993
      }]
    });

    pageState.url = new URL('http://localhost/v2/mail/unified?q=carol') as typeof pageState.url;
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Search result subject')).toBeInTheDocument());
  });
});

// ── 6. Box-specific empty states (snoozed + drafts wired) ────────────

describe('Box-specific wired boxes', () => {
  it('renders empty state for snoozed when GET /snoozed returns empty list', async () => {
    pageState.params = { box: 'snoozed' };
    pageState.url = new URL('http://localhost/v2/mail/snoozed') as typeof pageState.url;
    apiMock.listAccounts.mockResolvedValue({
      accounts: [{
        id: 'acct-ok', name: 'Work', username: 'work@example.com',
        domain: 'example.com', smtp_host: 'smtp.example.com', smtp_port: 465,
        imap_host: 'imap.example.com', imap_port: 993
      }]
    });
    apiMock.snoozed.mockResolvedValue({ snoozed: [] });

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Nothing snoozed')).toBeInTheDocument());
  });

  it('renders empty state for drafts when GET /drafts returns empty list', async () => {
    pageState.params = { box: 'drafts' };
    pageState.url = new URL('http://localhost/v2/mail/drafts') as typeof pageState.url;
    apiMock.listAccounts.mockResolvedValue({
      accounts: [{
        id: 'acct-ok', name: 'Work', username: 'work@example.com',
        domain: 'example.com', smtp_host: 'smtp.example.com', smtp_port: 465,
        imap_host: 'imap.example.com', imap_port: 993
      }]
    });
    apiMock.drafts.mockResolvedValue({ drafts: [] });

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('No drafts')).toBeInTheDocument());
  });
});
