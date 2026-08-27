// Drafts deep-link intercept in ReaderPane.
//
// Regression for the Drafts-folder deep link that landed on the read-only
// reader: `/mail/unified/{account}/{uid}?folder=Drafts` renders a message with
// no recipient fields and no Send, and opening it marks an unsent draft Seen.
// The working surface is /accounts/{account}/drafts/{localDraftId}, resolved
// through GET /api/accounts/{id}/drafts/by-imap-uid/{uid}.

import { render, screen, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { page as pageState } from '$app/state';
import { goto } from '$app/navigation';

const { readerApiMock, apiMock } = vi.hoisted(() => ({
  readerApiMock: {
    fetchMessageDetail: vi.fn(),
    fetchThread: vi.fn(),
    postFlags: vi.fn()
  },
  apiMock: {
    draftByImapUid: vi.fn()
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

vi.mock('$lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/api')>();
  return {
    ...actual,
    api: { ...actual.api, draftByImapUid: apiMock.draftByImapUid }
  };
});

import ReaderPane from './ReaderPane.svelte';
import { isDraftsFolder } from '$lib/mailboxes';
import { EnvelopeApiError } from '$lib/api';
import { __resetReadState } from '$lib/read-state.svelte';

const LOCAL_DRAFT_ID = '365d958c-6666-4872-898e-cb8a60f21aca';
const ACCOUNT = '9639d7f4-a187-473f-817d-b057b1038b9d';

/** Point the route at `folder`'s uid 38311 for this account. */
function routeTo(folder: string, uid = 38311, account = ACCOUNT) {
  pageState.params = { box: 'unified', account, uid: String(uid) };
  pageState.url = new URL(
    `http://localhost/mail/unified/${account}/${uid}?folder=${encodeURIComponent(folder)}`
  ) as typeof pageState.url;
}

beforeEach(() => {
  readerApiMock.fetchMessageDetail.mockResolvedValue({ message: null });
  readerApiMock.fetchThread.mockResolvedValue(null);
  readerApiMock.postFlags.mockResolvedValue({ ok: true, uid: 0, added: [], removed: [] });
  apiMock.draftByImapUid.mockResolvedValue({ draft: { id: LOCAL_DRAFT_ID } });
  __resetReadState();
});

afterEach(() => {
  vi.clearAllMocks();
  sessionStorage.clear();
});

// ── isDraftsFolder ────────────────────────────────────────────────────

describe('isDraftsFolder', () => {
  it('matches every name the backend classify_folder calls drafts', () => {
    for (const name of [
      'Drafts',
      '[Gmail]/Drafts',
      'Draft',
      'INBOX.Drafts',
      'INBOX/Drafts',
      'INBOX/draft',
      'drafts'
    ]) {
      expect(isDraftsFolder(name)).toBe(true);
    }
  });

  it('does not match non-draft mailboxes or empty input', () => {
    for (const name of ['INBOX', 'Sent', '[Gmail]/Sent Mail', 'Draft Reviews', 'Archive']) {
      expect(isDraftsFolder(name)).toBe(false);
    }
    expect(isDraftsFolder('')).toBe(false);
    expect(isDraftsFolder(null)).toBe(false);
    expect(isDraftsFolder(undefined)).toBe(false);
  });
});

// ── ReaderPane drafts intercept ───────────────────────────────────────

describe('ReaderPane drafts intercept', () => {
  it('resolves a Drafts deep link through by-imap-uid and navigates to the review composer', async () => {
    routeTo('Drafts');
    render(ReaderPane);

    await waitFor(() => expect(apiMock.draftByImapUid).toHaveBeenCalledWith(ACCOUNT, 38311));
    await waitFor(() =>
      expect(goto).toHaveBeenCalledWith(`/v2/accounts/${ACCOUNT}/drafts/${LOCAL_DRAFT_ID}`)
    );
  });

  it('never calls the read-message endpoint and never marks a draft read', async () => {
    routeTo('[Gmail]/Drafts');
    render(ReaderPane);

    await waitFor(() => expect(goto).toHaveBeenCalled());
    expect(readerApiMock.fetchMessageDetail).not.toHaveBeenCalled();
    expect(readerApiMock.postFlags).not.toHaveBeenCalled();
  });

  it('takes the same path for every drafts folder spelling', async () => {
    for (const folder of ['Draft', 'INBOX.Drafts', 'INBOX/Drafts', 'INBOX/draft']) {
      vi.clearAllMocks();
      apiMock.draftByImapUid.mockResolvedValue({ draft: { id: LOCAL_DRAFT_ID } });
      routeTo(folder);
      const { unmount } = render(ReaderPane);
      await waitFor(() =>
        expect(goto).toHaveBeenCalledWith(`/v2/accounts/${ACCOUNT}/drafts/${LOCAL_DRAFT_ID}`)
      );
      expect(readerApiMock.fetchMessageDetail).not.toHaveBeenCalled();
      unmount();
    }
  });

  it('renders a draft card — not the reader, not a 404 — when no local draft exists', async () => {
    apiMock.draftByImapUid.mockRejectedValueOnce(
      new EnvelopeApiError(404, 'http_404', 'draft not found', null)
    );
    routeTo('Drafts');
    render(ReaderPane);

    await waitFor(() => expect(document.getElementById('draft-card')).toBeTruthy());
    expect(goto).not.toHaveBeenCalled();
    expect(readerApiMock.fetchMessageDetail).not.toHaveBeenCalled();
    expect(readerApiMock.postFlags).not.toHaveBeenCalled();
    expect(screen.getByText('uid 38311')).toBeInTheDocument();
    // The read-only reader's empty state must not stand in for the card.
    expect(screen.queryByText('Select a message to read it.')).not.toBeInTheDocument();
  });

  it('surfaces a non-404 draft lookup failure as an error with a stable code', async () => {
    apiMock.draftByImapUid.mockRejectedValueOnce(
      new EnvelopeApiError(500, 'db_error', 'db error: locked', null)
    );
    routeTo('Drafts');
    render(ReaderPane);

    await waitFor(() => expect(screen.getByRole('alert')).toBeInTheDocument());
    expect(screen.getByText('db_error')).toBeInTheDocument();
    expect(goto).not.toHaveBeenCalled();
    expect(readerApiMock.fetchMessageDetail).not.toHaveBeenCalled();
  });

  it('leaves non-draft folders on the reader path', async () => {
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: {
        uid: 38311,
        message_id: null,
        from_addr: 'sender@example.com',
        to_addr: 'me@example.com',
        to_addrs: ['me@example.com'],
        cc_addrs: [],
        subject: 'A sent message',
        date: null,
        flags: ['\\Seen'],
        text_body: 'Body',
        html_body: null,
        attachments: []
      }
    });
    routeTo('[Gmail]/Sent Mail');
    render(ReaderPane);

    await waitFor(() => expect(screen.getByText('A sent message')).toBeInTheDocument());
    expect(apiMock.draftByImapUid).not.toHaveBeenCalled();
    expect(goto).not.toHaveBeenCalled();
  });
});
