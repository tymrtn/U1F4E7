import { render, screen, fireEvent, waitFor, within } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { apiMock } = vi.hoisted(() => ({
  apiMock: {
    mailEngineDecisions: vi.fn(),
    correctMailEngineDecision: vi.fn()
  }
}));

vi.mock('$lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/api')>();
  return { ...actual, api: { ...actual.api, ...apiMock } };
});

import DigestPage from '../../routes/digest/+page.svelte';

function decision(over: Record<string, unknown>) {
  return {
    account_id: 'acc-1',
    folder: 'INBOX',
    uidvalidity: 10,
    uid: 41,
    model_status: 'decided',
    status: 'decided',
    model_route: 'follow_up',
    route: 'follow_up',
    route_probability: 0.95,
    route_confidence: 0.94,
    urgency: 'not_urgent',
    model_urgency: 'not_urgent',
    correction_revision: 0,
    execution_status: 'not_requested',
    executed_action: null,
    model_error_code: null,
    error_code: null,
    decided_at: '2026-09-19T12:00:00Z',
    message_link: '/mail/unified/acc-1/41?folder=INBOX',
    metadata_state: 'available',
    trust: {
      schema: 'envelope.inbound-trust.v1',
      instructions_authoritative: false
    },
    untrusted_content: {
      from: 'Maria Keller',
      subject: 'Renewal terms',
      date: '2026-09-19T12:00:00Z'
    },
    ...over
  };
}

const RESPONSE = {
  state: 'available',
  returned: 4,
  limit: 200,
  pending_digest: 1,
  urgent_notification: {
    state: 'not_configured',
    matching_routes: 0,
    delivery_requires_engine_deliver: true
  },
  status: [],
  items: [
    decision({ uid: 44, route: 'important', urgency: 'urgent', untrusted_content: { from: 'Ops', subject: 'Service interruption', date: null } }),
    decision({ uid: 43, route: 'review', status: 'review', error_code: 'decision_interrupted', untrusted_content: { from: null, subject: null, date: null } }),
    decision({ uid: 42, route: 'digest_news', untrusted_content: { from: 'News desk', subject: 'Daily briefing', date: null } }),
    decision({ uid: 41 })
  ]
};

beforeEach(() => {
  apiMock.mailEngineDecisions.mockResolvedValue(RESPONSE);
  apiMock.correctMailEngineDecision.mockResolvedValue({ ok: true, revision: 1 });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('Mail-engine cockpit', () => {
  it('renders persisted decisions in attention order without inventing categories', async () => {
    render(DigestPage);
    await screen.findByText('Service interruption');
    expect(screen.getByRole('heading', { name: 'Urgent now' })).toBeInTheDocument();
    expect(screen.getByRole('heading', { name: 'Needs review' })).toBeInTheDocument();
    expect(screen.getByRole('heading', { name: 'Needs reply' })).toBeInTheDocument();
    expect(screen.getByRole('heading', { name: 'News digest' })).toBeInTheDocument();
    expect(screen.queryByText(/awaiting categorize backend/i)).not.toBeInTheDocument();
  });

  it('shows the real status strip and actionable error code', async () => {
    render(DigestPage);
    await screen.findByText('4');
    expect(screen.getByText('recent decisions')).toBeInTheDocument();
    expect(screen.getByText('digest pending')).toBeInTheDocument();
    expect(screen.getByText(/external alerts are not configured/i)).toBeInTheDocument();
    expect(screen.getByText('decision_interrupted')).toBeInTheDocument();
    expect(screen.getByText(/Use Correct decision below to classify it locally/i)).toBeInTheDocument();
  });

  it('explains the guarded recovery path for a missing OpenRouter key', async () => {
    apiMock.mailEngineDecisions.mockResolvedValueOnce({
      ...RESPONSE,
      returned: 1,
      items: [
        decision({
          model_status: 'review',
          status: 'review',
          model_route: 'review',
          route: 'review',
          model_error_code: 'openrouter_api_key_missing',
          error_code: 'openrouter_api_key_missing'
        })
      ]
    });
    render(DigestPage);
    expect(await screen.findByText('openrouter_api_key_missing')).toBeInTheDocument();
    expect(screen.getByText(/Restore OPENROUTER_API_KEY/i)).toBeInTheDocument();
    expect(screen.getByText(/--confirm-new-jev-call/i)).toBeInTheDocument();
  });

  it('links each row to the canonical message reader', async () => {
    render(DigestPage);
    const link = await screen.findByRole('link', { name: /Renewal terms/i });
    expect(link.getAttribute('href')).toContain('/mail/unified/acc-1/41?folder=INBOX');
  });

  it('refreshes from local mail-engine state', async () => {
    render(DigestPage);
    await screen.findByText('Service interruption');
    await fireEvent.click(screen.getByRole('button', { name: 'Refresh' }));
    await waitFor(() => expect(apiMock.mailEngineDecisions).toHaveBeenCalledTimes(2));
  });

  it('submits a revision-guarded local correction and reloads persisted state', async () => {
    render(DigestPage);
    const subject = await screen.findByText('Renewal terms');
    const row = subject.closest('li')!;
    const button = within(row).getByRole('button', { name: 'Important', hidden: true });
    await fireEvent.click(button);
    await waitFor(() => expect(apiMock.correctMailEngineDecision).toHaveBeenCalledTimes(1));
    const [item, correction] = apiMock.correctMailEngineDecision.mock.calls[0];
    expect(item.uid).toBe(41);
    expect(item.correction_revision).toBe(0);
    expect(correction.route).toBe('important');
    await waitFor(() => expect(apiMock.mailEngineDecisions).toHaveBeenCalledTimes(2));
  });

  it('states first-run baseline behavior instead of showing an empty product', async () => {
    apiMock.mailEngineDecisions.mockResolvedValueOnce({
      ...RESPONSE,
      state: 'not_started',
      returned: 0,
      items: []
    });
    render(DigestPage);
    expect(await screen.findByText('Watching has not started')).toBeInTheDocument();
    expect(screen.getByText(/new-mail-only baseline/i)).toBeInTheDocument();
  });

  it('surfaces a stable error code when local state cannot load', async () => {
    const { EnvelopeApiError } = await import('$lib/api');
    apiMock.mailEngineDecisions.mockRejectedValueOnce(
      new EnvelopeApiError(500, 'mail_engine_status_failed', 'boom', null)
    );
    render(DigestPage);
    await waitFor(() => expect(screen.getByRole('alert')).toBeInTheDocument());
    expect(screen.getByText('mail_engine_status_failed')).toBeInTheDocument();
  });
});
