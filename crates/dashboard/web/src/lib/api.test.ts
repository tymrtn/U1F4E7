import { afterEach, describe, expect, it, vi } from 'vitest';
import { EnvelopeApiError, api, request, resetCsrf } from './api';

type FetchCall = [RequestInfo | URL, RequestInit?];

function fetchCalls(fetchImpl: { mock: { calls: unknown[][] } }): FetchCall[] {
  return fetchImpl.mock.calls as unknown as FetchCall[];
}

/** Build a minimal Response-like object for a mocked fetch. */
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

afterEach(() => {
  resetCsrf();
  vi.restoreAllMocks();
});

describe('request() CSRF handling', () => {
  it('does not prime a CSRF token for GET requests', async () => {
    const fetchImpl = vi.fn(async () => jsonResponse({ accounts: [] }));

    await request('/accounts', { fetchImpl });

    expect(fetchImpl).toHaveBeenCalledTimes(1);
    const [url] = fetchCalls(fetchImpl)[0]!;
    expect(url).toBe('/api/accounts');
    // No /api/csrf call for a read.
    expect(fetchCalls(fetchImpl).some(([u]) => String(u).includes('/api/csrf'))).toBe(false);
  });

  it('primes the token and attaches X-Envelope-CSRF on POST', async () => {
    const fetchImpl = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) {
        return jsonResponse({ token: 'tok-abc' });
      }
      return jsonResponse({ ok: true });
    });

    await request('/messages/unified/refresh', { method: 'POST', fetchImpl });

    const csrfCall = fetchCalls(fetchImpl).find(([u]) => String(u).includes('/api/csrf'));
    expect(csrfCall).toBeTruthy();

    const mutatingCall = fetchCalls(fetchImpl).find(([u]) =>
      String(u).includes('/messages/unified/refresh')
    );
    expect(mutatingCall).toBeTruthy();
    const init = mutatingCall![1]!;
    const headers = init.headers as Record<string, string>;
    expect(headers['X-Envelope-CSRF']).toBe('tok-abc');
    expect(init.method).toBe('POST');
  });

  it('reuses the cached token across mutating calls', async () => {
    const fetchImpl = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok-1' });
      return jsonResponse({ ok: true });
    });

    await request('/a', { method: 'POST', fetchImpl });
    await request('/b', { method: 'POST', fetchImpl });

    const csrfCalls = fetchCalls(fetchImpl).filter(([u]) => String(u).includes('/api/csrf'));
    expect(csrfCalls.length).toBe(1);
  });

  it('retries once on a dashboard_csrf_required 403, re-priming the token', async () => {
    let mintCount = 0;
    let postCount = 0;
    const fetchImpl = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) {
        mintCount += 1;
        return jsonResponse({ token: `tok-${mintCount}` });
      }
      postCount += 1;
      if (postCount === 1) {
        return jsonResponse({ code: 'dashboard_csrf_required' }, { status: 403 });
      }
      return jsonResponse({ ok: true });
    });

    const result = await request<{ ok: boolean }>('/send', { method: 'POST', fetchImpl });

    expect(result.ok).toBe(true);
    // Minted twice: initial prime + re-prime after the 403.
    expect(mintCount).toBe(2);
    // The POST was retried, and the retry carried the fresh token.
    const postCalls = fetchCalls(fetchImpl).filter(([u]) => String(u).includes('/send'));
    expect(postCalls.length).toBe(2);
    const retryHeaders = postCalls[1]![1]!.headers as Record<string, string>;
    expect(retryHeaders['X-Envelope-CSRF']).toBe('tok-2');
  });

  it('does not retry a 403 that is not a CSRF error', async () => {
    const fetchImpl = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      return jsonResponse({ code: 'forbidden_other' }, { status: 403 });
    });

    await expect(request('/x', { method: 'POST', fetchImpl })).rejects.toMatchObject({
      code: 'forbidden_other',
      status: 403
    });
    const postCalls = fetchCalls(fetchImpl).filter(([u]) => String(u).includes('/x'));
    expect(postCalls.length).toBe(1);
  });

  it('surfaces errors as EnvelopeApiError with the stable code field', async () => {
    const fetchImpl = vi.fn(async () =>
      jsonResponse({ code: 'dashboard_auth_required', error: 'unauthorized' }, { status: 401 })
    );

    const err = await request('/accounts', { fetchImpl }).then(
      () => {
        throw new Error('expected request to fail');
      },
      (error: unknown) => error
    );
    expect(err).toBeInstanceOf(EnvelopeApiError);
    if (!(err instanceof EnvelopeApiError)) throw new Error('expected EnvelopeApiError');
    expect(err.code).toBe('dashboard_auth_required');
    expect(err.status).toBe(401);
  });

  it('surfaces a backend reason as the actionable error message', async () => {
    const fetchImpl = vi.fn(async () =>
      jsonResponse(
        {
          code: 'folder_not_resolved',
          reason: 'No provider Trash folder was detected; choose a folder with Move… instead.'
        },
        { status: 422 }
      )
    );

    const err = await request('/accounts/acct/messages/7/move', { fetchImpl }).then(
      () => {
        throw new Error('expected request to fail');
      },
      (error: unknown) => error
    );
    expect(err).toBeInstanceOf(EnvelopeApiError);
    if (!(err instanceof EnvelopeApiError)) throw new Error('expected EnvelopeApiError');
    expect(err.code).toBe('folder_not_resolved');
    expect(err.status).toBe(422);
    expect(err.message).toContain('No provider Trash folder was detected');
  });
});

describe('api.health()', () => {
  it('GETs /api/health and returns the running version', async () => {
    const fetchImpl = vi.fn(async () =>
      jsonResponse({ status: 'ok', service: 'envelope-dashboard', version: '1.0.3' })
    );

    const health = await api.health({ fetchImpl });

    expect(health.version).toBe('1.0.3');
    expect(fetchImpl).toHaveBeenCalledTimes(1);
    const [url, init] = fetchCalls(fetchImpl)[0]!;
    expect(url).toBe('/api/health');
    expect((init?.method ?? 'GET').toUpperCase()).toBe('GET');
  });

  it('propagates a failed health probe as EnvelopeApiError', async () => {
    const fetchImpl = vi.fn(async () => jsonResponse({ code: 'boom' }, { status: 500 }));

    await expect(api.health({ fetchImpl })).rejects.toBeInstanceOf(EnvelopeApiError);
  });
});

describe('api.draftByImapUid()', () => {
  it('GETs the by-imap-uid draft route and returns the local draft', async () => {
    const fetchImpl = vi.fn(async () =>
      jsonResponse({
        draft: { id: 'draft-abc' },
        dashboard_path: '/accounts/acct-a/drafts/draft-abc',
        review_url: 'http://localhost:3141/accounts/acct-a/drafts/draft-abc'
      })
    );

    const res = await api.draftByImapUid('acct a', 38311, { fetchImpl });

    expect(res.draft.id).toBe('draft-abc');
    const [url, init] = fetchCalls(fetchImpl)[0]!;
    expect(url).toBe('/api/accounts/acct%20a/drafts/by-imap-uid/38311');
    expect((init?.method ?? 'GET').toUpperCase()).toBe('GET');
  });

  it('propagates a missing local draft as a 404 EnvelopeApiError', async () => {
    const fetchImpl = vi.fn(async () => jsonResponse({ error: 'draft not found' }, { status: 404 }));

    await expect(api.draftByImapUid('acct-a', 999, { fetchImpl })).rejects.toMatchObject({
      status: 404
    });
  });
});

describe('dashboard context refinement API', () => {
  it('GETs the account-scoped, draft-scoped content-free projection', async () => {
    const fetchImpl = vi.fn(async () =>
      jsonResponse({
        eligible: true,
        revision: 9,
        action: 'send',
        protocol: 'envelope.attribution.v1',
        catalog: 'envelope',
        catalog_version: 1,
        reason_code: 'attributes_required',
        explanation: 'Review factual context.',
        attributes: []
      })
    );

    const result = await api.contextRefinement('acct a', 'draft/b', { fetchImpl });

    expect(result.revision).toBe(9);
    const [url, init] = fetchCalls(fetchImpl)[0]!;
    expect(url).toBe('/api/accounts/acct%20a/drafts/draft%2Fb/context-refinement');
    expect((init?.method ?? 'GET').toUpperCase()).toBe('GET');
  });

  it('POSTs only the revision-bound replacement and factual confirmation with CSRF', async () => {
    const fetchImpl = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'context-token' });
      return jsonResponse({
        draft_id: 'draft-b',
        revision: 9,
        status: 'governed_retry_queued',
        send_after: '2026-09-01T12:01:00Z',
        message: 'Corrected context recorded. Governed retry queued.'
      });
    });

    await api.retryContextRefinement(
      'acct-a',
      'draft-b',
      {
        expected_revision: 9,
        declarable_attributes: ['informational'],
        confirm_factual_accuracy: true
      },
      { fetchImpl }
    );

    const call = fetchCalls(fetchImpl).find(([url]) =>
      String(url).includes('/context-refinement/retry')
    );
    expect(call).toBeTruthy();
    const [url, init] = call!;
    expect(url).toBe('/api/accounts/acct-a/drafts/draft-b/context-refinement/retry');
    expect(init?.method).toBe('POST');
    expect(init?.headers).toMatchObject({ 'X-Envelope-CSRF': 'context-token' });
    expect(JSON.parse(String(init?.body))).toEqual({
      expected_revision: 9,
      declarable_attributes: ['informational'],
      confirm_factual_accuracy: true
    });
  });
});

describe('api.snoozeMessage()', () => {
  it('POSTs /messages/{uid}/snooze with folder + return_at body', async () => {
    const fetchImpl = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      return jsonResponse({ ok: true, uid: 42, return_at: '2026-08-09T09:00:00', snoozed_folder: 'Snoozed' });
    });

    const res = await api.snoozeMessage(
      'acct-a',
      42,
      { folder: 'INBOX', return_at: '2026-08-09T09:00:00', message_id: '<m@x>', subject: 'Hi' },
      { fetchImpl }
    );

    expect(res.snoozed_folder).toBe('Snoozed');
    const call = fetchCalls(fetchImpl).find(([u]) => String(u).includes('/messages/42/snooze'));
    expect(call).toBeTruthy();
    const [url, init] = call!;
    expect(url).toBe('/api/accounts/acct-a/messages/42/snooze');
    expect((init?.method ?? 'GET').toUpperCase()).toBe('POST');
    const body = JSON.parse(String(init?.body));
    expect(body).toMatchObject({
      folder: 'INBOX',
      return_at: '2026-08-09T09:00:00',
      message_id: '<m@x>',
      subject: 'Hi'
    });
  });
});
