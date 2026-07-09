// Tests for v2 message list: SelectionStore, BulkToolbar, MessageRow, SearchBar,
// star optimistic revert, and bulk client partial-failure + concurrency cap.

import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { SelectionStore } from '$lib/selection.svelte';
import { bulkClient, type BulkItem, EnvelopeApiError, resetCsrf } from '$lib/api';

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
    searchMessages: vi.fn(),
  }
}));

vi.mock('$lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/api')>();
  return { ...actual, api: apiMock };
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
});

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
