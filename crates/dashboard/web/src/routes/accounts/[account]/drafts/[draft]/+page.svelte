<script lang="ts">
  // /accounts/[account]/drafts/[draft] — the surface every generated draft link
  // resolves to (`review_url` / `dashboard_url` from the CLI and MCP, the
  // drafts API `dashboard_path`, and Review's row deep links).
  //
  // Thin shell by design, matching the reader route: all logic lives in
  // DraftComposer so it can be tested without a router.
  import DraftComposer from '$lib/components/DraftComposer.svelte';
</script>

<DraftComposer />
