<script lang="ts">
  import { Badge, Button, Drawer, EmptyState, Icon, Modal, MonoTag, Spinner, Toast } from '$lib/components';
  import { SelectionStore } from '$lib/selection.svelte';
  import MessageRow from '$lib/components/MessageRow.svelte';
  import BulkToolbar from '$lib/components/BulkToolbar.svelte';
  import { ICON_NAMES } from '$lib/icons';
  import { identityColor } from '$lib/hue';

  const HUE_KEYS = [
    'tyler@tmrtn.com',
    'work@example.com',
    'home@example.com',
    'desk@memberdesk.example',
    'newsletter@info.zoomadrid.com',
    'notifications@github.com'
  ];

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
            <MessageRow
              message={{ ...m, folder: 'INBOX' }}
              selection={demoSelection}
              orderedKeys={demoOrdered}
              verbs
            />
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

  <section class="block">
    <h2 class="block-head">Icons</h2>
    <p class="block-note">
      Vendored Lucide subset (design plan rev 3) — stroke 1.75 on the 24px grid, decorative unless
      labeled.
    </p>
    <div class="row icon-row">
      {#each ICON_NAMES as name (name)}
        <span class="icon-cell"><Icon {name} size={18} /><span class="icon-name">{name}</span></span>
      {/each}
    </div>
  </section>

  <section class="block">
    <h2 class="block-head">Identity hue</h2>
    <p class="block-note">
      Deterministic color per account/sender; lightness is solved per hue so white initials always
      pass AA (4.5:1). Color is metadata (A3) — a tick or an avatar fill, never a row wash.
    </p>
    <div class="row">
      {#each HUE_KEYS as key (key)}
        <span class="hue-swatch" style="background: {identityColor(key)}"
          >{key.slice(0, 2).toUpperCase()}</span
        >
      {/each}
    </div>
  </section>

  <section class="block">
    <h2 class="block-head">Instrument rail (rev 3 chrome)</h2>
    <p class="block-note">
      Static token sample — dark ground, icon-led rows, quiet mono counts, active = lift + accent
      edge. The live rail is on every mail route; this exists to eyeball tokens without a backend.
    </p>
    <div class="rail-sample">
      <p class="rs-label">Process</p>
      <span class="rs-item is-active"
        ><Icon name="inbox" size={15} /><span class="rs-l">Inbox</span><span class="rs-n">12</span
        ></span
      >
      <span class="rs-item"
        ><Icon name="zap" size={15} /><span class="rs-l">Today</span><span class="rs-n">4</span
        ></span
      >
      <span class="rs-item"
        ><Icon name="hourglass" size={15} /><span class="rs-l">Waiting On</span><span class="rs-n"
          >6</span
        ></span
      >
      <span class="rs-item"
        ><Icon name="clock" size={15} /><span class="rs-l">Snoozed</span><span class="rs-n">5</span
        ></span
      >
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
  .icon-row {
    flex-wrap: wrap;
    gap: 0.9rem;
  }
  .icon-cell {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    color: var(--env-ink);
  }
  .icon-name {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    color: var(--env-muted);
  }
  .hue-swatch {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 32px;
    height: 32px;
    border-radius: 50%;
    color: #fff;
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    font-weight: 500;
  }
  .rail-sample {
    background: var(--env-rail-ground);
    color: var(--env-rail-text);
    border-radius: var(--radius-md, 5px);
    padding: 0.75rem 0.6rem;
    max-width: 200px;
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
  }
  .rs-label {
    font-family: var(--font-mono);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.14em;
    color: var(--env-rail-muted);
    margin: 0 0 0.3rem;
    padding: 0 0.5rem;
  }
  .rs-item {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    padding: 0 0.5rem;
    height: 32px;
    font-size: 0.8125rem;
    border-radius: var(--radius-sm, 3px);
    color: var(--env-rail-text);
  }
  .rs-item :global(.icon) {
    color: var(--env-rail-muted);
  }
  .rs-item.is-active {
    background: var(--env-rail-lift);
    color: var(--env-rail-active-text);
    font-weight: 600;
    box-shadow: inset 2px 0 0 var(--env-rail-accent);
  }
  .rs-item.is-active :global(.icon) {
    color: var(--env-rail-accent);
  }
  .rs-l {
    flex: 1;
  }
  .rs-n {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    color: var(--env-rail-muted);
    font-variant-numeric: tabular-nums;
  }
  .rs-item.is-active .rs-n {
    color: var(--env-rail-text);
  }
</style>
