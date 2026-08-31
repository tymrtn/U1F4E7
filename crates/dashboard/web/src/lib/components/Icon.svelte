<script lang="ts">
  // Single entry point for line icons (vendored Lucide paths in $lib/icons).
  // Decorative by default (aria-hidden); pass `label` when the icon is the
  // only content of a control and must speak for itself.
  import { ICON_PATHS, type IconName } from '$lib/icons';

  let {
    name,
    size = 16,
    strokeWidth = 1.75,
    label
  }: {
    name: IconName;
    size?: number;
    strokeWidth?: number;
    label?: string;
  } = $props();
</script>

<svg
  class="icon"
  data-icon={name}
  width={size}
  height={size}
  viewBox="0 0 24 24"
  fill="none"
  stroke="currentColor"
  stroke-width={strokeWidth}
  stroke-linecap="round"
  stroke-linejoin="round"
  role={label ? 'img' : undefined}
  aria-label={label}
  aria-hidden={label ? undefined : 'true'}
>
  {#each ICON_PATHS[name] as d (d)}
    <path {d} />
  {/each}
</svg>

<style>
  .icon {
    flex-shrink: 0;
    display: inline-block;
    vertical-align: -0.125em;
  }
</style>
