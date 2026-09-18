// Logs page — the record of what agents and Envelope did, read from the
// `events` table through `GET /api/events`.
//
// Drives the real api client with a stubbed global fetch, matching
// app-shell.test.ts. The page is read-only: rows deep-link to the draft or
// message they concern, filters re-query the server, and "Load more" pages
// with the server's `next_before` cursor.

import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import LogsPage from '../../routes/logs/+page.svelte';
import type { LogsResponse } from '$lib/logs-api';

function jsonResponse(body: unknown, init: { status?: number } = {}): Response {
  const status = init.status ?? 200;
  const payload = JSON.stringify(body);
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => JSON.parse(payload),
    clone() {
      return jsonResponse(body, init);
    }
  } as unknown as Response;
}

const ACCOUNTS = {
  accounts: [
    { id: 'acc-1', name: 'work@example.com', display_name: 'Work', username: 'work@example.com' },
    { id: 'acc-2', name: 'home@example.com', display_name: 'Home', username: 'home@example.com' }
  ]
};

function entry(over: Partial<LogsResponse['entries'][number]> = {}): LogsResponse['entries'][number] {
  const id = over.id ?? 'evt-1';
  return {
    id,
    group_key: `acc-1|send_governor.blocked|${id}|2026-09-17`,
    account_id: 'acc-1',
    account_label: 'Work',
    event_type: 'send_governor.blocked',
    created_at: '2026-09-17T21:18:42Z',
    first_at: '2026-09-17T21:18:42Z',
    repeat_count: 1,
    agent_id: null,
    acked: true,
    draft_id: 'd-1',
    draft_link: '/accounts/acc-1/drafts/d-1',
    draft_subject: 'Plus Ultra inbox placement check',
    surface: 'scheduled',
    decision: 'review',
    block_code: 'governor_blocked',
    block_reason: "governor decision 'review' (state 'review_required') did not permit this send",
    attrs: ['cold_email', 'informational', 'short_body', 'single_recipient', 'unknown_domain'],
    policy_mode: null,
    denial_code: null,
    message_id: null,
    folder: 'policy',
    uid: null,
    message_link: null,
    from_addr: null,
    subject: null,
    ...over
  };
}

function page(entries: LogsResponse['entries'], next_before: string | null = null): LogsResponse {
  return { entries, next_before, limit: 100 };
}

/** Stub fetch; returns the list of `/api/events` URLs requested, in order. */
function stubFetch(pages: LogsResponse[] | { status: number; body: unknown }) {
  const eventsUrls: string[] = [];
  let pageIndex = 0;
  vi.stubGlobal(
    'fetch',
    vi.fn(async (url: RequestInfo | URL) => {
      const u = String(url);
      if (u.includes('/api/events')) {
        eventsUrls.push(u);
        if (!Array.isArray(pages)) return jsonResponse(pages.body, { status: pages.status });
        const body = pages[Math.min(pageIndex, pages.length - 1)];
        pageIndex += 1;
        return jsonResponse(body);
      }
      if (u.includes('/api/accounts')) return jsonResponse(ACCOUNTS);
      if (u.includes('/api/health')) return jsonResponse({ status: 'ok', version: 'test' });
      return jsonResponse({}, { status: 404 });
    })
  );
  return eventsUrls;
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('Logs page', () => {
  it('renders one row per entry with a plain label, the draft link, and the reason', async () => {
    stubFetch([page([entry()])]);
    const { container } = render(LogsPage);

    const row = await waitFor(() => {
      const el = container.querySelector('#logs .log-row');
      expect(el).not.toBeNull();
      return el!;
    });
    expect(row.textContent).toContain('Blocked by Governor');
    expect(row.textContent).toContain('Plus Ultra inbox placement check');
    expect(row.textContent).toContain('Work');
    expect(row.textContent).toContain('scheduled');
    expect(row.textContent).toContain("governor decision 'review'");
    const link = row.querySelector('a[href="/v2/accounts/acc-1/drafts/d-1"]');
    expect(link).not.toBeNull();
    // The five attributes Governor actually scored are shown as chips.
    expect(row.querySelectorAll('.log-attr')).toHaveLength(5);
  });

  it('shows the repeat count when the sweep re-evaluated the same draft', async () => {
    stubFetch([
      page([entry({ repeat_count: 7545, first_at: '2026-06-25T10:07:55Z', created_at: '2026-07-02T16:53:26Z' })])
    ]);
    const { container } = render(LogsPage);

    const repeat = await waitFor(() => {
      const el = container.querySelector('#logs .log-row .log-repeat');
      expect(el).not.toBeNull();
      return el!;
    });
    expect(repeat.textContent).toContain('7,545');
    expect(repeat.textContent).toContain('over 7 days');
  });

  it('labels the other event types without leaking raw type strings', async () => {
    stubFetch([
      page([
        entry({ id: 'e1', event_type: 'send.human_dashboard', decision: null, block_code: null, block_reason: null, attrs: [], surface: 'human:dashboard' }),
        entry({ id: 'e2', event_type: 'draft_approved', decision: null, block_code: null, block_reason: null, attrs: [], surface: null }),
        entry({ id: 'e3', event_type: 'send_completed', decision: null, block_code: null, block_reason: null, attrs: [], surface: null }),
        entry({ id: 'e4', event_type: 'send_governor.allowed', decision: 'allow', block_code: null, block_reason: null, surface: 'cli' }),
        entry({ id: 'e5', event_type: 'new_message', draft_id: null, draft_link: null, draft_subject: null, attrs: [], decision: null, block_code: null, block_reason: null, surface: null, folder: 'INBOX', uid: 33, message_link: '/mail/unified/acc-1/33?folder=INBOX', from_addr: 'alice@example.com', subject: 'Hello' })
      ])
    ]);
    const { container } = render(LogsPage);

    await waitFor(() => expect(container.querySelectorAll('#logs .log-row')).toHaveLength(5));
    const text = container.querySelector('#logs')!.textContent!;
    expect(text).toContain('Human-only Send');
    expect(text).toContain('Approved by human');
    expect(text).toContain('Sent');
    expect(text).toContain('Allowed by Governor');
    expect(text).toContain('New message');
    expect(text).toContain('alice@example.com');
    expect(container.querySelector('a[href="/v2/mail/unified/acc-1/33?folder=INBOX"]')).not.toBeNull();
    expect(text).not.toContain('send.human_dashboard');
    expect(text).not.toContain('send_governor.allowed');
  });

  it('re-queries the server when a filter changes', async () => {
    const urls = stubFetch([page([entry()])]);
    render(LogsPage);

    await waitFor(() => expect(urls.length).toBe(1));
    expect(urls[0]).not.toContain('account=');

    // The account list loads on its own; the option has to exist before it
    // can be picked.
    await screen.findByRole('option', { name: 'Home' });
    await fireEvent.change(screen.getByLabelText('Account'), { target: { value: 'acc-2' } });
    await waitFor(() => expect(urls.length).toBe(2));
    expect(urls[1]).toContain('account=acc-2');

    await fireEvent.change(screen.getByLabelText('Type'), { target: { value: 'send_governor' } });
    await waitFor(() => expect(urls.length).toBe(3));
    expect(urls[2]).toContain('type=send_governor');
    expect(urls[2]).toContain('account=acc-2');
  });

  it('pages with the server cursor on Load more', async () => {
    const urls = stubFetch([
      page([entry({ id: 'e1' })], '2026-09-17T00:00:00Z'),
      page([entry({ id: 'e2', created_at: '2026-09-16T10:00:00Z' })], null)
    ]);
    const { container } = render(LogsPage);

    const more = await screen.findByRole('button', { name: /load more/i });
    await fireEvent.click(more);

    await waitFor(() => expect(container.querySelectorAll('#logs .log-row')).toHaveLength(2));
    expect(urls[1]).toContain('before=2026-09-17T00%3A00%3A00Z');
    // No cursor left: the button goes away instead of fetching nothing forever.
    await waitFor(() => expect(screen.queryByRole('button', { name: /load more/i })).toBeNull());
  });

  it('shows an honest empty state', async () => {
    stubFetch([page([])]);
    render(LogsPage);

    await waitFor(() => expect(screen.getByText(/Nothing logged/)).toBeInTheDocument());
  });

  it('reports a failed load with the error code', async () => {
    stubFetch({ status: 500, body: { error: 'boom', code: 'store_error' } });
    render(LogsPage);

    await waitFor(() => expect(screen.getByText(/store_error/)).toBeInTheDocument());
  });
});
