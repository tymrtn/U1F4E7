// Tests for the draft review composer mounted at
// /accounts/[account]/drafts/[draft] — the surface CLI/MCP `review_url` and
// the cockpit Edit action deep-link into.
//
// Coverage:
//   • loads the exact draft named by the route params
//   • renders an editable composer (recipients, subject, body), not a card
//   • saves through the edit endpoint carrying the viewed expected_revision
//   • surfaces revision conflicts (409) instead of clobbering
//   • not-found / API error / loading states
//   • send is explicit: confirmation required, confirm=true + revision sent,
//     and blocked while edits are unsaved
//   • non-editable statuses render read-only

import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { page as pageState } from '$app/state';

const { apiMock } = vi.hoisted(() => ({
  apiMock: {
    draft: vi.fn(),
    editDraft: vi.fn(),
    sendDraft: vi.fn(),
    discardDraft: vi.fn()
  }
}));

vi.mock('$lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/api')>();
  return { ...actual, api: { ...actual.api, ...apiMock } };
});

import DraftComposer from './DraftComposer.svelte';
import { EnvelopeApiError, type Draft } from '$lib/api';

const ACCOUNT = '31f5fddf-04f9-4978-aea5-29aa9af12bb0';
const DRAFT = '365d958c-6666-4872-898e-cb8a60f21aca';
const DRAFT_B = '0f6c2a11-2222-4c6e-9f01-aaaaaaaaaaaa';

const BASE_DRAFT: Draft = {
  id: DRAFT,
  account_id: ACCOUNT,
  status: 'draft',
  to_addr: 'buyer@example.com',
  cc_addr: 'cc@example.com',
  bcc_addr: null,
  reply_to: null,
  subject: 'Quarterly update',
  text_content: 'Hello there',
  html_content: null,
  in_reply_to: null,
  metadata: null,
  attachments: [],
  message_id: null,
  send_after: null,
  snoozed_until: null,
  created_at: '2026-07-30T10:00:00Z',
  updated_at: '2026-07-30T10:00:00Z',
  sent_at: null,
  created_by: 'agent',
  revision: 7
};

function draftResponse(overrides: Partial<Draft> = {}) {
  return { draft: { ...BASE_DRAFT, ...overrides } };
}

/** Load the composer and wait for the draft to render. */
async function renderLoaded(overrides: Partial<Draft> = {}) {
  apiMock.draft.mockResolvedValue(draftResponse(overrides));
  render(DraftComposer);
  await waitFor(() => expect(screen.getByLabelText('To')).toBeInTheDocument());
}

beforeEach(() => {
  pageState.params = { account: ACCOUNT, draft: DRAFT };
  pageState.url = new URL(`http://localhost/accounts/${ACCOUNT}/drafts/${DRAFT}`) as typeof pageState.url;

  apiMock.draft.mockResolvedValue(draftResponse());
  apiMock.editDraft.mockImplementation((_a: string, _d: string, body: { expected_revision: number }) =>
    Promise.resolve({ draft: { ...BASE_DRAFT, revision: body.expected_revision + 1 }, status: 'edited' })
  );
  apiMock.sendDraft.mockResolvedValue({
    draft_id: DRAFT,
    sent: false,
    status: 'queued',
    send_after: '2026-07-30T10:02:00Z',
    cooldown_seconds: 120,
    queued_reason_code: 'outbox_cooldown',
    queued_reason: 'held in the outbox cooldown'
  });
});

afterEach(() => {
  vi.clearAllMocks();
});

// ── Loading the exact draft ───────────────────────────────────────────

describe('DraftComposer load', () => {
  it('fetches the draft named by the route params', async () => {
    await renderLoaded();
    expect(apiMock.draft).toHaveBeenCalledWith(ACCOUNT, DRAFT);
  });

  it('shows a loading state before the draft arrives', async () => {
    let resolve!: (value: unknown) => void;
    apiMock.draft.mockReturnValueOnce(new Promise((r) => (resolve = r)));
    render(DraftComposer);
    expect(screen.getByRole('status', { name: /loading/i })).toBeInTheDocument();
    resolve(draftResponse());
    await waitFor(() => expect(screen.getByLabelText('To')).toBeInTheDocument());
  });

  it('renders an editable composer with the draft content — not a read-only card', async () => {
    await renderLoaded();
    expect((screen.getByLabelText('To') as HTMLInputElement).value).toBe('buyer@example.com');
    expect((screen.getByLabelText('Cc') as HTMLInputElement).value).toBe('cc@example.com');
    expect((screen.getByLabelText('Subject') as HTMLInputElement).value).toBe('Quarterly update');
    expect((screen.getByLabelText('Message') as HTMLTextAreaElement).value).toBe('Hello there');
    for (const field of ['To', 'Cc', 'Subject', 'Message']) {
      expect(screen.getByLabelText(field)).not.toBeDisabled();
    }
  });

  it('shows a not-found state when the draft does not exist', async () => {
    apiMock.draft.mockRejectedValueOnce(
      new EnvelopeApiError(404, 'http_404', 'draft not found', null)
    );
    render(DraftComposer);
    await waitFor(() => expect(document.getElementById('draft-not-found')).toBeTruthy());
    expect(screen.queryByLabelText('To')).not.toBeInTheDocument();
  });

  it('shows an error state with a stable code when the load fails', async () => {
    apiMock.draft.mockRejectedValueOnce(
      new EnvelopeApiError(500, 'db_error', 'database unavailable', null)
    );
    render(DraftComposer);
    await waitFor(() => expect(screen.getByRole('alert')).toBeInTheDocument());
    expect(screen.getByText('db_error')).toBeInTheDocument();
  });
});

// ── Editing ───────────────────────────────────────────────────────────

describe('DraftComposer edit', () => {
  it('saves recipients, subject and body with the viewed expected_revision', async () => {
    await renderLoaded();

    await fireEvent.input(screen.getByLabelText('To'), { target: { value: 'new@example.com' } });
    await fireEvent.input(screen.getByLabelText('Subject'), { target: { value: 'Revised subject' } });
    await fireEvent.input(screen.getByLabelText('Message'), { target: { value: 'Revised body' } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));

    await waitFor(() => expect(apiMock.editDraft).toHaveBeenCalled());
    expect(apiMock.editDraft).toHaveBeenCalledWith(ACCOUNT, DRAFT, {
      expected_revision: 7,
      to_addr: 'new@example.com',
      cc_addr: 'cc@example.com',
      bcc_addr: '',
      subject: 'Revised subject',
      text_content: 'Revised body'
    });
  });

  it('sends only the edited body format so the stale alternate is cleared', async () => {
    await renderLoaded({ text_content: null, html_content: '<p>Hi</p>' });
    await fireEvent.input(screen.getByLabelText('Message'), { target: { value: '<p>Bye</p>' } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));

    await waitFor(() => expect(apiMock.editDraft).toHaveBeenCalled());
    const body = apiMock.editDraft.mock.calls[0][2];
    expect(body.html_content).toBe('<p>Bye</p>');
    expect(body).not.toHaveProperty('text_content');
  });

  it('advances to the revision the server returns, so a second save is not stale', async () => {
    await renderLoaded();
    await fireEvent.input(screen.getByLabelText('Subject'), { target: { value: 'First' } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));
    await waitFor(() => expect(apiMock.editDraft).toHaveBeenCalledTimes(1));

    await fireEvent.input(screen.getByLabelText('Subject'), { target: { value: 'Second' } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));
    await waitFor(() => expect(apiMock.editDraft).toHaveBeenCalledTimes(2));
    expect(apiMock.editDraft.mock.calls[1][2].expected_revision).toBe(8);
  });

  it('surfaces a revision conflict instead of retrying the overwrite', async () => {
    await renderLoaded();
    apiMock.editDraft.mockRejectedValueOnce(
      new EnvelopeApiError(409, 'http_409', 'draft modified concurrently', null)
    );

    await fireEvent.input(screen.getByLabelText('Subject'), { target: { value: 'Mine' } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));

    await waitFor(() => expect(document.getElementById('draft-conflict')).toBeTruthy());
    expect(apiMock.editDraft).toHaveBeenCalledTimes(1);
    expect(screen.getByRole('button', { name: /reload/i })).toBeInTheDocument();
  });

  it('reloads the latest draft from the conflict banner', async () => {
    await renderLoaded();
    apiMock.editDraft.mockRejectedValueOnce(
      new EnvelopeApiError(409, 'http_409', 'draft modified concurrently', null)
    );
    await fireEvent.input(screen.getByLabelText('Subject'), { target: { value: 'Mine' } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));
    await waitFor(() => expect(document.getElementById('draft-conflict')).toBeTruthy());

    apiMock.draft.mockResolvedValueOnce(draftResponse({ subject: 'Theirs', revision: 9 }));
    await fireEvent.click(screen.getByRole('button', { name: /reload/i }));

    await waitFor(() =>
      expect((screen.getByLabelText('Subject') as HTMLInputElement).value).toBe('Theirs')
    );
    expect(document.getElementById('draft-conflict')).toBeFalsy();
  });

  it('confirms a successful save', async () => {
    await renderLoaded();
    await fireEvent.input(screen.getByLabelText('Subject'), { target: { value: 'Saved' } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));
    await waitFor(() => expect(screen.getByText(/changes saved/i)).toBeInTheDocument());
  });
});

// ── Sending ───────────────────────────────────────────────────────────

describe('DraftComposer send', () => {
  it('requires explicit confirmation before queueing', async () => {
    await renderLoaded();
    await fireEvent.click(screen.getByRole('button', { name: /^send/i }));

    expect(apiMock.sendDraft).not.toHaveBeenCalled();
    await waitFor(() => expect(screen.getByRole('dialog')).toBeInTheDocument());
  });

  it('queues with confirm=true and the reviewed revision once confirmed', async () => {
    await renderLoaded();
    await fireEvent.click(screen.getByRole('button', { name: /^send/i }));
    await waitFor(() => expect(screen.getByRole('dialog')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: /queue for sending/i }));

    await waitFor(() =>
      expect(apiMock.sendDraft).toHaveBeenCalledWith(ACCOUNT, DRAFT, {
        confirm: true,
        expected_revision: 7
      })
    );
  });

  it('reports the queued outcome honestly — queued, not sent', async () => {
    await renderLoaded();
    await fireEvent.click(screen.getByRole('button', { name: /^send/i }));
    await waitFor(() => expect(screen.getByRole('dialog')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: /queue for sending/i }));

    await waitFor(() => expect(document.getElementById('draft-queued')).toBeTruthy());
    const banner = document.getElementById('draft-queued');
    expect(banner?.textContent).toMatch(/queued for sending/i);
    // Honest about what actually happened: queued into the outbox, not sent.
    expect(banner?.textContent).toMatch(/nothing has been transmitted yet/i);
    expect(banner?.textContent).not.toMatch(/\bsent\b/i);
  });

  it('badges the draft as Queued rather than Draft once it is in the outbox', async () => {
    await renderLoaded({ send_after: '2026-07-30T10:02:00Z' });
    expect(screen.getByText('Queued')).toBeInTheDocument();
    expect(screen.queryByText('Draft')).not.toBeInTheDocument();
  });

  it('cancelling the confirmation queues nothing', async () => {
    await renderLoaded();
    await fireEvent.click(screen.getByRole('button', { name: /^send/i }));
    await waitFor(() => expect(screen.getByRole('dialog')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: /keep editing/i }));

    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
    expect(apiMock.sendDraft).not.toHaveBeenCalled();
  });

  it('blocks sending while edits are unsaved, so the queued copy is the reviewed one', async () => {
    await renderLoaded();
    await fireEvent.input(screen.getByLabelText('Subject'), { target: { value: 'Unsaved' } });

    expect(screen.getByRole('button', { name: /^send/i })).toBeDisabled();
    expect(screen.getByText(/save your changes/i)).toBeInTheDocument();
    expect(apiMock.sendDraft).not.toHaveBeenCalled();
  });

  it('surfaces a send conflict with a stable code', async () => {
    await renderLoaded();
    apiMock.sendDraft.mockRejectedValueOnce(
      new EnvelopeApiError(409, 'http_409', 'draft modified concurrently', null)
    );
    await fireEvent.click(screen.getByRole('button', { name: /^send/i }));
    await waitFor(() => expect(screen.getByRole('dialog')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: /queue for sending/i }));

    await waitFor(() => expect(document.getElementById('draft-conflict')).toBeTruthy());
  });
});

// ── Non-editable statuses ─────────────────────────────────────────────

describe('DraftComposer status guards', () => {
  it('renders a sent draft read-only with no send action', async () => {
    apiMock.draft.mockResolvedValue(draftResponse({ status: 'sent', sent_at: '2026-07-30T11:00:00Z' }));
    render(DraftComposer);
    await waitFor(() => expect(screen.getByLabelText('To')).toBeInTheDocument());

    expect(screen.getByLabelText('To')).toBeDisabled();
    expect(screen.getByLabelText('Message')).toBeDisabled();
    expect(screen.queryByRole('button', { name: /^send/i })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /save/i })).not.toBeInTheDocument();
  });

  it('lets a blocked draft be edited but not queued', async () => {
    apiMock.draft.mockResolvedValue(draftResponse({ status: 'blocked' }));
    render(DraftComposer);
    await waitFor(() => expect(screen.getByLabelText('To')).toBeInTheDocument());

    expect(screen.getByLabelText('To')).not.toBeDisabled();
    expect(screen.getByRole('button', { name: /save/i })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /^send/i })).not.toBeInTheDocument();
  });

  it('renders a draft claimed by the send sweep read-only', async () => {
    apiMock.draft.mockResolvedValue(draftResponse({ status: 'sending' }));
    render(DraftComposer);
    await waitFor(() => expect(screen.getByLabelText('To')).toBeInTheDocument());
    expect(screen.getByLabelText('To')).toBeDisabled();
  });
});

// ── Already-queued drafts (persisted send_after) ──────────────────────
//
// `list_drafts_due_for_send` sweeps every `draft`-status row with a non-null
// `send_after`, and the edit statement never clears `send_after` — it only
// strips the approval attestation. So an edit made on a queued draft still
// gets transmitted, just without `tyler_approved`. Reload must therefore
// recover the queued state from the draft itself, not only from the transient
// response of a send this page happened to perform.

describe('DraftComposer queued state recovered on reload', () => {
  const QUEUED_AT = '2026-07-30T10:02:00Z';

  it('treats a persisted send_after as queued and locks the editor', async () => {
    await renderLoaded({ send_after: QUEUED_AT });

    expect(screen.getByLabelText('To')).toBeDisabled();
    expect(screen.getByLabelText('Cc')).toBeDisabled();
    expect(screen.getByLabelText('Subject')).toBeDisabled();
    expect(screen.getByLabelText('Message')).toBeDisabled();
  });

  it('offers neither Save nor Send for an already-queued draft', async () => {
    await renderLoaded({ send_after: QUEUED_AT });

    expect(screen.queryByRole('button', { name: /save/i })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /^send/i })).not.toBeInTheDocument();
  });

  it('shows the queued banner and the scheduled time without a fresh send response', async () => {
    await renderLoaded({ send_after: QUEUED_AT });

    const banner = document.getElementById('draft-queued');
    expect(banner).toBeTruthy();
    expect(banner?.textContent).toMatch(/queued for sending/i);
    // The resolved local time is rendered, not the raw ISO string.
    expect(banner?.textContent).not.toContain(QUEUED_AT);
    expect(banner?.textContent).toMatch(/\d/);
  });

  it('does not mistake a sent draft for a queued one', async () => {
    // A sent draft keeps the send_after it was queued with; only pre-terminal
    // rows are still headed for the sweep.
    await renderLoaded({ status: 'sent', send_after: QUEUED_AT, sent_at: '2026-07-30T10:05:00Z' });

    expect(document.getElementById('draft-queued')).toBeFalsy();
  });

  it('still opens a normal draft with no send_after as editable', async () => {
    await renderLoaded({ send_after: null });

    expect(screen.getByLabelText('To')).not.toBeDisabled();
    expect(screen.getByRole('button', { name: /^send/i })).toBeInTheDocument();
  });
});

// ── Recipient guard ───────────────────────────────────────────────────

describe('DraftComposer recipient guard', () => {
  it('keeps Send disabled when the stored recipient is empty', async () => {
    await renderLoaded({ to_addr: '' });

    expect(screen.getByRole('button', { name: /^send/i })).toBeDisabled();
    expect(screen.getByText(/valid recipient/i)).toBeInTheDocument();
  });

  it('keeps Send disabled when the stored recipient is not a usable address', async () => {
    await renderLoaded({ to_addr: 'not-an-address' });

    expect(screen.getByRole('button', { name: /^send/i })).toBeDisabled();
  });

  it('never opens the confirmation for a draft with no recipient', async () => {
    await renderLoaded({ to_addr: '' });

    await fireEvent.click(screen.getByRole('button', { name: /^send/i }));

    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(apiMock.sendDraft).not.toHaveBeenCalled();
  });

  it('accepts a comma-separated recipient list', async () => {
    await renderLoaded({ to_addr: 'a@example.com, b@example.com' });

    expect(screen.getByRole('button', { name: /^send/i })).toBeEnabled();
  });

  it('accepts a display-name address', async () => {
    await renderLoaded({ to_addr: 'Ada Lovelace <ada@example.com>' });

    expect(screen.getByRole('button', { name: /^send/i })).toBeEnabled();
  });

  it('re-disables Send when the recipient is edited down to nothing', async () => {
    await renderLoaded();
    await fireEvent.input(screen.getByLabelText('To'), { target: { value: '' } });

    expect(screen.getByRole('button', { name: /^send/i })).toBeDisabled();
    expect(screen.getByRole('button', { name: /save/i })).toBeDisabled();
  });
});

// ── Save race ─────────────────────────────────────────────────────────

describe('DraftComposer save race', () => {
  it('locks the editor while a save is in flight so later keystrokes are not silently overwritten', async () => {
    await renderLoaded();

    let resolveSave!: (value: unknown) => void;
    apiMock.editDraft.mockReturnValueOnce(new Promise((r) => (resolveSave = r)));

    await fireEvent.input(screen.getByLabelText('Subject'), { target: { value: 'First' } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));

    await waitFor(() => expect(screen.getByLabelText('Subject')).toBeDisabled());
    expect(screen.getByLabelText('To')).toBeDisabled();
    expect(screen.getByLabelText('Cc')).toBeDisabled();
    expect(screen.getByLabelText('Message')).toBeDisabled();

    resolveSave({ draft: { ...BASE_DRAFT, subject: 'First', revision: 8 }, status: 'edited' });

    await waitFor(() => expect(screen.getByLabelText('Subject')).not.toBeDisabled());
  });

  it('unlocks the editor again when a save fails', async () => {
    await renderLoaded();
    apiMock.editDraft.mockRejectedValueOnce(
      new EnvelopeApiError(500, 'db_error', 'database unavailable', null)
    );

    await fireEvent.input(screen.getByLabelText('Subject'), { target: { value: 'First' } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));

    await waitFor(() => expect(document.getElementById('draft-action-error')).toBeTruthy());
    expect(screen.getByLabelText('Subject')).not.toBeDisabled();
  });
});

// ── Queue cancellation race ───────────────────────────────────────────

describe('DraftComposer queue cancellation race', () => {
  /** Start a send and leave the POST pending. Returns its resolver. */
  async function beginQueueing(): Promise<(value: unknown) => void> {
    let resolveSend!: (value: unknown) => void;
    apiMock.sendDraft.mockReturnValueOnce(new Promise((r) => (resolveSend = r)));

    await fireEvent.click(screen.getByRole('button', { name: /^send/i }));
    await waitFor(() => expect(screen.getByRole('dialog')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: /queue for sending/i }));
    await waitFor(() =>
      expect(screen.getByRole('button', { name: /keep editing/i })).toBeDisabled()
    );
    return resolveSend;
  }

  it('disables Keep editing once the queue request is in flight', async () => {
    await renderLoaded();
    const resolveSend = await beginQueueing();

    await fireEvent.click(screen.getByRole('button', { name: /keep editing/i }));
    expect(screen.getByRole('dialog')).toBeInTheDocument();

    resolveSend({
      draft_id: DRAFT,
      sent: false,
      status: 'queued',
      send_after: '2026-07-30T10:02:00Z',
      cooldown_seconds: 120,
      queued_reason_code: 'safety_cooldown',
      queued_reason: 'held in the outbox cooldown'
    });
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
  });

  it('ignores Escape while the queue request is in flight', async () => {
    await renderLoaded();
    await beginQueueing();

    await fireEvent.keyDown(window, { key: 'Escape' });

    expect(screen.getByRole('dialog')).toBeInTheDocument();
  });

  it('ignores a backdrop click while the queue request is in flight', async () => {
    await renderLoaded();
    await beginQueueing();

    const backdrop = document.querySelector('.env-modal-backdrop');
    expect(backdrop).toBeTruthy();
    await fireEvent.click(backdrop!);

    expect(screen.getByRole('dialog')).toBeInTheDocument();
  });

  it('ignores the dialog close button while the queue request is in flight', async () => {
    await renderLoaded();
    await beginQueueing();

    await fireEvent.click(screen.getByRole('button', { name: /close/i }));

    expect(screen.getByRole('dialog')).toBeInTheDocument();
  });

  it('a queue that ultimately fails closes the dialog and reports the error', async () => {
    await renderLoaded();

    let rejectSend!: (reason: unknown) => void;
    apiMock.sendDraft.mockReturnValueOnce(new Promise((_r, reject) => (rejectSend = reject)));

    await fireEvent.click(screen.getByRole('button', { name: /^send/i }));
    await waitFor(() => expect(screen.getByRole('dialog')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: /queue for sending/i }));
    await waitFor(() =>
      expect(screen.getByRole('button', { name: /keep editing/i })).toBeDisabled()
    );

    rejectSend(new EnvelopeApiError(500, 'db_error', 'database unavailable', null));

    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
    expect(document.getElementById('draft-action-error')).toBeTruthy();
  });
});

// ── Route-change race ─────────────────────────────────────────────────
//
// The composer is a single mounted instance that re-targets when the route
// params change. An in-flight load, an open send confirmation, or a stale
// loaded draft must never cross that boundary: they belong to the draft the
// operator has already navigated away from.

describe('DraftComposer route change', () => {
  /** Point the route at another draft on the already-mounted component. */
  function navigateTo(draftId: string, account = ACCOUNT) {
    pageState.params = { account, draft: draftId };
    pageState.url = new URL(
      `http://localhost/accounts/${account}/drafts/${draftId}`
    ) as typeof pageState.url;
  }

  it('ignores a load that resolves after the route moved to another draft', async () => {
    let resolveA!: (value: unknown) => void;
    apiMock.draft
      .mockReturnValueOnce(new Promise((r) => (resolveA = r)))
      .mockResolvedValueOnce(draftResponse({ id: DRAFT_B, subject: 'Draft B' }));

    render(DraftComposer);
    await waitFor(() => expect(apiMock.draft).toHaveBeenCalledWith(ACCOUNT, DRAFT));

    navigateTo(DRAFT_B);
    await waitFor(() => expect(apiMock.draft).toHaveBeenCalledWith(ACCOUNT, DRAFT_B));
    await waitFor(() =>
      expect((screen.getByLabelText('Subject') as HTMLInputElement).value).toBe('Draft B')
    );

    // Draft A's response lands late; it must not repaint the editor.
    resolveA(draftResponse({ subject: 'Draft A' }));
    await new Promise((r) => setTimeout(r, 0));

    expect((screen.getByLabelText('Subject') as HTMLInputElement).value).toBe('Draft B');
  });

  it('does not let a stale load resurrect an error state over the new draft', async () => {
    let rejectA!: (reason: unknown) => void;
    apiMock.draft
      .mockReturnValueOnce(new Promise((_r, reject) => (rejectA = reject)))
      .mockResolvedValueOnce(draftResponse({ id: DRAFT_B, subject: 'Draft B' }));

    render(DraftComposer);
    await waitFor(() => expect(apiMock.draft).toHaveBeenCalledWith(ACCOUNT, DRAFT));

    navigateTo(DRAFT_B);
    await waitFor(() =>
      expect((screen.getByLabelText('Subject') as HTMLInputElement).value).toBe('Draft B')
    );

    rejectA(new EnvelopeApiError(500, 'db_error', 'database unavailable', null));
    await new Promise((r) => setTimeout(r, 0));

    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect((screen.getByLabelText('Subject') as HTMLInputElement).value).toBe('Draft B');
  });

  it('closes an open send confirmation when the route changes', async () => {
    await renderLoaded();
    await fireEvent.click(screen.getByRole('button', { name: /^send/i }));
    await waitFor(() => expect(screen.getByRole('dialog')).toBeInTheDocument());

    apiMock.draft.mockResolvedValueOnce(draftResponse({ id: DRAFT_B, subject: 'Draft B' }));
    navigateTo(DRAFT_B);

    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
    expect(apiMock.sendDraft).not.toHaveBeenCalled();
  });

  it('clears the queued banner when the route changes to a fresh draft', async () => {
    await renderLoaded({ send_after: '2026-07-30T10:02:00Z' });
    expect(document.getElementById('draft-queued')).toBeTruthy();

    apiMock.draft.mockResolvedValueOnce(draftResponse({ id: DRAFT_B, subject: 'Draft B' }));
    navigateTo(DRAFT_B);

    await waitFor(() =>
      expect((screen.getByLabelText('Subject') as HTMLInputElement).value).toBe('Draft B')
    );
    expect(document.getElementById('draft-queued')).toBeFalsy();
  });

  it('fails visibly rather than spinning forever when the route has no draft id', async () => {
    pageState.params = { account: ACCOUNT, draft: '' };
    render(DraftComposer);

    await waitFor(() => expect(document.getElementById('draft-not-found')).toBeTruthy());
    expect(screen.queryByRole('status', { name: /loading/i })).not.toBeInTheDocument();
    expect(apiMock.draft).not.toHaveBeenCalled();
  });

  it('never saves or sends against a draft the route has moved away from', async () => {
    await renderLoaded();

    // Hold the new draft's load open: the previous draft is still in `draft`
    // state, but its identity no longer matches the route.
    apiMock.draft.mockReturnValueOnce(new Promise(() => {}));
    navigateTo(DRAFT_B);
    await waitFor(() => expect(apiMock.draft).toHaveBeenCalledWith(ACCOUNT, DRAFT_B));

    expect(screen.queryByRole('button', { name: /^send/i })).not.toBeInTheDocument();
    expect(apiMock.editDraft).not.toHaveBeenCalled();
    expect(apiMock.sendDraft).not.toHaveBeenCalled();
  });
});

// ── Body preservation on non-body edits ───────────────────────────────
//
// The edit endpoint treats the body pair as one unit: supplying either field
// replaces the pair and CLEARS the omitted alternate. That is correct when the
// body really changed, and destructive when it did not — a subject-only save
// would otherwise silently drop a dual-format draft's HTML part.

describe('DraftComposer body preservation', () => {
  const DUAL = { text_content: 'Plain body', html_content: '<p>Rich body</p>' };

  it('omits both body fields on a subject-only save of a dual-format draft', async () => {
    await renderLoaded(DUAL);
    await fireEvent.input(screen.getByLabelText('Subject'), { target: { value: 'New subject' } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));

    await waitFor(() => expect(apiMock.editDraft).toHaveBeenCalled());
    const body = apiMock.editDraft.mock.calls[0][2];
    expect(body).not.toHaveProperty('text_content');
    expect(body).not.toHaveProperty('html_content');
    expect(body.subject).toBe('New subject');
  });

  it('omits both body fields on a recipient-only save', async () => {
    await renderLoaded(DUAL);
    await fireEvent.input(screen.getByLabelText('To'), { target: { value: 'someone@example.com' } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));

    await waitFor(() => expect(apiMock.editDraft).toHaveBeenCalled());
    const body = apiMock.editDraft.mock.calls[0][2];
    expect(body).not.toHaveProperty('text_content');
    expect(body).not.toHaveProperty('html_content');
    expect(body.to_addr).toBe('someone@example.com');
  });

  it('still replaces the pair — clearing the stale alternate — when the body changes', async () => {
    await renderLoaded(DUAL);
    await fireEvent.input(screen.getByLabelText('Message'), { target: { value: 'Rewritten' } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));

    await waitFor(() => expect(apiMock.editDraft).toHaveBeenCalled());
    const body = apiMock.editDraft.mock.calls[0][2];
    expect(body.text_content).toBe('Rewritten');
    expect(body).not.toHaveProperty('html_content');
  });

  it('sends the body when only the format changed', async () => {
    await renderLoaded(DUAL);
    await fireEvent.click(screen.getByRole('button', { name: 'HTML' }));
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));

    await waitFor(() => expect(apiMock.editDraft).toHaveBeenCalled());
    const body = apiMock.editDraft.mock.calls[0][2];
    expect(body).toHaveProperty('html_content');
    expect(body).not.toHaveProperty('text_content');
  });

  it('warns before a body edit drops the draft’s other format', async () => {
    await renderLoaded(DUAL);
    expect(screen.queryByText(/drops the other format/i)).not.toBeInTheDocument();

    await fireEvent.input(screen.getByLabelText('Message'), { target: { value: 'Rewritten' } });

    expect(screen.getByText(/drops the other format/i)).toBeInTheDocument();
  });

  it('does not warn about dropping a format when the draft only has one', async () => {
    await renderLoaded({ text_content: 'Only text', html_content: null });
    await fireEvent.input(screen.getByLabelText('Message'), { target: { value: 'Rewritten' } });

    expect(screen.queryByText(/drops the other format/i)).not.toBeInTheDocument();
  });
});

// ── Cc / Bcc validation ───────────────────────────────────────────────

describe('DraftComposer optional-recipient validation', () => {
  it('blocks Send when Cc is present but malformed', async () => {
    await renderLoaded({ cc_addr: 'not-an-address' });

    expect(screen.getByRole('button', { name: /^send/i })).toBeDisabled();
    expect(screen.getByText(/valid recipient/i)).toBeInTheDocument();
  });

  it('blocks Send when Bcc is present but malformed', async () => {
    await renderLoaded({ bcc_addr: 'nope@' });

    expect(screen.getByRole('button', { name: /^send/i })).toBeDisabled();
  });

  it('allows Send when Cc and Bcc are empty', async () => {
    await renderLoaded({ cc_addr: null, bcc_addr: null });

    expect(screen.getByRole('button', { name: /^send/i })).toBeEnabled();
  });

  it('allows Send for valid multi-entry Cc and Bcc', async () => {
    await renderLoaded({ cc_addr: 'a@example.com, b@example.com', bcc_addr: 'c@example.com' });

    expect(screen.getByRole('button', { name: /^send/i })).toBeEnabled();
  });

  it('never opens the confirmation when Cc is malformed', async () => {
    await renderLoaded({ cc_addr: 'broken' });

    await fireEvent.click(screen.getByRole('button', { name: /^send/i }));

    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(apiMock.sendDraft).not.toHaveBeenCalled();
  });
});
