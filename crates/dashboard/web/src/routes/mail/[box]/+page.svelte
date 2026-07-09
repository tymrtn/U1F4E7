<script lang="ts">
  // Reader column with no message selected. The list lives in the parent
  // layout; picking a row navigates to the nested [account]/[uid] route.
  import { EmptyState } from '$lib/components';
</script>

<EmptyState
  title="Open a message to read it"
  hint="Select a message from the list. Reading never marks it read — Envelope peeks with BODY.PEEK."
/>
