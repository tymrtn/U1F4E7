// Tests for the shared compose drawer's recipient gating.
//
// Cc and Bcc are optional headers, but a malformed one still reaches real
// people via SMTP, so Send has to hold out for them the same way it does for
// To. Blank stays valid.

import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { apiMock } = vi.hoisted(() => ({
  apiMock: {
    createDraft: vi.fn(),
    uploadDraftAttachments: vi.fn()
  }
}));

vi.mock('$lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/api')>();
  return { ...actual, api: { ...actual.api, ...apiMock } };
});

import ComposerDrawer from './ComposerDrawer.svelte';
import { getComposerStore, __resetComposerStore } from '$lib/composer.svelte';
import type { Account } from '$lib/api';

const ACCOUNTS: Account[] = [
  {
    id: 'acc1',
    name: 'Editor',
    username: 'editor@example.com',
    domain: 'example.com',
    smtp_host: 'smtp.example.com',
    smtp_port: 587,
    imap_host: 'imap.example.com',
    imap_port: 993
  }
];

/** Mount the drawer already open in compose mode with a valid To + Subject. */
async function renderCompose() {
  getComposerStore().open('compose', { accountId: 'acc1' });
  render(ComposerDrawer, { accounts: ACCOUNTS });
  await waitFor(() => expect(screen.getByLabelText('To')).toBeInTheDocument());
  await fireEvent.input(screen.getByLabelText('To'), { target: { value: 'buyer@example.com' } });
  await fireEvent.input(screen.getByLabelText('Subject'), { target: { value: 'Hello' } });
}

const sendButton = () => screen.getByRole('button', { name: /^human-only send$/i });

beforeEach(() => {
  __resetComposerStore();
  apiMock.createDraft.mockResolvedValue({
    ok: true,
    status: 'draft',
    draft: { id: 'd-1', account_id: 'acc1', revision: 1 }
  });
  apiMock.uploadDraftAttachments.mockResolvedValue({ draft: { id: 'd-1', revision: 2 } });
});

afterEach(() => {
  __resetComposerStore();
  vi.clearAllMocks();
});

describe('ComposerDrawer names the send action Human-only Send', () => {
  it('explains what the operator is authorizing', async () => {
    await renderCompose();

    const note = document.getElementById('composer-human-send-note');
    expect(note).toBeTruthy();
    expect(note).toHaveTextContent(/your explicit send/i);
    expect(note).toHaveTextContent(/cooldown/i);
    expect(note).toHaveTextContent(/governor/i);
  });
});

describe('ComposerDrawer recipient gating', () => {
  it('enables Send with a valid To and blank Cc/Bcc', async () => {
    await renderCompose();
    expect(sendButton()).toBeEnabled();
  });

  it('blocks Send when Cc is present but malformed', async () => {
    await renderCompose();
    await fireEvent.input(screen.getByLabelText('Cc'), { target: { value: 'broken' } });

    expect(sendButton()).toBeDisabled();
    expect(screen.getByText(/valid cc addresses/i)).toBeInTheDocument();
  });

  it('re-enables Send once a malformed Cc is corrected', async () => {
    await renderCompose();
    const cc = screen.getByLabelText('Cc');

    await fireEvent.input(cc, { target: { value: 'broken' } });
    expect(sendButton()).toBeDisabled();

    await fireEvent.input(cc, { target: { value: 'ops@example.com' } });
    expect(sendButton()).toBeEnabled();
  });

  it('blocks Send when one entry of a Cc list is malformed', async () => {
    await renderCompose();
    await fireEvent.input(screen.getByLabelText('Cc'), {
      target: { value: 'ops@example.com, broken' }
    });

    expect(sendButton()).toBeDisabled();
  });

  it('blocks Send when Bcc is present but malformed', async () => {
    await renderCompose();
    await fireEvent.click(screen.getByRole('button', { name: /^bcc$/i }));
    await fireEvent.input(screen.getByLabelText('Bcc'), { target: { value: 'nope@' } });

    expect(sendButton()).toBeDisabled();
    expect(screen.getByText(/valid bcc addresses/i)).toBeInTheDocument();
  });

  it('keeps Send enabled when Bcc is revealed but left blank', async () => {
    await renderCompose();
    await fireEvent.click(screen.getByRole('button', { name: /^bcc$/i }));

    expect(screen.getByLabelText('Bcc')).toBeInTheDocument();
    expect(sendButton()).toBeEnabled();
  });
});

// ── Save draft ────────────────────────────────────────────────────────
// A fresh message can be kept as a local draft without sending. Files go up
// through the draft attachment route so they get its size and threat checks.

describe('ComposerDrawer Save draft', () => {
  const saveButton = () => screen.getByRole('button', { name: /^save draft$/i });

  it('saves the typed message as a draft and closes', async () => {
    const onsaved = vi.fn();
    getComposerStore().open('compose', { accountId: 'acc1' });
    render(ComposerDrawer, { accounts: ACCOUNTS, onsaved });
    await waitFor(() => expect(screen.getByLabelText('To')).toBeInTheDocument());
    await fireEvent.input(screen.getByLabelText('To'), { target: { value: 'buyer@example.com' } });
    await fireEvent.input(screen.getByLabelText('Subject'), { target: { value: 'Hello' } });
    await fireEvent.input(screen.getByLabelText('Message'), { target: { value: 'Half done' } });

    await fireEvent.click(saveButton());

    await waitFor(() => expect(getComposerStore().isOpen).toBe(false));
    expect(apiMock.createDraft).toHaveBeenCalledWith('acc1', {
      to: 'buyer@example.com',
      subject: 'Hello',
      text: 'Half done',
      html: null,
      cc: null,
      bcc: null
    });
    expect(apiMock.uploadDraftAttachments).not.toHaveBeenCalled();
    expect(onsaved).toHaveBeenCalledWith('acc1', 'd-1');
  });

  it('saves without a recipient', async () => {
    getComposerStore().open('compose', { accountId: 'acc1' });
    render(ComposerDrawer, { accounts: ACCOUNTS });
    await waitFor(() => expect(screen.getByLabelText('Subject')).toBeInTheDocument());
    await fireEvent.input(screen.getByLabelText('Subject'), { target: { value: 'Idea' } });
    expect(sendButton()).toBeDisabled();
    expect(saveButton()).toBeEnabled();
  });

  it('uploads attachments against the saved revision', async () => {
    await renderCompose();
    const input = document.getElementById('composer-attachments') as HTMLInputElement;
    const file = new File(['hello'], 'note.txt', { type: 'text/plain' });
    Object.defineProperty(input, 'files', { value: [file], configurable: true });
    await fireEvent.change(input);
    await waitFor(() => expect(screen.getByText('note.txt')).toBeInTheDocument());

    await fireEvent.click(saveButton());

    await waitFor(() => expect(apiMock.uploadDraftAttachments).toHaveBeenCalledTimes(1));
    const [accountId, draftId, body] = apiMock.uploadDraftAttachments.mock.calls[0];
    expect([accountId, draftId]).toEqual(['acc1', 'd-1']);
    expect(body.expected_revision).toBe(1);
    expect(body.attachments[0]).toMatchObject({ filename: 'note.txt', content_type: 'text/plain' });
  });

  it('keeps the composer open and says why when the save fails', async () => {
    const { EnvelopeApiError } = await import('$lib/api');
    apiMock.createDraft.mockRejectedValueOnce(new EnvelopeApiError(500, 'db_error', 'disk full', null));
    await renderCompose();
    await fireEvent.click(saveButton());
    expect(await screen.findByRole('alert')).toHaveTextContent(/disk full/);
    expect(getComposerStore().isOpen).toBe(true);
  });

  it('is offered in the close prompt', async () => {
    await renderCompose();
    await fireEvent.keyDown(window, { key: 'Escape' });
    const buttons = await screen.findAllByRole('button', { name: /^save draft$/i });
    await fireEvent.click(buttons[buttons.length - 1]);
    await waitFor(() => expect(apiMock.createDraft).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(getComposerStore().isOpen).toBe(false));
  });

  it('is not offered on replies', async () => {
    getComposerStore().open('reply', { accountId: 'acc1', parentUid: 7, parentFolder: 'INBOX' });
    render(ComposerDrawer, { accounts: ACCOUNTS });
    await waitFor(() => expect(screen.getByLabelText('Message')).toBeInTheDocument());
    expect(screen.queryByRole('button', { name: /^save draft$/i })).toBeNull();
  });
});

// ── Discard protection ────────────────────────────────────────────────
// Esc / × / backdrop on a composer with content must not silently throw the
// draft away without asking.

describe('ComposerDrawer discard protection', () => {
  it('closes immediately when nothing has been typed', async () => {
    getComposerStore().open('compose', { accountId: 'acc1' });
    render(ComposerDrawer, { accounts: ACCOUNTS });
    await waitFor(() => expect(screen.getByLabelText('To')).toBeInTheDocument());
    await fireEvent.keyDown(window, { key: 'Escape' });
    expect(getComposerStore().isOpen).toBe(false);
  });

  it('keeps the draft and asks first when content is present', async () => {
    await renderCompose();
    await fireEvent.keyDown(window, { key: 'Escape' });
    expect(getComposerStore().isOpen).toBe(true);
    expect(await screen.findByRole('button', { name: 'Keep editing' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Discard draft' })).toBeInTheDocument();
    // Content survived the Escape.
    expect((screen.getByLabelText('Subject') as HTMLInputElement).value).toBe('Hello');
  });

  it('Keep editing returns to the draft; Discard draft closes it', async () => {
    await renderCompose();
    await fireEvent.keyDown(window, { key: 'Escape' });
    await fireEvent.click(await screen.findByRole('button', { name: 'Keep editing' }));
    expect(getComposerStore().isOpen).toBe(true);
    expect((screen.getByLabelText('Subject') as HTMLInputElement).value).toBe('Hello');
    await fireEvent.keyDown(window, { key: 'Escape' });
    await fireEvent.click(await screen.findByRole('button', { name: 'Discard draft' }));
    expect(getComposerStore().isOpen).toBe(false);
  });
});
