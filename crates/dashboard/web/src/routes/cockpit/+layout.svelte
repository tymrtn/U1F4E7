<script lang="ts">
  // Cockpit shell: the instrument rail framing the fleet view (design plan
  // rev 3, D). Same rail column as the mail and digest shells so the chrome
  // never shifts between routes — the cockpit is the money shot, and it now
  // sits inside the same dark instrument frame.
  import type { Snippet } from 'svelte';
  import { Rail } from '$lib/components';

  let { children }: { children: Snippet } = $props();
</script>

<div class="cockpit-shell">
  <Rail />
  <div class="cockpit-main">
    {@render children()}
  </div>
</div>

<style>
  .cockpit-shell {
    display: grid;
    grid-template-columns: 240px minmax(0, 1fr);
    height: 100%;
    min-height: 0;
  }
  .cockpit-main {
    min-height: 0;
    overflow-y: auto;
    background: var(--env-page);
  }
  @media (max-width: 1100px) {
    .cockpit-shell {
      grid-template-columns: 220px minmax(0, 1fr);
    }
  }
  @media (max-width: 760px) {
    .cockpit-shell {
      grid-template-columns: minmax(0, 1fr);
    }
    .cockpit-shell > :global(.rail) {
      display: none;
    }
  }
</style>
