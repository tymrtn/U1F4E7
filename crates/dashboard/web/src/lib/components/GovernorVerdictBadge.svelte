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

<!-- A Governor verdict reads as an instrument readout: mono, uppercase, the
     decision word carried by the Badge's severity accent. -->
<Badge {variant}><span class="verdict-readout">{label}</span></Badge>

<style>
  .verdict-readout {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
</style>
