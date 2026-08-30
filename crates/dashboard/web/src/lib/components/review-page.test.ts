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
      drafts: {
        counts: { pending_review: 0, blocked: 0 },
        returned: 0,
        truncated: false,
        items: []
      },
      failed_actions: { count: 0, returned: 0, truncated: false, items: [] },
      events: { count: 0, returned: 0, truncated: false, items: [] },
      proposed_rules: { count: 0, items: [] }
    },
    waiting: {
      count: 0,
      scheduled: { count: 0, due: 0, returned: 0, truncated: false, items: [] },
      due_snoozes: { count: 0, items: [] },
      awaiting_reply: { count: 0, items: [] }
    },
    needs_triage: { count: 0, returned: 0, truncated: false, source: 'durable_events', items: [] },
    operational_health: {
      count: 0,
      failed_auth: { count: 0, returned: 0, truncated: false, items: [] },
      failed_watches: { count: 0, returned: 0, truncated: false, items: [] },
      dead_letters: { count: 0, routes: [] }
    },
    sent_history: {
      source: 'observed_thread_history',
      coverage:
        'Observed thread history from locally scanned folders; not a complete mailbox census.',
      count: 0,
      returned: 0,
      truncated: false,
      items: []
    },
    generated_at: '2026-05-09T09:00:00'
  };
}

function populatedResponse(): ReviewResponse {
  const base = emptyResponse();
  base.summary = { decide_now: 4, waiting: 2, needs_triage: 1, operational_health: 2 };
  base.decide_now = {
    count: 4,
    drafts: {
      counts: { pending_review: 1, blocked: 1 },
      returned: 2,
      truncated: false,
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
      count: 1,
      returned: 1,
      truncated: false,
      items: [
        {
          id: 'fa-act-1',
          account_id: 'acc1',
          account_label: 'Work',
          action_type: 'send',
          action_status: 'failed',
          draft_id: 'd9',
          draft_link: '/accounts/acc1/drafts/d9',
          created_at: '2026-05-09T08:45:00'
        }
      ]
    },
    events: { count: 0, returned: 0, truncated: false, items: [] },
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
      returned: 1,
      truncated: false,
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
    returned: 1,
    truncated: false,
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
        folder: 'INBOX',
        uid: 101,
        message_link: '/mail/unified/acc1/101?folder=INBOX',
        secure_pending: false,
        created_at: '2026-05-09T08:58:00'
      }
    ]
  };
  base.operational_health = {
    count: 2,
    failed_auth: {
      count: 1,
      returned: 1,
      truncated: false,
      items: [
        {
          id: 'fa1',
          account_id: 'acc1',
          account_label: 'Work',
          backend: 'imap',
          status: 'auth_failed',
          created_at: '2026-05-09T08:00:00'
        }
      ]
    },
    failed_watches: {
      count: 1,
      returned: 1,
      truncated: false,
      items: [
        {
          id: 'w1',
          account_id: 'acc1',
          account_label: 'Work',
          folder: 'Archive',
          status: 'failed',
          last_heartbeat_at: '2026-05-09T08:00:00'
        }
      ]
    },
    dead_letters: { count: 0, routes: [] }
  };
  base.sent_history = {
    source: 'observed_thread_history',
    coverage:
      'Observed thread history from locally scanned folders; not a complete mailbox census.',
    count: 1,
    returned: 1,
    truncated: false,
    items: [
      {
        counterparty: 'plans@tripit.com',
        account_id: 'acc1',
        account_label: 'Work',
        message_count: 384,
        outbound_count: 382,
        inbound_count: 2,
        thread_count: 332,
        first_observed: '2024-05-01T00:00:00',
        last_observed: '2025-11-15T09:30:00',
        signal: 'historical_one_way',
        link: null,
        link_state: 'not_available'
      }
    ]
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
      'Operational health',
      'Sent relationship history'
    ]);
    // Counts ride next to their headings.
    expect(container.querySelector('#review-decide-now .group-count')?.textContent).toBe('4');
    expect(container.querySelector('#review-waiting .group-count')?.textContent).toBe('2');
    expect(container.querySelector('#review-needs-triage .group-count')?.textContent).toBe('1');
    expect(container.querySelector('#review-operational-health .group-count')?.textContent).toBe(
      '2'
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

  it('never renders a message-body snippet, even from a stale server payload', async () => {
    // The API contract no longer carries `snippet`; an older server might
    // still send it. The page must drop it, not render it.
    const stale = populatedResponse();
    (stale.needs_triage.items[0] as unknown as Record<string, unknown>)['snippet'] =
      'snippet-private-body';
    reviewMock.get.mockResolvedValue(stale);
    const { container } = render(ReviewPage);

    await waitFor(() => expect(screen.getByText('Invoice due')).toBeInTheDocument());
    expect(container.textContent).not.toContain('snippet-private-body');
  });

  it('never renders server free-text fields, even from a stale server payload', async () => {
    // The contract carries structured labels only. An older server might
    // still send the removed free-text columns; none may reach the DOM.
    const stale = populatedResponse();
    const patch = (item: unknown, fields: Record<string, string>) =>
      Object.assign(item as Record<string, unknown>, fields);
    patch(stale.decide_now.failed_actions.items[0], {
      justification: 'justification-private-material'
    });
    patch(stale.waiting.due_snoozes.items[0], {
      reason: 'reason-private-material',
      note: 'note-private-material'
    });
    patch(stale.operational_health.failed_auth.items[0], {
      reason: 'auth-reason-private-material',
      retry_guidance: 'guidance-private-material'
    });
    patch(stale.operational_health.failed_watches.items[0], {
      failure_reason: 'watch-failure-private-material'
    });
    reviewMock.get.mockResolvedValue(stale);
    const { container } = render(ReviewPage);

    await waitFor(() => expect(screen.getByText('Due follow-up')).toBeInTheDocument());
    expect(container.textContent).not.toContain('-private-material');
  });

  it('renders sent relationship history as context with observed counts, never as a task', async () => {
    const { container } = render(ReviewPage);
    await waitFor(() => expect(screen.getByText('plans@tripit.com')).toBeInTheDocument());

    const section = container.querySelector('#review-sent-history');
    expect(section).not.toBeNull();
    // Provenance disclosure: observed history, not a census, context only.
    expect(section!.textContent).toContain('not a complete mailbox census');
    expect(section!.textContent).toContain('Context only');
    // Observed aggregate facts, verbatim (template line breaks collapsed).
    expect(section!.textContent!.replace(/\s+/g, ' ')).toContain(
      '382 sent · 2 received · 332 threads'
    );
    // The truthful signal chip — and no task/obligation language anywhere.
    expect(screen.getByText('one-way · historical')).toBeInTheDocument();
    expect(section!.textContent!.toLowerCase()).not.toContain('awaiting');
    expect(section!.textContent!.toLowerCase()).not.toContain('reply');
    // No canonical relationship surface exists: the row must not be a link.
    expect(section!.querySelector('a')).toBeNull();
  });

  it('orders sent relationship history last, after the operational groups', async () => {
    const { container } = render(ReviewPage);
    await waitFor(() => expect(screen.getByText('plans@tripit.com')).toBeInTheDocument());

    const sections = Array.from(container.querySelectorAll('section.group')).map((s) => s.id);
    expect(sections[sections.length - 1]).toBe('review-sent-history');
  });

  it('says showing-N-of-M when sent history is capped', async () => {
    const capped = populatedResponse();
    capped.sent_history.count = 40;
    capped.sent_history.returned = 1;
    capped.sent_history.truncated = true;
    reviewMock.get.mockResolvedValue(capped);
    const { container } = render(ReviewPage);

    await waitFor(() => expect(screen.getByText('plans@tripit.com')).toBeInTheDocument());
    const section = container.querySelector('#review-sent-history');
    expect(section!.textContent).toMatch(/Showing 1 of\s+40\./);
    expect(section!.querySelector('.group-count')?.textContent).toBe('40');
  });

  it('never renders thread subjects or snippets from a stale sent-history payload', async () => {
    const stale = populatedResponse();
    Object.assign(stale.sent_history.items[0] as unknown as Record<string, unknown>, {
      subject: 'thread-subject-private-material',
      snippet: 'thread-snippet-private-body'
    });
    reviewMock.get.mockResolvedValue(stale);
    const { container } = render(ReviewPage);

    await waitFor(() => expect(screen.getByText('plans@tripit.com')).toBeInTheDocument());
    expect(container.textContent).not.toContain('-private-');
  });

  it('shows a truthful sent-history empty state', async () => {
    reviewMock.get.mockResolvedValue(emptyResponse());
    render(ReviewPage);

    await waitFor(() =>
      expect(screen.getByText('No sent relationship history')).toBeInTheDocument()
    );
  });

  it('says showing-N-of-M when a capped source is truncated', async () => {
    const truncated = populatedResponse();
    truncated.decide_now.failed_actions.count = 30;
    truncated.decide_now.failed_actions.returned = 1;
    truncated.decide_now.failed_actions.truncated = true;
    truncated.summary.decide_now = 33;
    truncated.decide_now.count = 33;
    reviewMock.get.mockResolvedValue(truncated);
    const { container } = render(ReviewPage);

    await waitFor(() => expect(screen.getByText('Approve outreach')).toBeInTheDocument());
    // The heading carries the true total, the disclosure the honest cap, and
    // the group badge never reports an item-list length as the whole queue.
    expect(screen.getByText('Failed agent actions · 30')).toBeInTheDocument();
    expect(container.textContent).toMatch(/Showing 1 of\s+30\./);
    expect(container.querySelector('#review-decide-now .group-count')?.textContent).toBe('33');
  });
});
