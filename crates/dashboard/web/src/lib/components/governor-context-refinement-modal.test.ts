import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const { apiMock } = vi.hoisted(() => ({
  apiMock: {
    contextRefinement: vi.fn(),
    retryContextRefinement: vi.fn()
  }
}));

vi.mock('$lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/api')>();
  return { ...actual, api: { ...actual.api, ...apiMock } };
});

import { EnvelopeApiError } from '$lib/api';
import GovernorContextRefinementModal from './GovernorContextRefinementModal.svelte';

const projection = {
  eligible: true,
  revision: 12,
  action: 'send' as const,
  protocol: 'envelope.attribution.v1',
  catalog: 'envelope',
  catalog_version: 1,
  reason_code: 'attributes_required',
  explanation: 'Confirm only facts that are true of this exact draft.',
  attributes: [
    {
      key: 'informational',
      category: 'purpose',
      description: 'Primarily informational',
      provenance: 'declarable' as const,
      state: 'not_selected' as const,
      selectable: true,
      read_only: false,
      explanation: null
    },
    {
      key: 'single_recipient',
      category: 'delivery',
      description: 'One recipient',
      provenance: 'host_derived' as const,
      state: 'observed' as const,
      selectable: false,
      read_only: true,
      explanation: null
    },
    {
      key: 'tyler_approved',
      category: 'authority',
      description: 'Human approval',
      provenance: 'requires_attestation' as const,
      state: 'unavailable' as const,
      selectable: false,
      read_only: true,
      explanation: 'Authority facts cannot be asserted in this modal.'
    }
  ]
};

function mount(onsuccess = vi.fn(), onclose = vi.fn()) {
  return {
    onsuccess,
    onclose,
    ...render(GovernorContextRefinementModal, {
      open: true,
      accountId: 'acct-a',
      draftId: 'draft-b',
      onsuccess,
      onclose
    })
  };
}

beforeEach(() => {
  vi.clearAllMocks();
  apiMock.contextRefinement.mockResolvedValue(projection);
  apiMock.retryContextRefinement.mockResolvedValue({
    draft_id: 'draft-b',
    revision: 12,
    status: 'governed_retry_queued',
    send_after: '2026-09-01T12:01:00Z',
    message: 'Corrected context recorded. Governed retry queued.'
  });
});

describe('GovernorContextRefinementModal', () => {
  it('makes only declarable facts selectable and keeps host/authority facts read-only', async () => {
    mount();

    await screen.findByText('Facts you can correct');
    expect(screen.getByText('Observed by Envelope')).toBeInTheDocument();
    expect(screen.getByText('Authority facts')).toBeInTheDocument();
    expect(screen.getByText('observed')).toBeInTheDocument();
    expect(screen.getByText('Unavailable')).toBeInTheDocument();
    expect(screen.getAllByRole('checkbox')).toHaveLength(2);
    expect(screen.getByRole('button', { name: 'Retry with corrected context' })).toBeDisabled();
  });

  it('submits only the exact revision, replacement keys, and factual confirmation', async () => {
    const { onsuccess } = mount();
    const factual = await screen.findByRole('checkbox', { name: /primarily informational/i });
    await fireEvent.click(factual);
    await fireEvent.click(
      screen.getByRole('checkbox', { name: /confirm these factual labels are accurate/i })
    );
    await fireEvent.click(screen.getByRole('button', { name: 'Retry with corrected context' }));

    await waitFor(() =>
      expect(apiMock.retryContextRefinement).toHaveBeenCalledWith('acct-a', 'draft-b', {
        expected_revision: 12,
        declarable_attributes: ['informational'],
        confirm_factual_accuracy: true
      })
    );
    expect(onsuccess).toHaveBeenCalledWith(
      expect.objectContaining({ status: 'governed_retry_queued' })
    );
  });

  it('does not automatically retry a revision conflict and requires review again', async () => {
    apiMock.retryContextRefinement.mockRejectedValue(
      new EnvelopeApiError(409, 'draft_modified', 'draft changed', null)
    );
    mount();
    await fireEvent.click(
      await screen.findByRole('checkbox', { name: /primarily informational/i })
    );
    await fireEvent.click(
      screen.getByRole('checkbox', { name: /confirm these factual labels are accurate/i })
    );
    await fireEvent.click(screen.getByRole('button', { name: 'Retry with corrected context' }));

    await screen.findByText(/reload the draft, and review its context again/i);
    expect(apiMock.retryContextRefinement).toHaveBeenCalledTimes(1);
    expect(screen.getByRole('button', { name: 'Retry with corrected context' })).toBeDisabled();
  });
});
