<script lang="ts">
  // Minimal 3-pane webmail skeleton. Real navigation, data loading, and the
  // reader all land in the next wave — this establishes the frame and tokens.
  import { EmptyState } from '$lib/components';
</script>

<div class="mail-shell">
  <aside class="rail" aria-label="Mailboxes">
    <p class="rail-label">Mailboxes</p>
    <ul class="rail-list">
      <li class="rail-item is-active">Unified Inbox</li>
      <li class="rail-item">Needs Attention</li>
      <li class="rail-item">Snoozed</li>
      <li class="rail-item">Drafts</li>
      <li class="rail-item">Sent</li>
      <li class="rail-item">All Mail</li>
    </ul>
    <p class="rail-note">Navigation is a placeholder — wiring lands next wave.</p>
  </aside>

  <section class="list" aria-label="Message list">
    <header class="pane-head">
      <span class="pane-title">Unified Inbox</span>
    </header>
    <EmptyState
      title="No messages loaded"
      hint="The message list mounts here once the inbox data flow is wired."
    />
  </section>

  <section class="reader" aria-label="Reader">
    <EmptyState
      title="Open a message to read it"
      hint="The reader pane renders the selected message, its thread, and actions."
    />
  </section>
</div>

<style>
  .mail-shell {
    flex: 1;
    min-height: 0;
    display: grid;
    grid-template-columns: 240px minmax(320px, 1fr) minmax(360px, 1.4fr);
  }
  .rail {
    border-right: 1px solid var(--env-rule);
    padding: 0.85rem 0.75rem;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    background: var(--env-paper);
    overflow-y: auto;
  }
  .rail-label,
  .pane-title {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    text-transform: uppercase;
    letter-spacing: 0.12em;
    color: var(--env-muted);
    margin: 0 0 0.35rem;
  }
  .rail-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
  }
  .rail-item {
    padding: 0.35rem 0.5rem;
    font-size: 0.8125rem;
    border-radius: var(--radius-sm, 3px);
    cursor: default;
    color: var(--env-ink);
  }
  .rail-item:hover {
    background: var(--env-accent-soft);
  }
  .rail-item.is-active {
    background: var(--env-accent-soft);
    color: var(--env-accent);
    font-weight: 600;
  }
  .rail-note {
    margin-top: auto;
    font-size: 0.6875rem;
    color: var(--env-muted);
    line-height: 1.4;
  }
  .list {
    border-right: 1px solid var(--env-rule);
    display: flex;
    flex-direction: column;
    min-height: 0;
    overflow-y: auto;
  }
  .reader {
    display: flex;
    flex-direction: column;
    min-height: 0;
    overflow-y: auto;
    background: var(--env-surface);
  }
  .pane-head {
    padding: 0.75rem 1rem 0;
  }
</style>
