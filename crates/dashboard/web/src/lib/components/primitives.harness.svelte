<script lang="ts">
  // Test-only harness: renders every primitive with representative props so the
  // tripwire test can scan all accessible names and visible text at once.
  import Badge from './Badge.svelte';
  import Button from './Button.svelte';
  import Drawer from './Drawer.svelte';
  import EmptyState from './EmptyState.svelte';
  import Modal from './Modal.svelte';
  import MonoTag from './MonoTag.svelte';
  import Spinner from './Spinner.svelte';
  import Toast from './Toast.svelte';
</script>

<Spinner label="Fetching inbox" />

<MonoTag>uid 38103</MonoTag>

<Badge variant="ok">Delivered</Badge>
<Badge variant="warn">Pending review</Badge>
<Badge variant="pending">Snoozed</Badge>
<Badge variant="danger">Blocked</Badge>

<Button variant="primary">Send</Button>
<Button variant="ghost">Archive</Button>
<Button variant="danger">Delete</Button>

<Toast variant="ok" onclose={() => {}}>Draft saved.</Toast>
<Toast variant="warn">Reconnecting to mailbox.</Toast>
<Toast variant="danger">Send failed. Check the recipient address.</Toast>

<EmptyState title="No drafts waiting" hint="Composed drafts appear here before you send them.">
  {#snippet action()}
    <Button variant="ghost">Compose</Button>
  {/snippet}
</EmptyState>

<Modal open={true} title="Discard draft?" onclose={() => {}}>
  This removes the draft permanently.
  {#snippet footer()}
    <Button variant="danger">Discard</Button>
  {/snippet}
</Modal>

<Drawer open={true} title="Message details" onclose={() => {}}>
  Thread metadata and evidence actions render here.
</Drawer>
