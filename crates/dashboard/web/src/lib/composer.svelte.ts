// Composer store — shared singleton that controls the composer drawer.
//
// Usage from any component:
//   import { getComposerStore } from '$lib/composer.svelte';
//   const composer = getComposerStore();
//   composer.open('compose', { accountId: 'acc1' });
//   composer.open('reply', { accountId: 'acc1', parentUid: 101, parentFolder: 'INBOX', replyAll: false });
//   composer.open('forward', { accountId: 'acc1', parentUid: 101, parentFolder: 'INBOX' });
//
// The reader agent can call `open(mode, context)` without importing ComposerDrawer
// — the store is the coordination point. ComposerDrawer reads it.

export type ComposerMode = 'compose' | 'reply' | 'reply-all' | 'forward';

export interface ComposeContext {
  accountId: string;
  /** Reply/reply-all/forward: UID of the parent message. */
  parentUid?: number;
  /** Reply/reply-all/forward: folder of the parent message. Default 'INBOX'. */
  parentFolder?: string;
  /** Prefill: To field (compose only). */
  to?: string;
  /** Prefill: Subject (compose/forward). */
  subject?: string;
  /** Prefill: body snippet (forward quote text). */
  bodyPrefix?: string;
}

export class ComposerStore {
  isOpen = $state(false);
  mode = $state<ComposerMode>('compose');
  context = $state<ComposeContext>({ accountId: '' });

  /** Open the composer in the given mode with the given context. */
  open(mode: ComposerMode, ctx: ComposeContext): void {
    this.mode = mode;
    this.context = ctx;
    this.isOpen = true;
  }

  /** Close and reset. */
  close(): void {
    this.isOpen = false;
    this.context = { accountId: '' };
  }
}

// ── Lazy session singleton ────────────────────────────────────────────

let singleton: ComposerStore | null = null;

/**
 * Return the shared session ComposerStore, creating it on first call.
 * Safe to call from multiple components.
 */
export function getComposerStore(): ComposerStore {
  if (!singleton) {
    singleton = new ComposerStore();
  }
  return singleton;
}

/** Reset the singleton (tests / teardown). */
export function __resetComposerStore(): void {
  singleton = null;
}
