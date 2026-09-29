// Durable receipts (#170): a message that leaves the list leaves a receipt
// naming where it went, with an exact way back when the server gave one.
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { moveMock } = vi.hoisted(() => ({ moveMock: vi.fn() }));

vi.mock('$lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/api')>();
  return { ...actual, api: { ...actual.api, messageMove: moveMock } };
});

import ActionReceipts from './ActionReceipts.svelte';
import { EnvelopeApiError } from '$lib/api';
import { getMessageActions, __resetMessageActions } from '$lib/message-actions.svelte';
import { __resetMailboxOpsStore } from '$lib/mailbox-ops.svelte';

const T = { accountId: 'acct-1', folder: 'INBOX', uid: 12, uidvalidity: 5, messageId: '<j@x>', subject: 'Win a prize' };

beforeEach(() => {
  __resetMessageActions();
  __resetMailboxOpsStore();
});
afterEach(() => vi.clearAllMocks());

describe('ActionReceipts', () => {
  it('shows where a junked message went and moves it back exactly', async () => {
    moveMock.mockResolvedValueOnce({
      ok: true, uid: 12, from_folder: 'INBOX', moved_to: 'Junk', moved_uid: 88, moved_uidvalidity: 3
    });
    render(ActionReceipts);
    await getMessageActions().dispatch(T, { kind: 'junk' });
    expect(await screen.findByText(/Moved to Junk \(Junk\)/)).toBeInTheDocument();

    moveMock.mockResolvedValueOnce({
      ok: true, uid: 88, from_folder: 'Junk', moved_to: 'INBOX', moved_uid: 13, moved_uidvalidity: 5
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Move back' }));
    await waitFor(() => expect(moveMock).toHaveBeenCalledTimes(2));
    expect(moveMock.mock.calls[1]).toEqual([
      'acct-1',
      88,
      { folder: 'Junk', to_folder: 'INBOX', uidvalidity: 3, message_id: '<j@x>' }
    ]);
    await waitFor(() => expect(screen.queryByText(/Moved to Junk/)).toBeNull());
  });

  it('keeps the receipt and shows the error when the way back fails', async () => {
    moveMock.mockResolvedValueOnce({
      ok: true, uid: 12, from_folder: 'INBOX', moved_to: 'Junk', moved_uid: 88, moved_uidvalidity: 3
    });
    render(ActionReceipts);
    await getMessageActions().dispatch(T, { kind: 'junk' });
    moveMock.mockRejectedValueOnce(new EnvelopeApiError(409, 'message_not_found', 'gone from Junk', null));
    await fireEvent.click(await screen.findByRole('button', { name: 'Move back' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('gone from Junk');
    expect(screen.getByText(/Moved to Junk/)).toBeInTheDocument();
  });

  it('is dismissible and stays until dismissed', async () => {
    moveMock.mockResolvedValueOnce({
      ok: true, uid: 12, from_folder: 'INBOX', moved_to: 'Archive', moved_uid: null, moved_uidvalidity: null
    });
    render(ActionReceipts);
    await getMessageActions().dispatch(T, { kind: 'archive' });
    expect(await screen.findByText(/Archived/)).toBeInTheDocument();
    // No exact handle: no fake way back.
    expect(screen.queryByRole('button', { name: 'Move back' })).toBeNull();
    await fireEvent.click(screen.getByRole('button', { name: 'Dismiss' }));
    expect(screen.queryByText(/Archived/)).toBeNull();
  });
});
