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
  apiMock: { bulkClient: vi.fn(), messageFlags: vi.fn(), messageMove: vi.fn() }
}));

vi.mock('$lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/api')>();
  return {
    ...actual,
    bulkClient: apiMock.bulkClient,
    api: { ...actual.api, messageFlags: apiMock.messageFlags, messageMove: apiMock.messageMove }
  };
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
import { __resetReadState, readState } from '$lib/read-state.svelte';
import { __resetMessageActions } from '$lib/message-actions.svelte';
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
  apiMock.messageMove.mockResolvedValue({
    ok: true, uid: 42, from_folder: 'INBOX', moved_to: 'Archive', moved_uid: 900, moved_uidvalidity: 3
  });
  apiMock.messageFlags.mockImplementation(async (_a: string, _u: number, o: { add?: string[]; remove?: string[] }) => ({
    ok: true, uid: 42, added: o.add ?? [], removed: o.remove ?? [], confirmed: true,
    flags: [], seen: (o.add ?? []).includes('\\Seen'), flagged: (o.add ?? []).includes('\\Flagged')
  }));
  __resetReadState();
  __resetMessageActions();
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

/** jsdom lays nothing out, so hand the sizer the height a browser would
 *  report for the `#env-content` wrapper it measures. */
function stubContentHeight(doc: Document, height: number): void {
  const wrapper = doc.getElementById('env-content');
  if (!wrapper) throw new Error('stubContentHeight: fixture has no #env-content');
  wrapper.getBoundingClientRect = () => ({ height, width: 600, top: 0, left: 0, right: 600,
    bottom: height, x: 0, y: 0, toJSON: () => ({}) }) as DOMRect;
}

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

  it('keeps a 105 KB transactional email parent-scrollable well past the old 20,000px cap', async () => {
    const rows = Array.from(
      { length: 900 },
      (_, i) =>
        `<tr style="height:72px;overflow:hidden"><td style="height:72px;overflow:auto">` +
        `Booking.com reservation line ${i}: confirmation details and policies</td></tr>`
    ).join('');
    const longHtml = `<table style="overflow:hidden">${rows}</table><p>End of booking</p>`;
    expect(new TextEncoder().encode(longHtml).byteLength).toBeGreaterThan(105 * 1024);

    render(BodyFrame, { html: longHtml });
    const frame = await screen.findByTitle('Message body') as HTMLIFrameElement;
    const srcdoc = frame.getAttribute('srcdoc') ?? '';
    expect(srcdoc).toContain('Booking.com reservation line 0');
    expect(srcdoc).toContain('Booking.com reservation line 899');
    expect(srcdoc).toContain('End of booking');

    // jsdom does not lay out srcdoc, so provide the rendered measurement the
    // browser exposes after load. This is deliberately beyond the old cap.
    const renderedDocument = document.implementation.createHTMLDocument('Long message');
    renderedDocument.body.innerHTML =
      `<div id="env-content"><table>${rows}</table><p>End of booking</p></div>`;
    stubContentHeight(renderedDocument, 105_000);
    Object.defineProperty(frame, 'contentDocument', {
      configurable: true,
      value: renderedDocument
    });

    await fireEvent.load(frame);
    expect(frame.style.height).toBe('105016px');

  });

  it('never collapses the frame to measure, so the reader keeps its scroll position', async () => {
    // The regression: the sizer set the frame to 0px to read the content
    // height. The frame lives inside the reader's scroll container, so every
    // collapse shrank that container, the browser clamped scrollTop, and the
    // pane snapped to the top — on a real newsletter, ~120 times a second,
    // which made the reader unscrollable outright.
    render(BodyFrame, { html: '<p>Long message body</p>' });
    const frame = await screen.findByTitle('Message body') as HTMLIFrameElement;

    const renderedDocument = document.implementation.createHTMLDocument('Message');
    renderedDocument.body.innerHTML = '<div id="env-content"><p>Long message body</p></div>';
    stubContentHeight(renderedDocument, 4000);
    Object.defineProperty(frame, 'contentDocument', {
      configurable: true,
      value: renderedDocument
    });

    const written: string[] = [];
    const observed = new MutationObserver(() => written.push(frame.style.height));
    observed.observe(frame, { attributes: true, attributeFilter: ['style'] });

    await fireEvent.load(frame);
    await waitFor(() => expect(frame.style.height).toBe('4016px'));
    observed.disconnect();

    expect(written).not.toContain('0px');
  });

  it('writes nothing on a re-fit that measures the same content, so the observer settles', async () => {
    // Loop termination. The old sizer disconnected and re-observed inside its
    // own callback; observe() always delivers an initial callback, so it woke
    // itself forever and rewrote the height on every pass.
    render(BodyFrame, { html: '<p>Steady</p>' });
    const frame = await screen.findByTitle('Message body') as HTMLIFrameElement;

    const renderedDocument = document.implementation.createHTMLDocument('Message');
    renderedDocument.body.innerHTML = '<div id="env-content"><p>Steady</p></div>';
    stubContentHeight(renderedDocument, 2013);
    Object.defineProperty(frame, 'contentDocument', {
      configurable: true,
      value: renderedDocument
    });

    await fireEvent.load(frame);
    await waitFor(() => expect(frame.style.height).toBe('2029px'));

    // A settled frame re-measured: the height it holds is the height it wants,
    // so nothing is written and nothing wakes the observer again.
    const written: string[] = [];
    const observed = new MutationObserver(() => written.push(frame.style.height));
    observed.observe(frame, { attributes: true, attributeFilter: ['style'] });
    await fireEvent.load(frame);
    await new Promise((resolve) => setTimeout(resolve, 20));
    observed.disconnect();

    expect(written).toEqual([]);
  });

  it('cleans iframe document listeners on srcdoc replacement and teardown', async () => {
    const { rerender, unmount } = render(BodyFrame, { html: '<p>First</p>' });
    const frame = await screen.findByTitle('Message body') as HTMLIFrameElement;
    const firstDocument = frame.contentDocument;
    expect(firstDocument).not.toBeNull();
    const firstRemove = vi.spyOn(firstDocument as Document, 'removeEventListener');

    await fireEvent.load(frame);
    firstRemove.mockClear();
    await rerender({ html: '<p>Second</p>' });
    await waitFor(() =>
      expect(firstRemove).toHaveBeenCalledWith('wheel', expect.any(Function), true)
    );

    const secondDocument = frame.contentDocument;
    expect(secondDocument).not.toBeNull();
    const secondRemove = vi.spyOn(secondDocument as Document, 'removeEventListener');
    await fireEvent.load(frame);
    secondRemove.mockClear();
    unmount();
    expect(secondRemove).toHaveBeenCalledWith('wheel', expect.any(Function), true);
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
    // Tyler 2026-09-29: opening marks read, so the old "opening leaves it
    // unread" hint must not come back.
    expect(screen.queryByText(/Opening a message leaves it unread/)).not.toBeInTheDocument();
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

  it('opens an HTML message as HTML without the operator choosing', async () => {
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: { ...BASE_MSG, html_body: '<p>HTML body</p>', text_body: 'Plain body' }
    });
    render(ReaderPane);
    await waitFor(() => expect(screen.getByTitle('Message body')).toBeInTheDocument());
    expect(screen.getByRole('button', { name: /^HTML$/i }).className).toContain('is-active');
    expect(screen.queryByText('Plain body')).not.toBeInTheDocument();
  });

  it('opens a text-only message as plain text, with no toggle to choose from', async () => {
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: { ...BASE_MSG, html_body: null, text_body: 'Plain body' }
    });
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Plain body')).toBeInTheDocument());
    expect(screen.getByText('Plain text only')).toBeInTheDocument();
    expect(screen.queryByTitle('Message body')).not.toBeInTheDocument();
  });

  it('ignores a blank HTML part and renders the text that is actually there', async () => {
    // A present-but-empty part is not a body. Preferring it rendered an empty
    // reader over a perfectly good sibling part.
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: { ...BASE_MSG, html_body: '   \r\n  ', text_body: 'Plain body' }
    });
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Plain body')).toBeInTheDocument());
    expect(screen.queryByTitle('Message body')).not.toBeInTheDocument();
    expect(screen.getByText('Plain text only')).toBeInTheDocument();
  });

  it('does not carry a Plain text choice over to the next message', async () => {
    // The regression: the toggle wrote a session-wide preference, so picking
    // Plain text once made every later message open as plain text until the
    // tab closed, and the operator had to keep re-selecting HTML.
    readerApiMock.fetchMessageDetail.mockResolvedValue({
      message: { ...BASE_MSG, html_body: '<p>HTML body</p>', text_body: 'Plain body' }
    });
    render(ReaderPane);
    await waitFor(() => expect(screen.getByTitle('Message body')).toBeInTheDocument());

    await fireEvent.click(screen.getByRole('button', { name: /Plain text/i }));
    await waitFor(() => expect(screen.getByText('Plain body')).toBeInTheDocument());

    // Open the next message.
    pageState.params = { account: 'acct-a', uid: '43', box: 'unified' };
    await waitFor(() => expect(screen.getByTitle('Message body')).toBeInTheDocument());
    expect(screen.getByRole('button', { name: /^HTML$/i }).className).toContain('is-active');
  });

  it('opening an unread message marks it read (Tyler 2026-09-29)', async () => {
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: { ...BASE_MSG, flags: [] } // unread
    });
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Read')).toBeInTheDocument());
    expect(apiMock.messageFlags).toHaveBeenCalledTimes(1);
    expect(apiMock.messageFlags).toHaveBeenCalledWith(
      'acct-a',
      42,
      expect.objectContaining({
        folder: 'INBOX',
        add: ['\\Seen'],
        remove: [],
        message_id: '<test@example.com>'
      })
    );
    expect(readState.isUnread('acct-a', 'INBOX', 42, true)).toBe(false);
    // One read/unread toggle whose label follows the state: opened → read →
    // the toggle says Mark unread, and Mark read is not shown anywhere.
    expect(screen.getByRole('button', { name: 'Mark unread' })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Mark read' })).not.toBeInTheDocument();
  });

  it('marking read on open is quiet: no status note and no list reload', async () => {
    const ops = getMailboxOpsStore();
    const before = ops.version;
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Read')).toBeInTheDocument());
    await new Promise((r) => setTimeout(r, 20));
    expect(screen.queryByText('Marked read')).not.toBeInTheDocument();
    expect(ops.version).toBe(before);
  });

  it('an action clicked while the open’s mark-read is still in flight runs anyway', async () => {
    let release: (v: unknown) => void = () => {};
    apiMock.messageFlags.mockImplementationOnce(
      () => new Promise((r) => { release = r; })
    );
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    const archive = screen.getByRole('button', { name: /^Archive/ });
    expect(archive).not.toBeDisabled();
    await fireEvent.click(archive);
    await waitFor(() => expect(apiMock.messageMove).toHaveBeenCalledTimes(1));
    release({ ok: true, uid: 42, added: ['\\Seen'], removed: [], confirmed: true, flags: [], seen: true, flagged: false });
  });

  it('a failed mark-read on open keeps the message unread and says why', async () => {
    apiMock.messageFlags.mockRejectedValueOnce(
      new EnvelopeApiError(409, 'message_not_found', 'this message is no longer in that folder', null)
    );
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText(/no longer in that folder/)).toBeInTheDocument());
    expect(screen.getByText('Unread')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Mark read' })).toBeInTheDocument();
  });

  it('does NOT auto-mark read when the detail load fails', async () => {
    readerApiMock.fetchMessageDetail.mockRejectedValueOnce(
      new EnvelopeApiError(502, 'imap_unavailable', 'IMAP down', null)
    );
    render(ReaderPane);
    await waitFor(() => expect(screen.getByRole('alert')).toBeInTheDocument());
    expect(readerApiMock.postFlags).not.toHaveBeenCalled();
    expect(apiMock.messageFlags).not.toHaveBeenCalled();
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
    expect(apiMock.messageFlags).not.toHaveBeenCalled();
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
      expect(apiMock.messageFlags).toHaveBeenCalledWith(
        'acct-a',
        42,
        expect.objectContaining({
          folder: 'INBOX',
          add: [],
          remove: ['\\Seen'],
          message_id: '<test@example.com>'
        })
      )
    );
    await waitFor(() => expect(screen.getByText('Unread')).toBeInTheDocument());
    expect(readState.isUnread('acct-a', 'INBOX', 42, false)).toBe(true);
  });

  it('treats the server’s index spelling ("Seen") as read', async () => {
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: { ...BASE_MSG, flags: ['Seen'] }
    });
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Read')).toBeInTheDocument());
    expect(screen.getByRole('button', { name: 'Mark unread' })).toBeInTheDocument();
  });

  it('shows "Read" badge when message has \\Seen flag', async () => {
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: { ...BASE_MSG, flags: ['\\Seen'] }
    });
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Read')).toBeInTheDocument());
    expect(screen.queryByText('Unread')).not.toBeInTheDocument();
  });

  it('the read toggle flips its label after each change: Mark unread → Mark read → Mark unread', async () => {
    render(ReaderPane); // BASE_MSG is unread (flags: []); opening marks it read
    await waitFor(() => expect(screen.getByRole('button', { name: 'Mark unread' })).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Mark unread' }));
    await waitFor(() => expect(screen.getByText('Unread')).toBeInTheDocument());
    expect(screen.getByRole('button', { name: 'Mark read' })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Mark unread' })).not.toBeInTheDocument();
    await fireEvent.click(screen.getByRole('button', { name: 'Mark read' }));
    await waitFor(() => expect(screen.getByText('Read')).toBeInTheDocument());
    expect(screen.getByRole('button', { name: 'Mark unread' })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Mark read' })).not.toBeInTheDocument();
    expect(apiMock.messageFlags.mock.calls.map((c) => c[2].add)).toEqual([
      ['\\Seen'],
      [],
      ['\\Seen']
    ]);
  });

  it('a failed Mark read keeps the message unread and says why', async () => {
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: { ...BASE_MSG, flags: ['\\Seen'] }
    });
    render(ReaderPane);
    await waitFor(() => expect(screen.getByRole('button', { name: 'Mark unread' })).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Mark unread' }));
    await waitFor(() => expect(screen.getByText('Unread')).toBeInTheDocument());
    apiMock.messageFlags.mockRejectedValueOnce(
      new EnvelopeApiError(409, 'message_not_found', 'this message is no longer in that folder', null)
    );
    await fireEvent.click(screen.getByRole('button', { name: 'Mark read' }));
    await waitFor(() => expect(screen.getByText(/no longer in that folder/)).toBeInTheDocument());
    expect(screen.getByText('Unread')).toBeInTheDocument();
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
    await waitFor(() => expect(apiMock.messageMove).toHaveBeenCalled());
    expect(apiMock.messageMove).toHaveBeenCalledWith('acct-a', 42, {
      folder: 'INBOX',
      to_folder: '\\Archive',
      uidvalidity: null,
      message_id: '<test@example.com>'
    });
    await waitFor(() => expect(ops.version).toBe(1));
    expect(goto).toHaveBeenCalledWith('/v2/mail/unified');
  });

  it('Delete outside Trash moves the message to \\Trash without a confirm', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Delete' }));
    await waitFor(() => expect(apiMock.messageMove).toHaveBeenCalled());
    expect(apiMock.messageMove.mock.calls[0][2]).toMatchObject({ folder: 'INBOX', to_folder: '\\Trash' });
    await waitFor(() => expect(goto).toHaveBeenCalledWith('/v2/mail/unified'));
  });

  it('Move to Junk targets the open message’s own folder, then leaves the reader', async () => {
    pageState.url = new URL('http://localhost/v2/mail/unified/acct-a/42?folder=Newsletters') as typeof pageState.url;
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Move to Junk' }));
    await waitFor(() => expect(apiMock.messageMove).toHaveBeenCalled());
    expect(apiMock.messageMove.mock.calls[0][2]).toMatchObject({
      folder: 'Newsletters',
      to_folder: '\\Junk'
    });
    await waitFor(() => expect(goto).toHaveBeenCalledWith('/v2/mail/unified'));
  });

  it('inside Junk the reader offers Not junk, which moves to Inbox', async () => {
    pageState.url = new URL('http://localhost/v2/mail/unified/acct-a/42?folder=Junk') as typeof pageState.url;
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Not junk: move to Inbox' }));
    await waitFor(() => expect(apiMock.messageMove).toHaveBeenCalled());
    expect(apiMock.messageMove.mock.calls[0][2]).toMatchObject({ folder: 'Junk', to_folder: 'INBOX' });
  });

  it('the reader’s More actions lists Remind and Follow up as unavailable', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'More actions' }));
    const remind = screen.getByText('Remind me…').closest('[role="menuitem"]')!;
    expect(remind).toHaveAttribute('aria-disabled', 'true');
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
    apiMock.messageMove.mockRejectedValueOnce(new EnvelopeApiError(502, 'http_502', 'IMAP down', null));
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));
    await waitFor(() => expect(screen.getByText(/IMAP down/)).toBeInTheDocument());
    expect(goto).not.toHaveBeenCalled();
    expect(getMailboxOpsStore().version).toBe(0);
  });

  it('Flag sets \\Flagged on the open message and flips to Unflag', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Flag' }));
    await waitFor(() =>
      expect(apiMock.messageFlags).toHaveBeenCalledWith(
        'acct-a',
        42,
        expect.objectContaining({ folder: 'INBOX', add: ['\\Flagged'], remove: [] })
      )
    );
    await waitFor(() => expect(screen.getByRole('button', { name: 'Unflag' })).toBeInTheDocument());
  });

  it('Unflag removes \\Flagged when the message is already flagged (index spelling)', async () => {
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: { ...BASE_MSG, flags: ['Flagged'] }
    });
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Unflag' }));
    await waitFor(() =>
      expect(apiMock.messageFlags).toHaveBeenCalledWith(
        'acct-a',
        42,
        expect.objectContaining({ add: [], remove: ['\\Flagged'] })
      )
    );
  });
});

// ── Phase C: reader as a document ─────────────────────────────────────
// Header hierarchy (subject headline + removable folder chip + right-edge
// cluster), an exact timestamp, and a Quick Reply that PROMOTES to the review
// composer — never a new send path.

describe('ReaderPane — document view (Phase C)', () => {
  it('shows an exact timestamp as a <time> with a machine datetime', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    const time = document.querySelector('time.msg-meta-date') as HTMLTimeElement | null;
    expect(time).not.toBeNull();
    expect(time!.getAttribute('datetime')).toBe('2026-07-08T10:00:00Z');
    // Down to the second, per A5.
    expect(time!.textContent).toMatch(/\d{1,2}:\d{2}:\d{2}/);
  });

  it('the folder chip ✕ archives (removes from the source folder)', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Remove from Inbox' }));
    await waitFor(() => expect(apiMock.messageMove).toHaveBeenCalled());
    expect(apiMock.messageMove.mock.calls[0][2]).toMatchObject({ folder: 'INBOX', to_folder: '\\Archive' });
  });

  it('Details reveals the full headers (To, UID)', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    expect(screen.queryByText('uid 42')).not.toBeInTheDocument();
    await fireEvent.click(screen.getByRole('button', { name: 'Details' }));
    await waitFor(() => expect(screen.getByText('uid 42')).toBeInTheDocument());
  });

  it('Quick Reply carries typed text into the composer, above the quote — no direct send', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    const box = screen.getByPlaceholderText(/reply to/i);
    await fireEvent.input(box, { target: { value: 'On it, thanks.' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Reply in composer' }));
    const composer = getComposerStore();
    expect(composer.isOpen).toBe(true);
    expect(composer.mode).toBe('reply');
    expect(composer.context.parentUid).toBe(42);
    // Typed text leads; the quote follows. The composer is the only send path.
    expect(composer.context.bodyPrefix?.startsWith('On it, thanks.')).toBe(true);
    expect(composer.context.bodyPrefix).toContain('sender@example.com wrote:');
  });

  it('Quick Reply with no text still promotes to the composer with just the quote', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Reply in composer' }));
    const composer = getComposerStore();
    expect(composer.isOpen).toBe(true);
    expect(composer.context.bodyPrefix).toContain('sender@example.com wrote:');
  });

  it('Cmd/Ctrl+Enter in the Quick Reply box promotes to the composer', async () => {
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    const box = screen.getByPlaceholderText(/reply to/i);
    await fireEvent.input(box, { target: { value: 'Quick keyboard reply.' } });
    // A plain Enter must NOT promote (it inserts a newline in the textarea).
    await fireEvent.keyDown(box, { key: 'Enter' });
    expect(getComposerStore().isOpen).toBe(false);
    // Cmd/Ctrl+Enter promotes, carrying the typed text.
    await fireEvent.keyDown(box, { key: 'Enter', metaKey: true });
    const composer = getComposerStore();
    expect(composer.isOpen).toBe(true);
    expect(composer.mode).toBe('reply');
    expect(composer.context.bodyPrefix?.startsWith('Quick keyboard reply.')).toBe(true);
  });

  it('keeps a dotted leaf name intact in the folder chip on a /-hierarchy server', async () => {
    pageState.url = new URL(
      'http://localhost/v2/mail/unified/acct-a/42?folder=Clients/Acme.com'
    ) as typeof pageState.url;
    render(ReaderPane);
    await waitFor(() => expect(screen.getByText('Test subject')).toBeInTheDocument());
    // Not 'Remove from Com' — the '.' in the leaf must not be a separator here.
    expect(screen.getByRole('button', { name: 'Remove from Acme.com' })).toBeInTheDocument();
  });
});
