// Live dashboard state backed by the SSE stream (Svelte 5 runes store).
//
// Holds the last event seen per type plus a running count per type, and mirrors
// the SSE connection state + `degraded` fallback hint as reactive properties.
// Future components (mail list, reader, account rail) consume this instead of
// touching the raw `SseClient`, so there is one connection and one reactive
// source of truth per session.
//
// Usage (later round, from a component/layout):
//   import { getLiveStore } from '$lib/live.svelte.ts';
//   const live = getLiveStore();          // starts the shared client once
//   $effect(() => { if (live.lastByType.new_mail) refetchInbox(); });
//   // or subscribe imperatively:
//   const off = live.on('draft_status_changed', (e) => { … });
//
// This module deliberately exposes a lazy singleton so importing it does not
// open a connection during SSR/tests; the connection starts on first
// `getLiveStore()` in the browser.

import {
  SseClient,
  type ConnectionState,
  type DashboardEvent,
  type DashboardEventType,
  type EventHandler,
  type SseClientOptions
} from './sse';

/** The event types the store tracks counts/last-value for. */
const TRACKED_TYPES: DashboardEventType[] = [
  'new_mail',
  'draft_queued',
  'draft_status_changed',
  'send_status',
  'unsnoozed',
  'account_health'
];

type CountMap = Record<DashboardEventType, number>;
type LastMap = Partial<Record<DashboardEventType, DashboardEvent>>;

function zeroCounts(): CountMap {
  return {
    new_mail: 0,
    draft_queued: 0,
    draft_status_changed: 0,
    send_status: 0,
    unsnoozed: 0,
    account_health: 0
  };
}

/**
 * Reactive live store. One per session. Wraps an [`SseClient`] and records the
 * most recent event and a running count per type. All reactive properties are
 * Svelte 5 runes, so `$derived`/`$effect` in components react automatically.
 */
export class LiveStore {
  /** Connection state, mirrored from the client. */
  connection = $state<ConnectionState>('closed');
  /** True when the stream gave up reconnecting — consumers should poll instead. */
  degraded = $state(false);
  /** Last event received per type (undefined until first of that type). */
  lastByType = $state<LastMap>({});
  /** Running count of events received per type since the store was created. */
  counts = $state<CountMap>(zeroCounts());
  /** Incremented whenever a `lagged` control frame arrives; a change signals
   *  consumers to re-poll for exactness. */
  laggedTicks = $state(0);

  private readonly client: SseClient;
  private readonly offFns: Array<() => void> = [];

  constructor(clientOrOpts?: SseClient | SseClientOptions) {
    this.client =
      clientOrOpts instanceof SseClient ? clientOrOpts : new SseClient(clientOrOpts);

    this.offFns.push(this.client.onStateChange((s) => (this.connection = s)));
    this.offFns.push(this.client.onDegradedChange((d) => (this.degraded = d)));

    this.offFns.push(
      this.client.subscribe(TRACKED_TYPES, (event) => {
        const t = event.type;
        // Reassign (not mutate) so the runes proxy sees a new reference.
        this.lastByType = { ...this.lastByType, [t]: event };
        this.counts = { ...this.counts, [t]: (this.counts[t] ?? 0) + 1 };
      })
    );

    // The `lagged` control frame is not a DashboardEventType; subscribe raw.
    this.offFns.push(
      this.client.subscribe('*', () => {
        /* no-op: '*' ensures every tracked type is wired even if TRACKED_TYPES
           drifts; real handling is above. */
      })
    );
  }

  /** Open the underlying connection (idempotent). */
  start(): void {
    this.client.start();
  }

  /** Close the connection and drop listeners. */
  stop(): void {
    for (const off of this.offFns.splice(0)) off();
    this.client.stop();
  }

  /** Imperative per-type subscription passthrough for components that prefer it. */
  on(types: DashboardEventType[] | '*', handler: EventHandler): () => void {
    return this.client.subscribe(types, handler);
  }

  /** Register a handler for the `lagged` control frame (re-poll trigger). */
  onLagged(handler: () => void): () => void {
    // sse.ts dispatches `lagged` under that literal type name.
    return this.client.subscribe(['lagged' as DashboardEventType], () => {
      this.laggedTicks += 1;
      handler();
    });
  }
}

// ── Lazy session singleton ────────────────────────────────────────────

let singleton: LiveStore | null = null;

/**
 * Return the shared session `LiveStore`, creating and starting it on first call.
 * Safe to call from multiple components — they all share one connection. Pass
 * options only on the first call (later calls ignore them and return the
 * existing store).
 */
export function getLiveStore(opts?: SseClientOptions): LiveStore {
  if (!singleton) {
    singleton = new LiveStore(opts);
    singleton.start();
  }
  return singleton;
}

/** Reset the singleton (tests / teardown). Stops the existing store first. */
export function __resetLiveStore(): void {
  singleton?.stop();
  singleton = null;
}
