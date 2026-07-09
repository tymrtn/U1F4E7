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
import { isSeen, formatBytes, attachmentDownloadUrl } from '$lib/reader-api';
import { EnvelopeApiError } from '$lib/api';

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
});

afterEach(() => {
  vi.clearAllMocks();
  sessionStorage.clear();
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
  const msgs = [
    { uid: 10, message_id: '<a@x>', from_addr: 'alice@example.com', to_addr: 'me@x', subject: 'First', date: '2026-07-01T10:00:00Z', flags: [], size: 100 },
    { uid: 11, message_id: '<b@x>', from_addr: 'bob@example.com', to_addr: 'me@x', subject: 'Reply', date: '2026-07-02T10:00:00Z', flags: ['\\Seen'], size: 100 },
    { uid: 12, message_id: '<c@x>', from_addr: 'carol@example.com', to_addr: 'me@x', subject: 'Re: Reply', date: '2026-07-03T10:00:00Z', flags: [], size: 100 }
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
    expect(links[0].getAttribute('href')).toBe('/v2/mail/unified/acct-a/10');
  });

  it('shows +N more label when totalCount exceeds display limit', () => {
    const many = Array.from({ length: 8 }, (_, i) => ({
      uid: i + 1,
      message_id: `<${i}@x>`,
      from_addr: `user${i}@example.com`,
      to_addr: 'me@x',
      subject: `Msg ${i}`,
      date: null,
      flags: [],
      size: 0
    }));
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
    // The "never marks read" note uses plain language.
    expect(screen.getByText('Reading here never marks messages as read.')).toBeInTheDocument();
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

  it('calls postFlags when mark-read button is clicked', async () => {
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: { ...BASE_MSG, flags: [] } // unread
    });
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());

    const btn = screen.getByRole('button', { name: /mark read/i });
    await fireEvent.click(btn);
    await waitFor(() =>
      expect(readerApiMock.postFlags).toHaveBeenCalledWith(
        'acct-a',
        42,
        'INBOX',
        ['\\Seen'],
        []
      )
    );
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

  it('shows "Unread" badge when message has no \\Seen flag', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Unread')).toBeInTheDocument());
  });

  it('reader note uses plain language — no protocol jargon', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    // Must contain the human note.
    expect(screen.getByText('Reading here never marks messages as read.')).toBeInTheDocument();
    // Must NOT expose protocol names to the user.
    const bodyText = document.body.textContent ?? '';
    expect(bodyText).not.toContain('BODY.PEEK');
  });

  it('renders thread strip when thread has multiple messages', async () => {
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: { ...BASE_MSG, message_id: '<test@x>' }
    });
    readerApiMock.fetchThread.mockResolvedValueOnce({
      thread_id: 'thread-1',
      messages: [
        { uid: 40, message_id: '<prev@x>', from_addr: 'alice@x', to_addr: 'me@x', subject: 'Prev', date: null, flags: [], size: 0 },
        { uid: 42, message_id: '<test@x>', from_addr: 'sender@example.com', to_addr: 'me@x', subject: 'Test subject', date: null, flags: [], size: 0 }
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
