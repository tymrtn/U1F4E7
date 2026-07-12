import { afterEach, describe, expect, it, vi } from 'vitest';
import { EnvelopeApiError, request, resetCsrf } from './api';

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
});
