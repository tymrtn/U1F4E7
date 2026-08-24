// Unified inbox paging + honest banners (sweep blocker #4, UI half).
//
// Contracts: a full page's next_cursor renders a "Load more" that fetches the
// next page with the cursor params and APPENDS (deduped, order kept); the
// unreachable-accounts banner names the accounts instead of only counting them.
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { page as pageState } from '$app/state';

const { apiMock, readerApiMock } = vi.hoisted(() => ({
  apiMock: {
    listAccounts: vi.fn(),
    cockpit: vi.fn(),
    stats: vi.fn(),
    unifiedInbox: vi.fn(),
    refreshUnifiedInbox: vi.fn(),
    searchMessages: vi.fn(),
    message: vi.fn()
  },
  readerApiMock: { fetchMessageDetail: vi.fn(), fetchThread: vi.fn(), postFlags: vi.fn() }
}));
vi.mock('$lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/api')>();
  return { ...actual, api: apiMock };
});
vi.mock('$lib/reader-api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/reader-api')>();
  return { ...actual, ...readerApiMock };
});

import MailLayout from '../../routes/mail/[box]/+layout.svelte';
import { createRawSnippet } from 'svelte';
const emptyChildren = createRawSnippet(() => ({ render: () => '<span></span>' }));

const ACCT = { id: 'acct-a', name: 'A', username: 'a@example.com', domain: 'example.com', smtp_host: 's', smtp_port: 1, imap_host: 'i', imap_port: 1, display_name: 'A' };

const ROW = (uid: number, subject: string) => ({
  uid, message_id: `<${uid}@x>`, from_addr: 'p@example.com', to_addr: 'me@example.com',
  subject, date: '2026-08-20T10:00:00Z', flags: ['\\Seen'], size: 10, unread: false,
  account_id: 'acct-a', account_username: 'a@example.com', account_display_name: 'A',
  folder: 'INBOX', uidvalidity: 1, snippet: null, thread_id: null, indexed_at: '2026-08-24T10:00:00Z',
  index_freshness: 'fresh', date_epoch: 1_750_000_000 - uid
});

const PAGE = (rows: ReturnType<typeof ROW>[], next: boolean, errors: unknown[] = []) => ({
  scope: 'unified_inbox', status: 'ok', folder: 'INBOX', limit: 2, unread_count: 0,
  freshness: 'fresh', accounts: [], errors,
  messages: rows,
  next_cursor: next ? { date_epoch: rows[rows.length - 1].date_epoch, uid: rows[rows.length - 1].uid, account_id: 'acct-a' } : null
});

beforeEach(() => {
  pageState.params = { box: 'unified' };
  pageState.url = new URL('http://localhost/v2/mail/unified') as typeof pageState.url;
  apiMock.listAccounts.mockResolvedValue({ accounts: [ACCT] });
  apiMock.cockpit.mockResolvedValue({ auth: { items: [] }, actions: { failed: [] } });
  apiMock.stats.mockResolvedValue({ accounts: 1, snoozed: 0, drafts: 0 });
  readerApiMock.fetchThread.mockResolvedValue(null);
});
afterEach(() => vi.clearAllMocks());

describe('unified pagination', () => {
  it('Load more fetches the next page with the cursor and appends deduped', async () => {
    apiMock.unifiedInbox
      .mockResolvedValueOnce(PAGE([ROW(10, 'first'), ROW(9, 'second')], true))
      .mockResolvedValueOnce(PAGE([ROW(9, 'second'), ROW(8, 'third')], false));
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('first')).toBeInTheDocument());

    const more = screen.getByRole('button', { name: 'Load more' });
    await fireEvent.click(more);
    await waitFor(() => expect(screen.getByText('third')).toBeInTheDocument());

    // Cursor params reached the API on the second call.
    const second = apiMock.unifiedInbox.mock.calls[1];
    expect(second[1]).toEqual({ date_epoch: 1_750_000_000 - 9, uid: 9, account_id: 'acct-a' });

    // Appended, deduped, order kept.
    expect(screen.getAllByText('second')).toHaveLength(1);
    const subjects = Array.from(document.querySelectorAll('#unified-msg-list .msg-subject, #unified-msg-list a')).map((n) => n.textContent ?? '');
    const joined = subjects.join(' ');
    expect(joined.indexOf('first')).toBeLessThan(joined.indexOf('third'));

    // No further page: the button is gone.
    expect(screen.queryByRole('button', { name: 'Load more' })).not.toBeInTheDocument();
  });
});

describe('unified banner names unreachable accounts', () => {
  it('lists the account usernames, never only a count', async () => {
    apiMock.unifiedInbox.mockResolvedValue(
      PAGE([ROW(10, 'only')], false, [
        { account_id: 'acct-x', account_username: 'x@example.com', account_display_name: null, folder: 'INBOX', error: 'IMAP: Connection too slow' },
        { account_id: 'acct-y', account_username: 'y@example.com', account_display_name: null, folder: 'INBOX', error: 'IMAP: auth' }
      ])
    );
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('only')).toBeInTheDocument());
    const note = screen.getByRole('status');
    expect(note.textContent).toContain('x@example.com');
    expect(note.textContent).toContain('y@example.com');
  });
});
