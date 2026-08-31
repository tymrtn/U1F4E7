<script lang="ts">
  // Root route: load() redirects to /mail/unified. This fallback only shows if
  // a client-side navigation lands here before the redirect resolves.
  import { base } from '$app/paths';
  import { goto } from '$app/navigation';
  import { onMount } from 'svelte';
  import { Spinner } from '$lib/components';

  onMount(() => {
    goto(`${base}/mail/unified`, { replaceState: true });
  });
</script>

<div class="root-redirect">
  <Spinner label="Opening Digest" />
</div>

<style>
  .root-redirect {
    flex: 1;
    display: flex;
    align-items: center;
    justify-content: center;
  }
</style>
