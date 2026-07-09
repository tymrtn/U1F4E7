import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  SseClient,
  type DashboardEvent,
  type EventSourceLike,
  type SseClientOptions
} from './sse';

// A controllable fake EventSource. Tests drive open/error/message manually so
// everything stays synchronous and deterministic (no real network or timers).
class FakeEventSource implements EventSourceLike {
  static instances: FakeEventSource[] = [];
  url: string;
  onopen: ((ev: Event) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;
  closed = false;
  private listeners = new Map<string, Array<(ev: MessageEvent) => void>>();

  constructor(url: string) {
    this.url = url;
    FakeEventSource.instances.push(this);
  }

  addEventListener(type: string, listener: (ev: MessageEvent) => void): void {
    const arr = this.listeners.get(type) ?? [];
    arr.push(listener);
    this.listeners.set(type, arr);
  }

  close(): void {
    this.closed = true;
  }

  // ── test drivers ──
  emitOpen(): void {
    this.onopen?.(new Event('open'));
  }

  emitError(): void {
    this.onerror?.(new Event('error'));
  }

  emit(type: string, data: unknown): void {
    const ev = { data: typeof data === 'string' ? data : JSON.stringify(data) } as MessageEvent;
    for (const l of this.listeners.get(type) ?? []) l(ev);
  }
}

/** A controllable fake timer: records scheduled callbacks so tests can fire
 *  them on demand without wall-clock waits. */
interface ScheduledJob {
  id: number;
  fn: () => void;
  ms: number;
}

function fakeTimer() {
  const scheduled: ScheduledJob[] = [];
  let next = 1;
  return {
    setTimeoutImpl: (fn: () => void, ms: number): ReturnType<typeof setTimeout> => {
      const id = next++;
      scheduled.push({ id, fn, ms });
      return id as unknown as ReturnType<typeof setTimeout>;
    },
    clearTimeoutImpl: (h: ReturnType<typeof setTimeout>): void => {
      const idx = scheduled.findIndex((s) => s.id === (h as unknown as number));
      if (idx >= 0) scheduled.splice(idx, 1);
    },
    /** Fire the most recently scheduled pending callback (the reconnect). */
    fireLast(): number | undefined {
      const job = scheduled.pop();
      job?.fn();
      return job?.ms;
    },
    pending: scheduled
  };
}

function makeClient(overrides: Partial<SseClientOptions> = {}) {
  const timer = fakeTimer();
  const client = new SseClient({
    minBackoffMs: 1000,
    maxBackoffMs: 30000,
    degradeAfter: 4,
    eventSourceFactory: (url) => new FakeEventSource(url),
    setTimeoutImpl: timer.setTimeoutImpl,
    clearTimeoutImpl: timer.clearTimeoutImpl,
    random: () => 0.5, // deterministic mid-jitter
    ...overrides
  });
  return { client, timer };
}

afterEach(() => {
  FakeEventSource.instances = [];
  vi.restoreAllMocks();
});

describe('SseClient connection lifecycle', () => {
  it('opens and transitions connecting → open', () => {
    const { client } = makeClient();
    const states: string[] = [];
    client.onStateChange((s) => states.push(s));

    client.start();
    expect(client.getState()).toBe('connecting');

    FakeEventSource.instances[0].emitOpen();
    expect(client.getState()).toBe('open');
    expect(states).toEqual(['connecting', 'open']);
  });

  it('delivers a typed event to a matching subscriber', () => {
    const { client } = makeClient();
    const received: DashboardEvent[] = [];
    client.subscribe(['new_mail'], (e) => received.push(e));
    client.start();
    FakeEventSource.instances[0].emitOpen();

    FakeEventSource.instances[0].emit('new_mail', {
      type: 'new_mail',
      account_id: 'acc1',
      unread_count: 3
    });

    expect(received).toHaveLength(1);
    expect(received[0]).toMatchObject({ type: 'new_mail', account_id: 'acc1', unread_count: 3 });
  });

  it('does not deliver an event type a handler did not subscribe to', () => {
    const { client } = makeClient();
    const received: DashboardEvent[] = [];
    client.subscribe(['new_mail'], (e) => received.push(e));
    client.start();
    FakeEventSource.instances[0].emitOpen();

    FakeEventSource.instances[0].emit('send_status', { type: 'send_status', account_id: 'a' });
    expect(received).toHaveLength(0);
  });

  it("'*' subscribes to every tracked type", () => {
    const { client } = makeClient();
    const received: DashboardEvent[] = [];
    client.subscribe('*', (e) => received.push(e));
    client.start();
    FakeEventSource.instances[0].emitOpen();

    FakeEventSource.instances[0].emit('unsnoozed', { type: 'unsnoozed', account_id: 'a' });
    FakeEventSource.instances[0].emit('account_health', { type: 'account_health', account_id: 'a' });
    expect(received.map((e) => e.type)).toEqual(['unsnoozed', 'account_health']);
  });

  it('tolerates a malformed frame without throwing and delivers a typed fallback', () => {
    const { client } = makeClient();
    const received: DashboardEvent[] = [];
    client.subscribe(['draft_queued'], (e) => received.push(e));
    client.start();
    FakeEventSource.instances[0].emitOpen();

    FakeEventSource.instances[0].emit('draft_queued', 'not-json{');
    expect(received).toHaveLength(1);
    expect(received[0].type).toBe('draft_queued');
  });
});

describe('SseClient reconnect + backoff', () => {
  it('reconnects on error with exponential backoff and creates a fresh source', () => {
    const { client, timer } = makeClient();
    client.start();
    FakeEventSource.instances[0].emitOpen();
    expect(client.getState()).toBe('open');

    // First failure → reconnecting, backoff ~min (attempt 1: 1000..1000 range).
    FakeEventSource.instances[0].emitError();
    expect(client.getState()).toBe('reconnecting');
    expect(FakeEventSource.instances[0].closed).toBe(true);
    const firstDelay = timer.pending[timer.pending.length - 1].ms;
    expect(firstDelay).toBe(1000);

    // Fire the reconnect timer → a new source is created.
    timer.fireLast();
    expect(FakeEventSource.instances).toHaveLength(2);

    // Second failure → larger backoff (attempt 2: exp=2000, mid jitter=1500).
    FakeEventSource.instances[1].emitError();
    const secondDelay = timer.pending[timer.pending.length - 1].ms;
    expect(secondDelay).toBeGreaterThan(firstDelay);
    expect(secondDelay).toBeLessThanOrEqual(30000);
  });

  it('clamps backoff to maxBackoffMs after many failures', () => {
    const { client, timer } = makeClient();
    client.start();
    // Drive many failures; each fire creates a new source then errors it.
    let lastDelay = 0;
    for (let i = 0; i < 12; i++) {
      const src = FakeEventSource.instances[FakeEventSource.instances.length - 1];
      src.emitError();
      lastDelay = timer.pending[timer.pending.length - 1].ms;
      timer.fireLast();
    }
    expect(lastDelay).toBeLessThanOrEqual(30000);
    // With attempt >= ~6 and mid jitter, we should be pinned near the ceiling.
    expect(lastDelay).toBeGreaterThanOrEqual(15000);
  });

  it('resets backoff and clears degraded after a successful reopen', () => {
    const { client, timer } = makeClient({ degradeAfter: 2 });
    const degradedStates: boolean[] = [];
    client.onDegradedChange((d) => degradedStates.push(d));
    client.start();

    // Two failures → degraded true.
    FakeEventSource.instances[0].emitError();
    timer.fireLast();
    FakeEventSource.instances[1].emitError();
    expect(client.isDegraded()).toBe(true);

    // Reconnect and open successfully → degraded clears, backoff resets.
    timer.fireLast();
    FakeEventSource.instances[2].emitOpen();
    expect(client.isDegraded()).toBe(false);
    expect(client.getState()).toBe('open');

    // Next failure starts backoff from the minimum again.
    FakeEventSource.instances[2].emitError();
    expect(timer.pending[timer.pending.length - 1].ms).toBe(1000);
    expect(degradedStates).toEqual([true, false]);
  });

  it('flips degraded true after degradeAfter consecutive failures', () => {
    const { client, timer } = makeClient({ degradeAfter: 3 });
    client.start();
    expect(client.isDegraded()).toBe(false);

    for (let i = 0; i < 3; i++) {
      const src = FakeEventSource.instances[FakeEventSource.instances.length - 1];
      src.emitError();
      if (i < 2) timer.fireLast();
    }
    expect(client.isDegraded()).toBe(true);
  });

  it('stop() cancels pending reconnect and closes the source', () => {
    const { client, timer } = makeClient();
    client.start();
    FakeEventSource.instances[0].emitError();
    expect(timer.pending).toHaveLength(1);

    client.stop();
    expect(timer.pending).toHaveLength(0);
    expect(client.getState()).toBe('closed');

    // A late error after stop() must not schedule a new reconnect.
    FakeEventSource.instances[0].emitError();
    expect(timer.pending).toHaveLength(0);
  });
});

describe('SseClient auth query token', () => {
  it('appends access_token to the URL for bearer-only contexts', () => {
    const { client } = makeClient({ accessToken: 'tok en&x' });
    client.start();
    expect(FakeEventSource.instances[0].url).toBe(
      '/api/events/stream?access_token=tok%20en%26x'
    );
  });

  it('uses the plain endpoint (cookie path) when no token is given', () => {
    const { client } = makeClient();
    client.start();
    expect(FakeEventSource.instances[0].url).toBe('/api/events/stream');
  });
});
