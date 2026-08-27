// Tests for the v2 reader pane components and reader-api helpers.
//
// Coverage:
//  BodyFrame  — sandbox attrs, CSP, allow-scripts absent, remote-image toggle
//  ReaderPane — text/html toggle, read-toggle calls flags endpoint,
//               empty/error states carry stable codes, copy affordances
//  ThreadStrip — renders, highlights current, navigates, +N overflow
//  reader-api utils — isSeen, formatBytes, attachmentDownloadUrl

import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

// ── Module mocks ──────────────────────────────────────────────────────
import { page as pageState } from '$app/state';

const { readerApiMock } = vi.hoisted(() => ({
  readerApiMock: {
    fetchMessageDetail: vi.fn(),
    fetchThread: vi.fn(),
    postFlags: vi.fn()
  }
}));

const { apiMock } = vi.hoisted(() => ({
  apiMock: { bulkClient: vi.fn() }
}));

vi.mock('$lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/api')>();
  return { ...actual, bulkClient: apiMock.bulkClient };
});

vi.mock('$lib/reader-api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/reader-api')>();
  return {
    ...actual,
    fetchMessageDetail: readerApiMock.fetchMessageDetail,
    fetchThread: readerApiMock.fetchThread,
    postFlags: readerApiMock.postFlags
  };
});

import BodyFrame from './BodyFrame.svelte';
import ThreadStrip from './ThreadStrip.svelte';
import ReaderPane from './ReaderPane.svelte';
import {
  isSeen,
  formatBytes,
  attachmentDownloadUrl,
  type ThreadMessage
} from '$lib/reader-api';
import { EnvelopeApiError } from '$lib/api';
import { __resetReadState } from '$lib/read-state.svelte';
import { getComposerStore, __resetComposerStore } from '$lib/composer.svelte';
import { getMailboxOpsStore, __resetMailboxOpsStore } from '$lib/mailbox-ops.svelte';
import { goto } from '$app/navigation';

// ── Fixtures ──────────────────────────────────────────────────────────

const BASE_MSG = {
  uid: 42,
  message_id: '<test@example.com>',
  from_addr: 'sender@example.com',
  to_addr: 'me@example.com',
  to_addrs: ['me@example.com'],
  cc_addrs: [],
  subject: 'Test subject',
  date: '2026-07-08T10:00:00Z',
  flags: [],
  text_body: 'Hello world',
  html_body: null,
  unread: true,
  attachments: []
};

beforeEach(() => {
  pageState.params = { box: 'unified', account: 'acct-a', uid: '42' };
  pageState.url = new URL('http://localhost/v2/mail/unified/acct-a/42') as typeof pageState.url;

  readerApiMock.fetchMessageDetail.mockResolvedValue({ message: BASE_MSG });
  readerApiMock.fetchThread.mockResolvedValue(null);
  readerApiMock.postFlags.mockResolvedValue({ ok: true, uid: 42, added: [], removed: [] });
  apiMock.bulkClient.mockResolvedValue({ done: 1, total: 1, failed: [] });
  __resetReadState();
  __resetMailboxOpsStore();
});

afterEach(() => {
  vi.clearAllMocks();
  sessionStorage.clear();
  __resetComposerStore();
});

// ── reader-api utils ──────────────────────────────────────────────────

describe('reader-api utils', () => {
  describe('isSeen', () => {
    it('returns true when flags contain \\Seen (case-insensitive)', () => {
      expect(isSeen(['\\Seen'])).toBe(true);
      expect(isSeen(['\\seen'])).toBe(true);
      expect(isSeen(['\\SEEN'])).toBe(true);
    });
    it('returns false when \\Seen is absent', () => {
      expect(isSeen([])).toBe(false);
      expect(isSeen(['\\Flagged'])).toBe(false);
    });
  });

  describe('formatBytes', () => {
    it('formats bytes', () => {
      expect(formatBytes(500)).toBe('500 B');
      expect(formatBytes(1536)).toBe('2 KB');
      expect(formatBytes(1048576)).toBe('1.0 MB');
    });
  });

  describe('attachmentDownloadUrl', () => {
    it('builds the correct API path with folder param', () => {
      const url = attachmentDownloadUrl('acc1', 42, 'report.pdf', 'INBOX');
      expect(url).toBe('/api/accounts/acc1/messages/42/attachments/report.pdf?folder=INBOX');
    });
    it('URL-encodes account and filename', () => {
      const url = attachmentDownloadUrl('a b', 1, 'my file.pdf', 'Sent');
      expect(url).toContain('a%20b');
      expect(url).toContain('my%20file.pdf');
      expect(url).toContain('folder=Sent');
    });
  });
});

// ── BodyFrame ─────────────────────────────────────────────────────────

describe('BodyFrame', () => {
  it('renders an iframe with sandbox=allow-same-origin (no allow-scripts)', async () => {
    render(BodyFrame, { html: '<p>Hello</p>' });
    const frame = await screen.findByTitle('Message body') as HTMLIFrameElement;
    expect(frame.tagName).toBe('IFRAME');
    const sandbox = frame.getAttribute('sandbox') ?? '';
    expect(sandbox).toContain('allow-same-origin');
    expect(sandbox).not.toContain('allow-scripts');
    expect(sandbox).not.toContain('allow-same-origin allow-scripts');
  });

  it('srcdoc contains a CSP meta tag blocking external images by default', async () => {
    render(BodyFrame, { html: '<p>Test</p>' });
    const frame = await screen.findByTitle('Message body') as HTMLIFrameElement;
    const srcdoc = frame.getAttribute('srcdoc') ?? '';
    expect(srcdoc).toContain('Content-Security-Policy');
    // Remote images (https:) should not be allowed by default.
    expect(srcdoc).not.toMatch(/img-src[^;]*https:/);
  });

  it('srcdoc permits https: images when remoteImages=true', async () => {
    render(BodyFrame, { html: '<img src="https://example.com/img.png">', remoteImages: true });
    const frame = await screen.findByTitle('Message body') as HTMLIFrameElement;
    const srcdoc = frame.getAttribute('srcdoc') ?? '';
    expect(srcdoc).toMatch(/img-src[^;]*https:/);
  });

  it('blocks remote img src and substitutes transparent placeholder when remoteImages=false', async () => {
    let blockedCount = 0;
    render(BodyFrame, {
      html: '<img src="https://tracker.example.com/px.png">',
      remoteImages: false,
      onRemoteBlocked: (n: number) => { blockedCount = n; }
    });
    const frame = await screen.findByTitle('Message body') as HTMLIFrameElement;
    const srcdoc = frame.getAttribute('srcdoc') ?? '';
    // The blocked img should have its src replaced with the transparent data URL.
    expect(srcdoc).toContain('data-remote-src');
    expect(srcdoc).toContain('data:image/svg+xml');
    // $effect fires after render; wait for it.
    await waitFor(() => expect(blockedCount).toBe(1));
  });

  it('strips script tags from srcdoc', async () => {
    render(BodyFrame, { html: '<p>Hello</p><script>alert(1)</scr' + 'ipt>' });
    const frame = await screen.findByTitle('Message body') as HTMLIFrameElement;
    const srcdoc = frame.getAttribute('srcdoc') ?? '';
    expect(srcdoc).not.toContain('<script>');
    expect(srcdoc).not.toContain('alert(1)');
  });

  it('strips inline event handlers', async () => {
    render(BodyFrame, { html: '<p onclick="evil()">text</p>' });
    const frame = await screen.findByTitle('Message body') as HTMLIFrameElement;
    const srcdoc = frame.getAttribute('srcdoc') ?? '';
    expect(srcdoc).not.toContain('onclick');
  });
});

// ── ThreadStrip ───────────────────────────────────────────────────────

describe('ThreadStrip', () => {
  // Field-for-field the server's ThreadMessage (crates/store/src/models.rs).
  // The previous fixture invented `from_addr`/`flags`/`size`, which the
  // endpoint never returns — so these tests passed while the strip threw on
  // real data.
  const threadMsg = (over: Partial<ThreadMessage> & { id: number; uid: number }): ThreadMessage => ({
    thread_id: 't1',
    message_id: null,
    in_reply_to: null,
    references: null,
    folder: 'INBOX',
    from_address: null,
    to_addresses: 'me@x',
    date: null,
    subject: null,
    is_outbound: false,
    snippet: null,
    ...over
  });

  const msgs = [
    threadMsg({ id: 1, uid: 10, message_id: 'a@x', from_address: 'alice@example.com', subject: 'First', date: '2026-07-01T10:00:00Z' }),
    threadMsg({ id: 2, uid: 11, message_id: 'b@x', from_address: 'bob@example.com', subject: 'Reply', date: '2026-07-02T10:00:00Z' }),
    threadMsg({ id: 3, uid: 12, message_id: 'c@x', from_address: 'carol@example.com', subject: 'Re: Reply', date: '2026-07-03T10:00:00Z' })
  ];

  it('renders all thread messages when count <= display limit', () => {
    render(ThreadStrip, {
      messages: msgs,
      currentUid: 11,
      folder: 'INBOX',
      box: 'unified',
      accountId: 'acct-a'
    });
    expect(screen.getByText('alice')).toBeInTheDocument();
    expect(screen.getByText('bob')).toBeInTheDocument();
    expect(screen.getByText('carol')).toBeInTheDocument();
  });

  it('highlights the current message with aria-current=page', () => {
    render(ThreadStrip, {
      messages: msgs,
      currentUid: 11,
      folder: 'INBOX',
      box: 'unified',
      accountId: 'acct-a'
    });
    // aria-current is on the <a> links, not the <li> items.
    const links = document.querySelectorAll('a.thread-msg[aria-current="page"]');
    expect(links.length).toBe(1);
    expect(links[0].textContent).toContain('bob');
  });

  it('each message links to the correct reader URL', () => {
    render(ThreadStrip, {
      messages: msgs,
      currentUid: 11,
      folder: 'INBOX',
      box: 'unified',
      accountId: 'acct-a'
    });
    const links = document.querySelectorAll('a.thread-msg');
    expect(links[0].getAttribute('href')).toBe('/mail/unified/acct-a/10');
  });

  it('shows +N more label when totalCount exceeds display limit', () => {
    const many = Array.from({ length: 8 }, (_, i) =>
      threadMsg({ id: i + 1, uid: i + 1, message_id: `${i}@x`, from_address: `user${i}@example.com`, subject: `Msg ${i}` })
    );
    render(ThreadStrip, {
      messages: many,
      currentUid: 1,
      folder: 'INBOX',
      box: 'unified',
      accountId: 'acct-a',
      totalCount: 15
    });
    expect(screen.getByText('+7 more')).toBeInTheDocument();
  });

  it('renders nothing (no strip) when there is only one message', () => {
    render(ThreadStrip, {
      messages: [msgs[0]],
      currentUid: 10,
      folder: 'INBOX',
      box: 'unified',
      accountId: 'acct-a'
    });
    expect(screen.queryByRole('listitem')).not.toBeInTheDocument();
  });

  it('shows a spinner while loading', () => {
    render(ThreadStrip, {
      messages: [],
      currentUid: 0,
      folder: 'INBOX',
      box: 'unified',
      accountId: 'acct-a',
      loading: true
    });
    // Spinner renders with its label as an accessible element.
    const spinner = document.querySelector('.env-spinner, [aria-label]');
    expect(spinner).toBeTruthy();
  });
});

// ── ReaderPane ────────────────────────────────────────────────────────

describe('ReaderPane', () => {
  it('loads and displays a plain-text message', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    expect(readerApiMock.fetchMessageDetail).toHaveBeenCalledWith('acct-a', 42, 'INBOX');
    expect(screen.getByText('Hello world')).toBeInTheDocument();
    expect(screen.getByText('sender@example.com')).toBeInTheDocument();
  });

  it('shows an error state with a stable code when load fails', async () => {
    readerApiMock.fetchMessageDetail.mockRejectedValueOnce(
      new EnvelopeApiError(502, 'imap_unavailable', 'IMAP down', null)
    );
    render(ReaderPane);
    await waitFor(() => expect(screen.getByRole('alert')).toBeInTheDocument());
    expect(screen.getByText('imap_unavailable')).toBeInTheDocument();
    // Plain error text visible.
    expect(screen.getByText(/IMAP down/i)).toBeInTheDocument();
  });

  it('shows the empty state when no message is selected', async () => {
    pageState.params = { box: 'unified', account: '', uid: '' };
    render(ReaderPane);
    // Before any load, empty state should be visible.
    await waitFor(() =>
      expect(screen.getByText('Select a message to read it.')).toBeInTheDocument()
    );
    // The empty-state hint sets read-on-open expectations in plain language.
    expect(screen.getByText('Opening a message marks it read.')).toBeInTheDocument();
  });

  it('shows text/HTML toggle when both bodies are present', async () => {
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: { ...BASE_MSG, html_body: '<p>HTML body</p>', text_body: 'Plain body' }
    });
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    expect(screen.getByRole('button', { name: /HTML/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /Plain text/i })).toBeInTheDocument();
  });

  it('auto-marks an unread message read on successful open (postFlags add \\Seen, exactly once)', async () => {
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: { ...BASE_MSG, flags: [] } // unread
    });
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());

    await waitFor(() =>
      expect(readerApiMock.postFlags).toHaveBeenCalledWith(
        'acct-a',
        42,
        'INBOX',
        ['\\Seen'],
        []
      )
    );
    expect(readerApiMock.postFlags).toHaveBeenCalledTimes(1);
  });

  it('does NOT auto-mark read when the detail load fails', async () => {
    readerApiMock.fetchMessageDetail.mockRejectedValueOnce(
      new EnvelopeApiError(502, 'imap_unavailable', 'IMAP down', null)
    );
    render(ReaderPane);
    await waitFor(() => expect(screen.getByRole('alert')).toBeInTheDocument());
    expect(readerApiMock.postFlags).not.toHaveBeenCalled();
  });

  it('does NOT re-mark an already-read message on open (idempotent)', async () => {
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: { ...BASE_MSG, flags: ['\\Seen'] } // already read
    });
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    // Give any async auto-mark a chance to (wrongly) fire.
    await new Promise((r) => setTimeout(r, 20));
    expect(readerApiMock.postFlags).not.toHaveBeenCalled();
  });

  it('calls postFlags to remove \\Seen when marking unread', async () => {
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: { ...BASE_MSG, flags: ['\\Seen'] } // already read
    });
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());

    const btn = screen.getByRole('button', { name: /mark unread/i });
    await fireEvent.click(btn);
    await waitFor(() =>
      expect(readerApiMock.postFlags).toHaveBeenCalledWith(
        'acct-a',
        42,
        'INBOX',
        [],
        ['\\Seen']
      )
    );
  });

  it('shows "Read" badge when message has \\Seen flag', async () => {
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: { ...BASE_MSG, flags: ['\\Seen'] }
    });
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Read')).toBeInTheDocument());
    expect(screen.queryByText('Unread')).not.toBeInTheDocument();
  });

  it('flips the badge to "Read" after auto-marking an unread message on open', async () => {
    render(ReaderPane); // BASE_MSG is unread (flags: [])
    await waitFor(() => expect(screen.getByText('Read')).toBeInTheDocument());
    expect(screen.queryByText('Unread')).not.toBeInTheDocument();
  });

  it('reader UI uses plain language — no protocol jargon', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    // Must NOT expose protocol names to the user.
    const bodyText = document.body.textContent ?? '';
    expect(bodyText).not.toContain('BODY.PEEK');
    expect(bodyText).not.toContain('\\Seen');
  });

  it('renders thread strip when thread has multiple messages', async () => {
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: { ...BASE_MSG, message_id: '<test@x>' }
    });
    readerApiMock.fetchThread.mockResolvedValueOnce({
      thread_id: 'thread-1',
      messages: [
        { id: 1, thread_id: 'thread-1', uid: 40, message_id: 'prev@x', in_reply_to: null, references: null, folder: 'INBOX', from_address: 'alice@x', to_addresses: 'me@x', subject: 'Prev', date: null, is_outbound: false, snippet: null },
        { id: 2, thread_id: 'thread-1', uid: 42, message_id: 'test@x', in_reply_to: null, references: null, folder: 'INBOX', from_address: 'sender@example.com', to_addresses: 'me@x', subject: 'Test subject', date: null, is_outbound: false, snippet: null }
      ]
    });

    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    // Thread strip: at least alice is visible.
    await waitFor(() => expect(screen.getByText('alice')).toBeInTheDocument());
  });

  it('empty state stable code: renders reader-empty id when no message', async () => {
    pageState.params = { box: 'unified', account: '', uid: '' };
    render(ReaderPane);
    await waitFor(() => {
      const el = document.getElementById('reader-empty');
      expect(el).toBeTruthy();
    });
  });

  it('error state stable code: renders alert role with stable code', async () => {
    readerApiMock.fetchMessageDetail.mockRejectedValueOnce(
      new EnvelopeApiError(404, 'message_not_found', 'Not found', null)
    );
    render(ReaderPane);
    await waitFor(() => screen.getByRole('alert'));
    expect(screen.getByText('message_not_found')).toBeInTheDocument();
  });
});

// ── ReaderPane reply / reply-all / forward ────────────────────────────
// A human must be able to answer mail from the reader. The composer store is
// the coordination point: the reader opens it in the right mode with the open
// message as the parent; ComposerDrawer (mounted in the mail layout) does the
// rest. Forward is a fresh message, so subject + quoted body are prefilled.

describe('ReaderPane reply/forward actions', () => {
  it('renders Reply, Reply all, and Forward once a message loads', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    expect(screen.getByRole('button', { name: 'Reply' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Reply all' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Forward' })).toBeInTheDocument();
  });

  it('Reply opens the composer in reply mode for the open message, quoting it', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Reply' }));
    const composer = getComposerStore();
    expect(composer.isOpen).toBe(true);
    expect(composer.mode).toBe('reply');
    expect(composer.context.accountId).toBe('acct-a');
    expect(composer.context.parentUid).toBe(42);
    expect(composer.context.parentFolder).toBe('INBOX');
    expect(composer.context.bodyPrefix).toContain('sender@example.com wrote:');
    expect(composer.context.bodyPrefix).toContain('> Hello world');
  });

  it('Reply all opens the composer in reply-all mode', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Reply all' }));
    const composer = getComposerStore();
    expect(composer.isOpen).toBe(true);
    expect(composer.mode).toBe('reply-all');
    expect(composer.context.parentUid).toBe(42);
  });

  it('Forward opens a fresh message with a Fwd: subject and the original quoted', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Forward' }));
    const composer = getComposerStore();
    expect(composer.isOpen).toBe(true);
    expect(composer.mode).toBe('forward');
    expect(composer.context.accountId).toBe('acct-a');
    expect(composer.context.subject).toBe('Fwd: Test subject');
    expect(composer.context.bodyPrefix).toContain('---------- Forwarded message ----------');
    expect(composer.context.bodyPrefix).toContain('From: sender@example.com');
    expect(composer.context.bodyPrefix).toContain('Hello world');
  });

  it('does not re-prefix a subject that already carries Fwd:', async () => {
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: { ...BASE_MSG, subject: 'Fwd: Test subject' }
    });
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Fwd: Test subject')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Forward' }));
    expect(getComposerStore().context.subject).toBe('Fwd: Test subject');
  });
});

// ── ReaderPane mailbox actions: archive / delete / star ───────────────
// Gmail parity: the open message can be archived, trashed (or permanently
// deleted from inside Trash, behind a confirm), and starred without going back
// to the list. Moves reuse the same canonical special-use targets and the same
// per-message endpoints BulkToolbar uses; the list is told to refresh via the
// shared mailbox-ops store, and the reader returns to the list.

describe('ReaderPane mailbox actions', () => {
  it('Archive moves the open message to \\Archive, refreshes the list, and returns to it', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    const ops = getMailboxOpsStore();
    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));
    await waitFor(() => expect(apiMock.bulkClient).toHaveBeenCalled());
    expect(apiMock.bulkClient).toHaveBeenCalledWith(
      { type: 'move', to_folder: '\\Archive', folder: 'INBOX' },
      [{ accountId: 'acct-a', uid: 42, folder: 'INBOX' }]
    );
    await waitFor(() => expect(ops.version).toBe(1));
    expect(goto).toHaveBeenCalledWith('/v2/mail/unified');
  });

  it('Delete outside Trash moves the message to \\Trash without a confirm', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Delete' }));
    await waitFor(() => expect(apiMock.bulkClient).toHaveBeenCalled());
    expect(apiMock.bulkClient).toHaveBeenCalledWith(
      { type: 'move', to_folder: '\\Trash', folder: 'INBOX' },
      [{ accountId: 'acct-a', uid: 42, folder: 'INBOX' }]
    );
    expect(goto).toHaveBeenCalledWith('/v2/mail/unified');
  });

  it('Delete inside Trash asks for confirmation, then permanently deletes', async () => {
    pageState.url = new URL('http://localhost/v2/mail/unified/acct-a/42?folder=Trash') as typeof pageState.url;
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Delete forever' }));
    // Nothing destructive yet: a confirm is showing.
    expect(apiMock.bulkClient).not.toHaveBeenCalled();
    const confirmBtn = await screen.findByRole('button', { name: 'Permanently delete' });
    await fireEvent.click(confirmBtn);
    await waitFor(() => expect(apiMock.bulkClient).toHaveBeenCalled());
    expect(apiMock.bulkClient).toHaveBeenCalledWith(
      { type: 'delete', folder: 'Trash' },
      [{ accountId: 'acct-a', uid: 42, folder: 'Trash' }]
    );
  });

  it('a failed move stays on the message and reports the error', async () => {
    apiMock.bulkClient.mockResolvedValueOnce({
      done: 1,
      total: 1,
      failed: [{ item: { accountId: 'acct-a', uid: 42, folder: 'INBOX' }, error: 'IMAP down' }]
    });
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));
    await waitFor(() => expect(screen.getByText(/IMAP down/)).toBeInTheDocument());
    expect(goto).not.toHaveBeenCalled();
    expect(getMailboxOpsStore().version).toBe(0);
  });

  it('Star sets \\Flagged on the open message and flips to Unstar', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Star' }));
    await waitFor(() =>
      expect(readerApiMock.postFlags).toHaveBeenCalledWith('acct-a', 42, 'INBOX', ['\\Flagged'], [])
    );
    await waitFor(() => expect(screen.getByRole('button', { name: 'Unstar' })).toBeInTheDocument());
  });

  it('Unstar removes \\Flagged when the message is already starred', async () => {
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: { ...BASE_MSG, flags: ['\\Flagged'] }
    });
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Unstar' }));
    await waitFor(() =>
      expect(readerApiMock.postFlags).toHaveBeenCalledWith('acct-a', 42, 'INBOX', [], ['\\Flagged'])
    );
  });
});
