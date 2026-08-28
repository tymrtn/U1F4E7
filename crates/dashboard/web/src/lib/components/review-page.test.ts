// Review page (`/review`) — the operator's daily decision queue.
//
// Pins the load-bearing product contract:
//   * group order: Decide now → Waiting → Needs triage → Operational health
//   * headings carry live counts; every item deep-links to its acting surface
//   * proposals are labeled as proposals, never as live automation
//   * empty states are truthful — Needs triage explicitly says the queue only
//     shows durable events and never classifies the inbox.
import { render, screen, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { reviewMock } = vi.hoisted(() => ({ reviewMock: { get: vi.fn() } }));
vi.mock('$lib/review-api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/review-api')>();
  return { ...actual, reviewApi: { ...actual.reviewApi, ...reviewMock } };
});

import ReviewPage from '../../routes/review/+page.svelte';
import type { ReviewResponse } from '$lib/review-api';

function emptyResponse(): ReviewResponse {
  return {
    summary: { decide_now: 0, waiting: 0, needs_triage: 0, operational_health: 0 },
    decide_now: {
      count: 0,
      drafts: { counts: { pending_review: 0, blocked: 0 }, items: [] },
      failed_actions: { count: 0, items: [] },
      events: { count: 0, items: [] },
      proposed_rules: { count: 0, items: [] }
    },
    waiting: {
      count: 0,
      scheduled: { count: 0, due: 0, items: [] },
      due_snoozes: { count: 0, items: [] },
      awaiting_reply: { count: 0, items: [] }
    },
    needs_triage: { count: 0, source: 'durable_events', items: [] },
    operational_health: {
      count: 0,
      failed_auth: { count: 0, items: [] },
      failed_watches: { count: 0, items: [] },
      dead_letters: { count: 0, routes: [] }
    },
    generated_at: '2026-05-09T09:00:00'
  };
}

function populatedResponse(): ReviewResponse {
  const base = emptyResponse();
  base.summary = { decide_now: 3, waiting: 2, needs_triage: 1, operational_health: 1 };
  base.decide_now = {
    count: 3,
    drafts: {
      counts: { pending_review: 1, blocked: 1 },
      items: [
        {
          id: 'd1',
          account_id: 'acc1',
          account_label: 'Work',
          subject: 'Approve outreach',
          status: 'pending_review',
          created_by: 'mcp',
          created_at: '2026-05-09T08:00:00',
          updated_at: '2026-05-09T08:30:00',
          send_after: null,
          revision: 1,
          link: '/accounts/acc1/drafts/d1',
          action_base: '/api/accounts/acc1/drafts/d1'
        },
        {
          id: 'd2',
          account_id: 'acc2',
          account_label: 'Personal',
          subject: 'Blocked reply',
          status: 'blocked',
          created_by: 'agent',
          created_at: '2026-05-09T07:00:00',
          updated_at: '2026-05-09T07:30:00',
          send_after: null,
          revision: 1,
          link: '/accounts/acc2/drafts/d2',
          action_base: '/api/accounts/acc2/drafts/d2'
        }
      ]
    },
    failed_actions: {
      count: 0,
      items: []
    },
    events: { count: 0, items: [] },
    proposed_rules: {
      count: 1,
      items: [
        {
          id: 'r1',
          account_id: 'acc1',
          account_label: 'Work',
          name: 'Junk sweep',
          action: { move: 'Junk' },
          review_state: 'proposed_disabled',
          live: false,
          priority: 10,
          created_at: '2026-05-08T00:00:00',
          updated_at: '2026-05-08T00:00:00',
          link: '/rules'
        }
      ]
    }
  };
  base.waiting = {
    count: 2,
    scheduled: {
      count: 1,
      due: 0,
      items: [
        {
          id: 'd3',
          account_id: 'acc1',
          account_label: 'Work',
          subject: 'Queued hello',
          created_by: 'agent',
          send_after: '2026-05-09T09:01:00',
          due: false,
          seconds_remaining: 60,
          link: '/accounts/acc1/drafts/d3',
          action_base: '/api/accounts/acc1/drafts/d3'
        }
      ]
    },
    due_snoozes: {
      count: 1,
      items: [
        {
          id: 's1',
          account_id: 'acc1',
          account_label: 'Work',
          subject: 'Due follow-up',
          return_at: '2026-05-09T08:30:00',
          reason: 'review',
          note: null,
          folder: 'Snoozed',
          uid: 42,
          message_link: '/mail/unified/acc1/42?folder=Snoozed',
          due: true
        }
      ]
    },
    awaiting_reply: { count: 0, items: [] }
  };
  base.needs_triage = {
    count: 1,
    source: 'durable_events',
    items: [
      {
        id: 'e1',
        account_id: 'acc1',
        account_label: 'Work',
        event_type: 'watch.message_matched',
        outcome: 'recorded',
        from_addr: 'sender@example.com',
        subject: 'Invoice due',
        snippet: 'Please pay',
        folder: 'INBOX',
        uid: 101,
        message_link: '/mail/unified/acc1/101?folder=INBOX',
        secure_pending: false,
        created_at: '2026-05-09T08:58:00'
      }
    ]
  };
  base.operational_health = {
    count: 1,
    failed_auth: {
      count: 1,
      items: [
        {
          id: 'fa1',
          account_id: 'acc1',
          account_label: 'Work',
          backend: 'imap',
          reason: 'LOGIN failed',
          retry_guidance: 'Create an app password and retry verification.',
          created_at: '2026-05-09T08:00:00'
        }
      ]
    },
    failed_watches: { count: 0, items: [] },
    dead_letters: { count: 0, routes: [] }
  };
  return base;
}

beforeEach(() => {
  reviewMock.get.mockResolvedValue(populatedResponse());
});
afterEach(() => vi.clearAllMocks());

describe('Review page groups', () => {
  it('renders the four groups in decision-priority order with counts', async () => {
    const { container } = render(ReviewPage);
    await waitFor(() => expect(screen.getByText('Approve outreach')).toBeInTheDocument());

    const headings = Array.from(container.querySelectorAll('h2')).map((h) =>
      h.textContent?.trim()
    );
    expect(headings).toEqual([
      'Decide now',
      'Waiting',
      'Needs triage',
      'Operational health'
    ]);
    // Counts ride next to their headings.
    expect(container.querySelector('#review-decide-now .group-count')?.textContent).toBe('3');
    expect(container.querySelector('#review-waiting .group-count')?.textContent).toBe('2');
    expect(container.querySelector('#review-needs-triage .group-count')?.textContent).toBe('1');
    expect(container.querySelector('#review-operational-health .group-count')?.textContent).toBe(
      '1'
    );
  });

  it('deep-links drafts, snoozes, and triage items to their canonical surfaces', async () => {
    render(ReviewPage);
    await waitFor(() => expect(screen.getByText('Approve outreach')).toBeInTheDocument());

    // base is '/v2' in the test stub.
    expect(screen.getByRole('link', { name: /Approve outreach/ })).toHaveAttribute(
      'href',
      '/v2/accounts/acc1/drafts/d1'
    );
    expect(screen.getByRole('link', { name: /Due follow-up/ })).toHaveAttribute(
      'href',
      '/v2/mail/unified/acc1/42?folder=Snoozed'
    );
    expect(screen.getByRole('link', { name: /Invoice due/ })).toHaveAttribute(
      'href',
      '/v2/mail/unified/acc1/101?folder=INBOX'
    );
    // Draft rows state their truthful current status.
    expect(screen.getByText('pending review')).toBeInTheDocument();
    expect(screen.getByText('blocked')).toBeInTheDocument();
    // Scheduled rows carry countdown facts.
    expect(screen.getByText('in 1m')).toBeInTheDocument();
  });

  it('labels disabled rules as proposals, never live automation', async () => {
    render(ReviewPage);
    await waitFor(() => expect(screen.getByText('Junk sweep')).toBeInTheDocument());

    expect(screen.getByText('proposal · disabled')).toBeInTheDocument();
    expect(screen.getByRole('link', { name: /Junk sweep/ })).toHaveAttribute(
      'href',
      '/v2/rules'
    );
  });

  it('shows truthful empty states, including the no-classifier disclosure', async () => {
    reviewMock.get.mockResolvedValue(emptyResponse());
    render(ReviewPage);
    await waitFor(() => expect(screen.getByText('Nothing to decide')).toBeInTheDocument());

    expect(screen.getByText('Nothing waiting')).toBeInTheDocument();
    expect(screen.getByText('No flagged messages')).toBeInTheDocument();
    expect(
      screen.getByText(/Envelope does not scan or classify your inbox/)
    ).toBeInTheDocument();
    expect(screen.getByText('All clear')).toBeInTheDocument();
  });

  it('surfaces a load error instead of pretending the queue is empty', async () => {
    reviewMock.get.mockRejectedValue(new Error('backend unreachable'));
    render(ReviewPage);

    await waitFor(() => expect(screen.getByText('Review unavailable')).toBeInTheDocument());
    expect(screen.getByText(/backend unreachable/)).toBeInTheDocument();
    expect(screen.queryByText('Nothing to decide')).not.toBeInTheDocument();
  });
});
