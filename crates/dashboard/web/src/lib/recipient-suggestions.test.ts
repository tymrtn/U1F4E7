// Tests for the recipient-suggestion fetcher.
//
// The two behaviours that matter here are invisible in a screenshot and
// expensive in practice: a slow response for an old prefix must never repaint
// the dropdown, and a prefix already answered must render without a request.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import moduleSource from './recipient-suggestions.ts?raw';
import { createSuggester, resetSuggestionCache, SUGGESTION_TTL_MS } from './recipient-suggestions';
import type { AddressSuggestion } from './api';

const ADA: AddressSuggestion = { email: 'ada@example.test', name: 'Ada Lovelace' };
const ADAM: AddressSuggestion = { email: 'adam@example.test', name: null };

/** A fetcher whose responses are resolved by hand, one deferred per call. */
function deferredFetcher() {
  const calls: {
    query: string;
    signal: AbortSignal;
    resolve: (rows: AddressSuggestion[]) => void;
    reject: (e: unknown) => void;
  }[] = [];

  const fetcher = (_account: string, query: string, _limit: number, signal: AbortSignal) =>
    new Promise<AddressSuggestion[]>((resolve, reject) => {
      calls.push({ query, signal, resolve, reject });
    });

  return { fetcher, calls };
}

beforeEach(() => {
  resetSuggestionCache();
});

describe('createSuggester', () => {
  it('returns rows for the current query', async () => {
    const fetcher = vi.fn().mockResolvedValue([ADA]);
    const suggester = createSuggester(fetcher);

    await expect(suggester.search('acc1', 'ada')).resolves.toEqual([ADA]);
    expect(fetcher).toHaveBeenCalledWith('acc1', 'ada', 8, expect.any(AbortSignal));
  });

  it('resolves a superseded search to null so it cannot repaint the dropdown', async () => {
    const { fetcher, calls } = deferredFetcher();
    const suggester = createSuggester(fetcher);

    const first = suggester.search('acc1', 'a');
    const second = suggester.search('acc1', 'ada');

    // The slow first response lands AFTER the newer one was issued.
    calls[1].resolve([ADA]);
    calls[0].resolve([ADAM]);

    expect(await second).toEqual([ADA]);
    expect(await first).toBeNull();
  });

  it('aborts the in-flight request when a newer one starts', async () => {
    const { fetcher, calls } = deferredFetcher();
    const suggester = createSuggester(fetcher);

    void suggester.search('acc1', 'a');
    expect(calls[0].signal.aborted).toBe(false);

    void suggester.search('acc1', 'ad');
    expect(calls[0].signal.aborted).toBe(true);
    expect(calls[1].signal.aborted).toBe(false);
  });

  it('swallows the abort rejection rather than surfacing it as an error', async () => {
    const { fetcher, calls } = deferredFetcher();
    const suggester = createSuggester(fetcher);

    const first = suggester.search('acc1', 'a');
    void suggester.search('acc1', 'ad');
    calls[0].reject(new DOMException('aborted', 'AbortError'));

    await expect(first).resolves.toBeNull();
  });

  it('propagates a real failure for the current query', async () => {
    const fetcher = vi.fn().mockRejectedValue(new Error('offline'));
    const suggester = createSuggester(fetcher);

    await expect(suggester.search('acc1', 'ada')).rejects.toThrow('offline');
  });

  it('answers a repeated prefix from cache without another request', async () => {
    const fetcher = vi.fn().mockResolvedValue([ADA]);
    const suggester = createSuggester(fetcher);

    await suggester.search('acc1', 'ada');
    expect(fetcher).toHaveBeenCalledTimes(1);

    expect(suggester.cached('acc1', 'ada')).toEqual([ADA]);
    await suggester.search('acc1', 'ada');
    expect(fetcher).toHaveBeenCalledTimes(1);
  });

  // The central just-sent-recipient workflow. A send folds its recipients into
  // address history immediately, so the prefix that matched nothing before the
  // send is the prefix that matches the person you just wrote to. Remembering
  // the miss hid them until the tab was reloaded.
  it('re-asks after an empty answer, so a just-sent recipient appears without a reload', async () => {
    const fetcher = vi.fn().mockResolvedValueOnce([]).mockResolvedValueOnce([ADAM]);
    const suggester = createSuggester(fetcher);

    await expect(suggester.search('acc1', 'adam')).resolves.toEqual([]);
    expect(suggester.cached('acc1', 'adam')).toBeUndefined();

    // ...the message is sent here; the backend now knows the address.
    await expect(suggester.search('acc1', 'adam')).resolves.toEqual([ADAM]);
    expect(fetcher).toHaveBeenCalledTimes(2);
  });

  it('caches the answer once it is no longer empty', async () => {
    const fetcher = vi.fn().mockResolvedValueOnce([]).mockResolvedValueOnce([ADAM]);
    const suggester = createSuggester(fetcher);

    await suggester.search('acc1', 'adam');
    await suggester.search('acc1', 'adam');
    expect(suggester.cached('acc1', 'adam')).toEqual([ADAM]);

    await suggester.search('acc1', 'adam');
    expect(fetcher).toHaveBeenCalledTimes(2);
  });

  // Not caching a miss must not cost the stale-response guard: a re-asked
  // prefix is still a sequenced search.
  it('still resolves a superseded re-ask to null', async () => {
    const { fetcher, calls } = deferredFetcher();
    const suggester = createSuggester(fetcher);

    const first = suggester.search('acc1', 'ad');
    const second = suggester.search('acc1', 'ada');

    calls[1].resolve([ADA]);
    calls[0].resolve([]);

    expect(await second).toEqual([ADA]);
    expect(await first).toBeNull();
  });

  it('caches per account so one account never answers for another', async () => {
    const fetcher = vi
      .fn()
      .mockResolvedValueOnce([ADA])
      .mockResolvedValueOnce([ADAM]);
    const suggester = createSuggester(fetcher);

    await suggester.search('acc1', 'ad');
    expect(suggester.cached('acc2', 'ad')).toBeUndefined();
    await expect(suggester.search('acc2', 'ad')).resolves.toEqual([ADAM]);
  });

  it('treats the cache case-insensitively, matching backend ranking', async () => {
    const fetcher = vi.fn().mockResolvedValue([ADA]);
    const suggester = createSuggester(fetcher);

    await suggester.search('acc1', 'Ada');
    expect(suggester.cached('acc1', 'ada')).toEqual([ADA]);
    expect(fetcher).toHaveBeenCalledTimes(1);
  });

  it('cancel() aborts and invalidates whatever is in flight', async () => {
    const { fetcher, calls } = deferredFetcher();
    const suggester = createSuggester(fetcher);

    const pending = suggester.search('acc1', 'ada');
    suggester.cancel();
    expect(calls[0].signal.aborted).toBe(true);

    calls[0].resolve([ADA]);
    await expect(pending).resolves.toBeNull();
  });

  // Account and query are both free-form, so the key has to say where one ends
  // and the other begins. Joining them with a character that can occur inside
  // either — a space, most obviously — lets ('acc1', 'ada bob') and
  // ('acc1 ada', 'bob') land on one entry and answer for each other.
  it('keeps distinct account/query pairs from sharing a key', async () => {
    const fetcher = vi.fn().mockResolvedValueOnce([ADA]).mockResolvedValueOnce([ADAM]);
    const suggester = createSuggester(fetcher);

    await suggester.search('acc1', 'ada bob');

    expect(suggester.cached('acc1 ada', 'bob')).toBeUndefined();
    await expect(suggester.search('acc1 ada', 'bob')).resolves.toEqual([ADAM]);
  });
});

// A cached answer is a bet that address history has not moved underneath it,
// and that bet is only good for a moment: a send folds its recipients into
// history the instant it is durable. Without an expiry, a prefix that already
// matched somebody keeps serving the pre-send list until the entry is evicted
// or the tab is reloaded — the one case autocomplete exists for.
describe('cached answer freshness', () => {
  beforeEach(() => {
    // Fake timers keep the clock under the test's control: no wall-clock
    // sleeps, no dependence on how long a run takes.
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('bounds staleness to two seconds', () => {
    expect(SUGGESTION_TTL_MS).toBe(2000);
  });

  it('answers from cache without a request while the entry is fresh', async () => {
    const fetcher = vi.fn().mockResolvedValue([ADA]);
    const suggester = createSuggester(fetcher);

    await suggester.search('acc1', 'ada');
    vi.advanceTimersByTime(SUGGESTION_TTL_MS - 1);

    expect(suggester.cached('acc1', 'ada')).toEqual([ADA]);
    await expect(suggester.search('acc1', 'ada')).resolves.toEqual([ADA]);
    expect(fetcher).toHaveBeenCalledTimes(1);
  });

  it('re-asks once the entry expires, so a just-sent recipient shows up', async () => {
    const fetcher = vi.fn().mockResolvedValueOnce([ADA]).mockResolvedValueOnce([ADA, ADAM]);
    const suggester = createSuggester(fetcher);

    await suggester.search('acc1', 'ad');
    // ...the message to Adam is sent somewhere in here.
    vi.advanceTimersByTime(SUGGESTION_TTL_MS);

    expect(suggester.cached('acc1', 'ad')).toBeUndefined();
    await expect(suggester.search('acc1', 'ad')).resolves.toEqual([ADA, ADAM]);
    expect(fetcher).toHaveBeenCalledTimes(2);
  });

  it('starts a fresh window from the answer that replaced the expired one', async () => {
    const fetcher = vi.fn().mockResolvedValue([ADA]);
    const suggester = createSuggester(fetcher);

    await suggester.search('acc1', 'ada');
    vi.advanceTimersByTime(SUGGESTION_TTL_MS);
    await suggester.search('acc1', 'ada');
    expect(fetcher).toHaveBeenCalledTimes(2);

    vi.advanceTimersByTime(SUGGESTION_TTL_MS - 1);
    expect(suggester.cached('acc1', 'ada')).toEqual([ADA]);
    await suggester.search('acc1', 'ada');
    expect(fetcher).toHaveBeenCalledTimes(2);
  });

  it('expires each account independently', async () => {
    const fetcher = vi
      .fn()
      .mockResolvedValueOnce([ADA])
      .mockResolvedValueOnce([ADAM])
      .mockResolvedValueOnce([ADA]);
    const suggester = createSuggester(fetcher);

    await suggester.search('acc1', 'ad');
    vi.advanceTimersByTime(SUGGESTION_TTL_MS - 1);
    await suggester.search('acc2', 'ad');

    // acc1 expires first; acc2 was answered a beat later and is still fresh.
    vi.advanceTimersByTime(1);
    expect(suggester.cached('acc1', 'ad')).toBeUndefined();
    expect(suggester.cached('acc2', 'ad')).toEqual([ADAM]);
  });

  it('still refuses to cache an empty answer', async () => {
    const fetcher = vi.fn().mockResolvedValue([]);
    const suggester = createSuggester(fetcher);

    await suggester.search('acc1', 'zz');
    expect(suggester.cached('acc1', 'zz')).toBeUndefined();

    await suggester.search('acc1', 'zz');
    expect(fetcher).toHaveBeenCalledTimes(2);
  });
});

describe('module source', () => {
  // The key separator used to be a literal NUL, which made Git classify this
  // module as binary: no textual diff, no reviewable change, no merge. Keep
  // every byte of the source printable.
  it('carries no control characters', () => {
    // Tab (0x09) and newline (0x0a) are the only sub-0x20 bytes a source file
    // has any business holding.
    const control = [...moduleSource]
      .map((ch) => ch.charCodeAt(0))
      .filter((code) => code < 0x20 && code !== 0x09 && code !== 0x0a);

    expect(control).toEqual([]);
  });
});
