<script lang="ts">
  // ThreadStrip — horizontal compact thread navigation for the reader pane.
  // Shows a bounded list of thread messages (oldest to newest); current
  // message is highlighted. Click loads that message.

  import type { ThreadMessage } from '$lib/reader-api';
  import { Spinner } from '$lib/components';

  interface Props {
    /** All messages in the thread. */
    messages: ThreadMessage[];
    /** UID of the message currently open in the reader. */
    currentUid: number;
    /** The folder the messages live in (passed through to the nav URL). */
    folder: string;
    /** The box segment of the current URL (e.g. "unified") — for building hrefs. */
    box: string;
    /** Account ID — for building hrefs. */
    accountId: string;
    /** When the API returned a capped list this is the total thread count. */
    totalCount?: number;
    /** True while the thread is being fetched. */
    loading?: boolean;
  }

  let {
    messages,
    currentUid,
    folder,
    box,
    accountId,
    totalCount,
    loading = false
  }: Props = $props();

  // Limit display to 8; show "+N more" when there are extra.
  const DISPLAY_LIMIT = 8;
  let visible = $derived(messages.slice(0, DISPLAY_LIMIT));
  let overflow = $derived(
    (totalCount ?? messages.length) > DISPLAY_LIMIT
      ? (totalCount ?? messages.length) - DISPLAY_LIMIT
      : 0
  );

  function msgHref(msg: ThreadMessage): string {
    const q = folder !== 'INBOX' ? `?folder=${encodeURIComponent(folder)}` : '';
    return `/v2/mail/${encodeURIComponent(box)}/${encodeURIComponent(accountId)}/${msg.uid}${q}`;
  }

  function fmtDate(iso: string | null): string {
    if (!iso) return '';
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return '';
    return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
  }

  function isMsgSeen(flags: string[]): boolean {
    return flags.some((f) => f.toLowerCase() === '\\seen');
  }

  /** Shorten an address to the local part or a display name. */
  function shortAddr(addr: string): string {
    const m = addr.match(/^(.+?)\s*<[^>]+>$/);
    if (m) return m[1].trim();
    return addr.split('@')[0] || addr;
  }
</script>

<div class="thread-strip" id="thread-strip" aria-label="Thread messages">
  {#if loading}
    <span class="thread-loading"><Spinner label="Loading thread" /></span>
  {:else if messages.length > 1}
    <span class="thread-label">Thread</span>
    <ul class="thread-messages">
      {#each visible as msg (msg.uid)}
        {@const current = msg.uid === currentUid}
        {@const read = isMsgSeen(msg.flags)}
        <li class="thread-msg-item">
          <a
            href={msgHref(msg)}
            class="thread-msg"
            class:is-current={current}
            class:is-unread={!read}
            aria-current={current ? 'page' : undefined}
            title="{msg.from_addr} — {msg.subject}"
          >
            <span class="thread-msg-from">{shortAddr(msg.from_addr)}</span>
            {#if msg.date}
              <span class="thread-msg-date">{fmtDate(msg.date)}</span>
            {/if}
          </a>
        </li>
      {/each}
      {#if overflow > 0}
        <li class="thread-overflow">+{overflow} more</li>
      {/if}
    </ul>
  {/if}
</div>

<style>
  .thread-strip {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    flex-wrap: wrap;
    padding: 0.5rem 0;
    border-bottom: 1px solid var(--env-rule);
    margin-bottom: 0.75rem;
  }
  .thread-loading {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    font-size: 0.75rem;
    color: var(--env-muted);
  }
  .thread-label {
    font-family: var(--font-mono);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--env-muted);
    white-space: nowrap;
    flex-shrink: 0;
  }
  .thread-messages {
    display: flex;
    align-items: center;
    gap: 0.25rem;
    flex-wrap: wrap;
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .thread-msg-item {
    display: flex;
  }
  .thread-msg {
    display: inline-flex;
    flex-direction: column;
    gap: 0.05rem;
    padding: 0.25rem 0.5rem;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-xs, 2px);
    text-decoration: none;
    font-size: 0.6875rem;
    color: var(--env-muted);
    background: var(--env-surface);
    transition: border-color 0.1s ease, background 0.1s ease;
    max-width: 9rem;
    overflow: hidden;
  }
  .thread-msg:hover {
    border-color: var(--env-accent);
    background: var(--env-accent-soft);
    color: var(--env-accent);
  }
  .thread-msg.is-current {
    border-color: var(--env-accent);
    background: var(--env-accent-soft);
    color: var(--env-accent);
    font-weight: 600;
  }
  .thread-msg.is-unread .thread-msg-from {
    font-weight: 700;
    color: var(--env-ink);
  }
  .thread-msg.is-current.is-unread .thread-msg-from {
    color: var(--env-accent);
  }
  .thread-msg-from {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 8rem;
  }
  .thread-msg-date {
    font-size: 0.625rem;
    color: var(--env-muted);
  }
  .thread-overflow {
    font-size: 0.6875rem;
    color: var(--env-muted);
    padding: 0.25rem 0.35rem;
    white-space: nowrap;
  }
</style>
