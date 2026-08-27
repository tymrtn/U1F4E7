<script lang="ts">
  import { Badge, Button, Drawer, EmptyState, Modal, MonoTag, Spinner, Toast } from '$lib/components';
  import { SelectionStore } from '$lib/selection.svelte';
  import MessageRow from '$lib/components/MessageRow.svelte';
  import BulkToolbar from '$lib/components/BulkToolbar.svelte';

  let modalOpen = $state(false);
  let drawerOpen = $state(false);
  let showToast = $state(true);

  // Selection-toolbar showcase (no backend): two rows, both pre-selected so the
  // sticky action surface renders. Junk/Snooze menus are pure UI state.
  const demoSelection = new SelectionStore();
  const demoMessages = [
    {
      key: 'demo:1', uid: 1, accountId: 'demo',
      subject: 'Re: [NousResearch/hermes-agent] Add linked-activity startup notices',
      from: 'notifications@github.com', date: '2026-07-29T10:00:00Z', snippet: null,
      unread: true, starred: false, accountChip: 'ty@tmrtn.com', href: '#'
    },
    {
      key: 'demo:2', uid: 2, accountId: 'demo',
      subject: 'Celebra el Día Mundial del Tigre en el Zoo',
      from: 'newsletter@info.zoomadrid.com', date: '2026-07-29T09:00:00Z', snippet: null,
      unread: false, starred: true, accountChip: 'ty@tmrtn.com', href: '#'
    }
  ];
  const demoOrdered = demoMessages.map((m) => m.key);
  demoSelection.toggle('demo:1');
  demoSelection.toggle('demo:2');
  const demoIndex: Record<
    string,
    { accountId: string; uid: number; from: string; folder: string; message_id?: string; subject?: string }
  > = {
    'demo:1': {
      accountId: 'demo', uid: 1,
      from: 'notifications@github.com', folder: 'INBOX', message_id: '<1@x>', subject: 'Re: hermes-agent'
    },
    'demo:2': { accountId: 'demo', uid: 2, from: 'newsletter@info.zoomadrid.com', folder: 'INBOX' }
  };
</script>

<div class="sink">
  <h1 class="sink-title">Primitives</h1>
  <p class="sink-lede">
    The design-system foundation for Envelope v2 — ink on paper, one green accent, DM Mono for
    machine identifiers.
  </p>

  <section class="block">
    <h2 class="block-head">Buttons</h2>
    <p class="block-note">Labels are always action verbs.</p>
    <div class="row">
      <Button variant="primary">Send</Button>
      <Button variant="ghost">Archive</Button>
      <Button variant="danger">Delete</Button>
      <Button variant="primary" disabled>Retry</Button>
    </div>
  </section>

  <section class="block">
    <h2 class="block-head">Badges</h2>
    <div class="row">
      <Badge variant="ok">Delivered</Badge>
      <Badge variant="warn">Pending review</Badge>
      <Badge variant="pending">Snoozed</Badge>
      <Badge variant="danger">Blocked</Badge>
    </div>
  </section>

  <section class="block">
    <h2 class="block-head">Mono tags</h2>
    <div class="row">
      <MonoTag>uid 38103</MonoTag>
      <MonoTag>&lt;CAF3x2@mail.gmail.com&gt;</MonoTag>
    </div>
  </section>

  <section class="block">
    <h2 class="block-head">Spinner</h2>
    <div class="row">
      <Spinner label="Fetching inbox" />
    </div>
  </section>

  <section class="block">
    <h2 class="block-head">Toast</h2>
    <div class="row toasts">
      {#if showToast}
        <Toast variant="ok" onclose={() => (showToast = false)}>Draft saved.</Toast>
      {/if}
      <Toast variant="warn">Reconnecting to mailbox.</Toast>
      <Toast variant="danger">Send failed. Check the recipient address.</Toast>
    </div>
  </section>

  <section class="block">
    <h2 class="block-head">Empty state</h2>
    <div class="framed">
      <EmptyState title="No drafts waiting" hint="Composed drafts appear here before you send them.">
        {#snippet action()}
          <Button variant="ghost">Compose</Button>
        {/snippet}
      </EmptyState>
    </div>
  </section>

  <section class="block">
    <h2 class="block-head">Selection toolbar</h2>
    <p class="block-note">
      Appears when rows are selected · icon+label, 44px targets on phone · Snooze/Junk are split
      menus · secondary actions (Mark read/unread · Unflag · Move…) live behind More · from an
      ordinary mailbox Delete moves to Trash (reversible).
    </p>
    <div class="framed sink-list">
      <BulkToolbar selection={demoSelection} folder="INBOX" messageIndex={demoIndex} />
      <ul class="sink-msg-list">
        {#each demoMessages as m (m.key)}
          <li>
            <MessageRow message={m} selection={demoSelection} orderedKeys={demoOrdered} />
          </li>
        {/each}
      </ul>
    </div>
    <p class="block-note">In the Trash view, Delete becomes a permanent, count + account-scope confirmed hard-delete:</p>
    <div class="framed sink-list">
      <BulkToolbar selection={demoSelection} folder="Trash" messageIndex={demoIndex} />
    </div>
  </section>

  <section class="block">
    <h2 class="block-head">Overlays</h2>
    <div class="row">
      <Button variant="ghost" onclick={() => (modalOpen = true)}>Open modal</Button>
      <Button variant="ghost" onclick={() => (drawerOpen = true)}>Open drawer</Button>
    </div>
  </section>
</div>

<Modal open={modalOpen} title="Discard draft?" onclose={() => (modalOpen = false)}>
  This removes the draft permanently. You can't undo it.
  {#snippet footer()}
    <Button variant="ghost" onclick={() => (modalOpen = false)}>Keep</Button>
    <Button variant="danger" onclick={() => (modalOpen = false)}>Discard</Button>
  {/snippet}
</Modal>

<Drawer open={drawerOpen} title="Message details" onclose={() => (drawerOpen = false)}>
  <p>Thread metadata, headers, and evidence actions render here.</p>
</Drawer>

<style>
  .sink {
    padding: 1.5rem 2rem;
    max-width: 52rem;
    margin: 0 auto;
    overflow-y: auto;
    width: 100%;
  }
  .sink-title {
    margin: 0 0 0.25rem;
    font-size: 1.25rem;
    font-weight: 700;
    letter-spacing: -0.01em;
  }
  .sink-lede {
    margin: 0 0 1.75rem;
    color: var(--env-muted);
    font-size: 0.875rem;
    max-width: 46ch;
    line-height: 1.5;
  }
  .block {
    padding: 1rem 0;
    border-top: 1px solid var(--env-rule);
  }
  .block-head {
    margin: 0 0 0.15rem;
    font-size: 0.9375rem;
    font-weight: 600;
  }
  .block-note {
    margin: 0 0 0.65rem;
    font-size: 0.75rem;
    color: var(--env-muted);
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.6rem;
  }
  .toasts {
    flex-direction: column;
    align-items: flex-start;
  }
  .framed {
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-md, 5px);
    background: var(--env-surface);
  }
  .sink-list {
    overflow: hidden;
  }
  .sink-msg-list {
    list-style: none;
    margin: 0;
    padding: 0;
  }
</style>
