// Reconnecting Server-Sent Events client for the Envelope dashboard.
//
// Wraps the browser `EventSource` connecting to `GET /api/events/stream` (the
// Rust `handlers::events_stream::stream` endpoint) with:
//   - exponential backoff reconnect (1s → 30s) with full jitter
//   - a connection-state store consumers can read/subscribe to
//   - a `degraded` flag that flips true after repeated failures so callers can
//     fall back to polling (the app already has poll/refresh paths)
//   - a typed `subscribe(types, handler)` fan-out API
//
// The backend event contract (see crates/dashboard/src/events.rs):
//   event: <type>   — one of the DashboardEventType values below
//   data:  <json>   — { type, account_id?, ...metadata }  (NO bodies/subjects)
// Plus a control frame `event: lagged` when a subscriber fell behind; consumers
// that care about exactness should re-poll on `lagged`.
//
// Auth: `EventSource` cannot set an Authorization header. In the browser the
// request rides the same cookie/tailnet-identity credential every other /api
// call uses, so no token handling is needed here. Bearer-only (non-browser)
// contexts pass `?access_token=` — exposed via the `accessToken` option for
// completeness, though the SvelteKit app itself relies on the cookie path.

/** Event `type` discriminants emitted by the backend. Mirrors DashboardEvent. */
export type DashboardEventType =
  | 'new_mail'
  | 'draft_queued'
  | 'draft_status_changed'
  | 'send_status'
  | 'unsnoozed'
  | 'account_health';

/** A control frame the backend emits when a subscriber lagged the channel. */
export const LAGGED_EVENT = 'lagged' as const;

/** Parsed event delivered to handlers: the JSON `data` payload, always carrying
 *  at least `type`. Fields are metadata only (ids/counts/status). */
export interface DashboardEvent {
  type: DashboardEventType;
  account_id?: string;
  [key: string]: unknown;
}

export type ConnectionState = 'connecting' | 'open' | 'reconnecting' | 'closed';

/** Handler invoked with each matching parsed event. */
export type EventHandler = (event: DashboardEvent) => void;

/** Minimal EventSource surface we depend on — lets tests inject a fake. */
export interface EventSourceLike {
  addEventListener(type: string, listener: (ev: MessageEvent) => void): void;
  close(): void;
  onopen: ((ev: Event) => void) | null;
  onerror: ((ev: Event) => void) | null;
}

export type EventSourceFactory = (url: string) => EventSourceLike;

export interface SseClientOptions {
  /** Endpoint path. Defaults to the dashboard SSE route. */
  url?: string;
  /** Bearer token for non-browser contexts (appended as `?access_token=`). */
  accessToken?: string;
  /** Min backoff in ms (default 1000). */
  minBackoffMs?: number;
  /** Max backoff in ms (default 30000). */
  maxBackoffMs?: number;
  /** Consecutive failures before flipping `degraded` true (default 4). */
  degradeAfter?: number;
  /** Injectable EventSource constructor (defaults to global). Tests pass a fake. */
  eventSourceFactory?: EventSourceFactory;
  /** Injectable timer (defaults to setTimeout). Returns an opaque handle. */
  setTimeoutImpl?: (fn: () => void, ms: number) => ReturnType<typeof setTimeout>;
  clearTimeoutImpl?: (handle: ReturnType<typeof setTimeout>) => void;
  /** Injectable RNG in [0,1) for jitter (defaults to Math.random). */
  random?: () => number;
}

const DEFAULT_URL = '/api/events/stream';
/** The concrete event names the backend can send (excludes the `lagged` control
 *  frame, which is subscribed to separately). */
const ALL_TYPES: DashboardEventType[] = [
  'new_mail',
  'draft_queued',
  'draft_status_changed',
  'send_status',
  'unsnoozed',
  'account_health'
];

/**
 * A reconnecting SSE client. Construct one per app session, call `start()`, and
 * register handlers with `subscribe()`. Read connection status via `getState()`
 * / `onStateChange()` and the polling-fallback hint via `isDegraded()`.
 */
export class SseClient {
  private readonly url: string;
  private readonly minBackoff: number;
  private readonly maxBackoff: number;
  private readonly degradeAfter: number;
  private readonly makeSource: EventSourceFactory;
  private readonly setTimeoutImpl: NonNullable<SseClientOptions['setTimeoutImpl']>;
  private readonly clearTimeoutImpl: NonNullable<SseClientOptions['clearTimeoutImpl']>;
  private readonly random: () => number;

  private source: EventSourceLike | null = null;
  private state: ConnectionState = 'closed';
  private failures = 0;
  private degraded = false;
  private stopped = true;
  private reconnectHandle: ReturnType<typeof setTimeout> | null = null;

  private readonly handlers = new Map<string, Set<EventHandler>>();
  private readonly stateListeners = new Set<(s: ConnectionState) => void>();
  private readonly degradedListeners = new Set<(d: boolean) => void>();

  constructor(opts: SseClientOptions = {}) {
    const base = opts.url ?? DEFAULT_URL;
    this.url = opts.accessToken
      ? `${base}${base.includes('?') ? '&' : '?'}access_token=${encodeURIComponent(opts.accessToken)}`
      : base;
    this.minBackoff = opts.minBackoffMs ?? 1000;
    this.maxBackoff = opts.maxBackoffMs ?? 30000;
    this.degradeAfter = opts.degradeAfter ?? 4;
    this.makeSource =
      opts.eventSourceFactory ??
      ((u: string) => new EventSource(u, { withCredentials: true }) as unknown as EventSourceLike);
    this.setTimeoutImpl = opts.setTimeoutImpl ?? ((fn, ms) => setTimeout(fn, ms));
    this.clearTimeoutImpl = opts.clearTimeoutImpl ?? ((h) => clearTimeout(h));
    this.random = opts.random ?? Math.random;
  }

  /** Open the connection (idempotent). */
  start(): void {
    if (!this.stopped) return;
    this.stopped = false;
    this.open();
  }

  /** Close the connection and cancel any pending reconnect. */
  stop(): void {
    this.stopped = true;
    if (this.reconnectHandle !== null) {
      this.clearTimeoutImpl(this.reconnectHandle);
      this.reconnectHandle = null;
    }
    this.closeSource();
    this.setState('closed');
  }

  /**
   * Register a handler for one or more event types. Pass `'*'` to receive every
   * event type. Returns an unsubscribe function.
   */
  subscribe(types: DashboardEventType[] | '*', handler: EventHandler): () => void {
    const list = types === '*' ? ALL_TYPES : types;
    for (const t of list) {
      let set = this.handlers.get(t);
      if (!set) {
        set = new Set();
        this.handlers.set(t, set);
      }
      set.add(handler);
    }
    return () => {
      for (const t of list) this.handlers.get(t)?.delete(handler);
    };
  }

  getState(): ConnectionState {
    return this.state;
  }

  isDegraded(): boolean {
    return this.degraded;
  }

  onStateChange(listener: (s: ConnectionState) => void): () => void {
    this.stateListeners.add(listener);
    return () => this.stateListeners.delete(listener);
  }

  onDegradedChange(listener: (d: boolean) => void): () => void {
    this.degradedListeners.add(listener);
    return () => this.degradedListeners.delete(listener);
  }

  // ── internals ───────────────────────────────────────────────────────

  private open(): void {
    this.setState(this.failures === 0 ? 'connecting' : 'reconnecting');
    const source = this.makeSource(this.url);
    this.source = source;

    source.onopen = () => {
      // A successful open resets the backoff and clears the degraded flag.
      this.failures = 0;
      this.setDegraded(false);
      this.setState('open');
    };

    source.onerror = () => {
      // EventSource fires `error` on network drop; the browser also
      // auto-retries, but we manage reconnect explicitly for backoff control.
      this.handleFailure();
    };

    for (const type of ALL_TYPES) {
      source.addEventListener(type, (ev) => this.dispatch(type, ev));
    }
    // Control frame: the backend emits `event: lagged` when a subscriber fell
    // behind. Surface it as its own dispatch so consumers can re-poll.
    source.addEventListener(LAGGED_EVENT, (ev) => this.dispatch(LAGGED_EVENT, ev));
  }

  private dispatch(type: string, ev: MessageEvent): void {
    const set = this.handlers.get(type);
    if (!set || set.size === 0) return;
    let payload: DashboardEvent;
    try {
      payload = JSON.parse((ev as MessageEvent).data) as DashboardEvent;
    } catch {
      // Malformed frame — deliver a minimal typed shape rather than throwing so
      // one bad frame never kills the stream.
      payload = { type: type as DashboardEventType };
    }
    for (const h of set) h(payload);
  }

  private handleFailure(): void {
    if (this.stopped) return;
    this.closeSource();
    this.failures += 1;
    if (this.failures >= this.degradeAfter) {
      this.setDegraded(true);
    }
    this.setState('reconnecting');
    const delay = this.backoffDelay(this.failures);
    this.reconnectHandle = this.setTimeoutImpl(() => {
      this.reconnectHandle = null;
      if (!this.stopped) this.open();
    }, delay);
  }

  /** Exponential backoff with full jitter, clamped to [min, max]. */
  private backoffDelay(attempt: number): number {
    const exp = Math.min(this.maxBackoff, this.minBackoff * 2 ** (attempt - 1));
    // Full jitter: uniform in [min, exp] so retries spread out.
    const jittered = this.minBackoff + this.random() * (exp - this.minBackoff);
    return Math.min(this.maxBackoff, Math.max(this.minBackoff, Math.floor(jittered)));
  }

  private closeSource(): void {
    if (this.source) {
      this.source.close();
      this.source = null;
    }
  }

  private setState(next: ConnectionState): void {
    if (this.state === next) return;
    this.state = next;
    for (const l of this.stateListeners) l(next);
  }

  private setDegraded(next: boolean): void {
    if (this.degraded === next) return;
    this.degraded = next;
    for (const l of this.degradedListeners) l(next);
  }
}

/**
 * Convenience: construct, start, and return an `SseClient`. Future components
 * (mail list, reader) should import the shared instance from
 * `$lib/live.svelte.ts` rather than constructing their own, so there is a single
 * connection per session.
 */
export function createSseClient(opts?: SseClientOptions): SseClient {
  const client = new SseClient(opts);
  client.start();
  return client;
}
