<script lang="ts">
  // Digest shell: the instrument rail framing the board. Same rail column as
  // the mail shell so switching routes never shifts the chrome.
  import type { Snippet } from 'svelte';
  import { Rail } from '$lib/components';

  let { children }: { children: Snippet } = $props();
</script>

<div class="digest-shell">
  <Rail />
  <div class="digest-main">
    {@render children()}
  </div>
</div>

<style>
  .digest-shell {
    display: grid;
    grid-template-columns: 240px minmax(0, 1fr);
    height: 100%;
    min-height: 0;
  }
  .digest-main {
    min-height: 0;
    overflow-y: auto;
    background: var(--env-page);
  }
  @media (max-width: 1100px) {
    .digest-shell {
      grid-template-columns: 220px minmax(0, 1fr);
    }
  }
  @media (max-width: 760px) {
    .digest-shell {
      grid-template-columns: minmax(0, 1fr);
    }
    .digest-shell > :global(.rail) {
      display: none;
    }
  }
</style>
