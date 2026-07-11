<script lang="ts">
  // Governor verdict badge. Maps a Governor decision bucket to the shared Badge
  // palette: allow→ok, review→pending, block/deny→danger. The label is the
  // human decision word; the accent carries the severity.
  import Badge from './Badge.svelte';
  import type { GovernorBucket } from '$lib/cockpit-api';

  let {
    verdict,
    decision
  }: {
    verdict: GovernorBucket;
    decision?: string;
  } = $props();

  const variant = $derived(
    verdict === 'allow' ? 'ok' : verdict === 'review' ? 'pending' : 'danger'
  );
  const label = $derived(decision ?? verdict);
</script>

<Badge {variant}>{label}</Badge>
