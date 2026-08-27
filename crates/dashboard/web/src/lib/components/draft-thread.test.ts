// Tests for DraftThread — the conversation shown above the draft composer.
//
// Coverage:
//   • resolves the thread despite the bracket skew between how drafts store
//     In-Reply-To (`<id>`) and how the thread index stores Message-IDs (`id`)
//   • orders oldest → newest and auto-expands the message being replied to
//   • collapsed rows expand on click and fetch their body lazily
//   • a thread that cannot be loaded degrades quietly, leaving the draft usable
//   • renders nothing at all for a draft that is not a reply

import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const { readerApiMock } = vi.hoisted(() => ({
  readerApiMock: {
    fetchThread: vi.fn(),
    fetchMessageDetail: vi.fn()
  }
}));

vi.mock('$lib/reader-api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/reader-api')>();
  return {
    ...actual,
    fetchThread: readerApiMock.fetchThread,
    fetchMessageDetail: readerApiMock.fetchMessageDetail
  };
});

import DraftThread from './DraftThread.svelte';
import type { ThreadMessage } from '$lib/reader-api';

const ACCOUNT = '31f5fddf-04f9-4978-aea5-29aa9af12bb0';
/** The parent as a draft stores it — RFC 5322 bracketed. */
const PARENT_BRACKETED = '<9AFEA64F@RO.IB.WIPO.INT>';
/** The same id as the thread index stores it — bare. */
const PARENT_BARE = '9AFEA64F@RO.IB.WIPO.INT';

const threadMsg = (over: Partial<ThreadMessage> & { id: number; uid: number }): ThreadMessage => ({
  thread_id: 'thread-1',
  message_id: null,
  in_reply_to: null,
  references: null,
  folder: 'INBOX',
  from_address: null,
  to_addresses: 'tyler@martin.fm',
  date: null,
  subject: 'RE: fee confirmation',
  is_outbound: false,
  snippet: null,
  ...over
});

const MESSAGES: ThreadMessage[] = [
  threadMsg({
    id: 1,
    uid: 2292,
    message_id: 'D0B3851B@RO.IB.WIPO.INT',
    from_address: 'ro.ib@wipo.int',
    date: '2026-08-03T09:57:05Z',
    snippet: 'Dear Mr. Martin, the request has been received'
  }),
  threadMsg({
    id: 2,
    uid: 794,
    folder: 'INBOX/sent',
    message_id: '91359a5f@martin.fm',
    from_address: 'tyler@martin.fm',
    is_outbound: true,
    date: '2026-08-03T13:54:09Z',
    snippet: 'Thank you for confirming the issuance date'
  }),
  threadMsg({
    id: 3,
    uid: 2301,
    message_id: PARENT_BARE,
    from_address: 'ro.ib@wipo.int',
    date: '2026-08-04T12:48:01Z',
    snippet: 'The payment-method selector is now available'
  })
];

beforeEach(() => {
  vi.clearAllMocks();
  readerApiMock.fetchThread.mockResolvedValue({ thread_id: 'thread-1', messages: MESSAGES });
  readerApiMock.fetchMessageDetail.mockResolvedValue({
    message: {
      uid: 2301,
      message_id: PARENT_BARE,
      from_addr: 'ro.ib@wipo.int',
      to_addr: 'tyler@martin.fm',
      subject: 'RE: fee confirmation',
      date: '2026-08-04T12:48:01Z',
      flags: [],
      text_body: 'You may now select a payment method.',
      html_body: null
    }
  });
});

describe('DraftThread', () => {
  it('looks the thread up by the bare Message-ID even though the draft stores brackets', async () => {
    render(DraftThread, { accountId: ACCOUNT, inReplyTo: PARENT_BRACKETED });

    await waitFor(() => expect(readerApiMock.fetchThread).toHaveBeenCalled());
    expect(readerApiMock.fetchThread).toHaveBeenCalledWith(ACCOUNT, PARENT_BARE);
  });

  it('renders the conversation oldest first, with "me" for the outbound reply', async () => {
    render(DraftThread, { accountId: ACCOUNT, inReplyTo: PARENT_BRACKETED });

    await waitFor(() => expect(screen.getByText('3 messages')).toBeInTheDocument());

    const senders = [...document.querySelectorAll('.row-sender')].map((n) => n.textContent?.trim());
    expect(senders).toEqual(['ro.ib@wipo.int', 'me', 'ro.ib@wipo.int']);
  });

  it('auto-expands the message the draft is replying to and loads its body', async () => {
    render(DraftThread, { accountId: ACCOUNT, inReplyTo: PARENT_BRACKETED });

    await waitFor(() =>
      expect(screen.getByText('You may now select a payment method.')).toBeInTheDocument()
    );

    // Only the parent opens; the two earlier messages stay collapsed.
    const open = document.querySelectorAll('.thread-row[aria-expanded="true"]');
    expect(open.length).toBe(1);
    expect(readerApiMock.fetchMessageDetail).toHaveBeenCalledWith(ACCOUNT, 2301, 'INBOX');
  });

  it('expands a collapsed message on click, fetching it from its own folder', async () => {
    render(DraftThread, { accountId: ACCOUNT, inReplyTo: PARENT_BRACKETED });
    await waitFor(() => expect(screen.getByText('3 messages')).toBeInTheDocument());

    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: {
        uid: 794,
        message_id: '91359a5f@martin.fm',
        from_addr: 'tyler@martin.fm',
        to_addr: 'ro.ib@wipo.int',
        subject: 'RE: fee confirmation',
        date: '2026-08-03T13:54:09Z',
        flags: [],
        text_body: 'Thank you for confirming the issuance date.',
        html_body: null
      }
    });

    const rows = document.querySelectorAll('.thread-row');
    await fireEvent.click(rows[1]);

    await waitFor(() =>
      expect(screen.getByText('Thank you for confirming the issuance date.')).toBeInTheDocument()
    );
    // The sent copy lives in INBOX/sent — fetching it from INBOX would 404.
    expect(readerApiMock.fetchMessageDetail).toHaveBeenCalledWith(ACCOUNT, 794, 'INBOX/sent');
  });

  it('degrades to a notice when the thread cannot be loaded', async () => {
    readerApiMock.fetchThread.mockRejectedValueOnce(new Error('boom'));
    render(DraftThread, { accountId: ACCOUNT, inReplyTo: PARENT_BRACKETED });

    await waitFor(() =>
      expect(screen.getByText(/could not be loaded/i)).toBeInTheDocument()
    );
  });

  it('renders nothing when the draft is not a reply', async () => {
    render(DraftThread, { accountId: ACCOUNT, inReplyTo: null });

    await waitFor(() => expect(readerApiMock.fetchThread).not.toHaveBeenCalled());
    expect(document.getElementById('draft-thread')).toBeNull();
  });
});
