<script lang="ts">
  // Sender/account avatar: initials on a deterministic identity-hue fill
  // (A2 identity-first rows). Decorative — the sender name sits in the row
  // text beside it — so it is aria-hidden. `ring` is the CRM relationship
  // marker; it ships absent for launch (initials-only, per the locked
  // decision) and lights up with FRV R2.
  import { identityColor } from '$lib/hue';
  import { initials } from '$lib/identity';

  let {
    name,
    hueKey,
    size = 32,
    ring = false
  }: {
    /** Display label the initials come from. */
    name: string;
    /** Stable key the hue is derived from; defaults to `name`. */
    hueKey?: string;
    size?: number;
    /** CRM relationship ring (FRV R2). Off at launch. */
    ring?: boolean;
  } = $props();

  const label = $derived(initials(name));
  const fill = $derived(identityColor(hueKey ?? name));
</script>

<span
  class="avatar"
  class:has-ring={ring}
  style="--avatar-size: {size}px; --avatar-fill: {fill}; --avatar-font: {Math.round(size * 0.4)}px"
  aria-hidden="true"
  data-initials={label}
>
  <span class="avatar-initials">{label}</span>
</span>

<style>
  .avatar {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: var(--avatar-size);
    height: var(--avatar-size);
    border-radius: 50%;
    background: var(--avatar-fill);
    color: #fff;
    flex-shrink: 0;
    user-select: none;
  }
  .avatar.has-ring {
    /* Relationship ring: an outer accent halo, gapped from the fill. */
    box-shadow:
      0 0 0 2px var(--env-surface),
      0 0 0 3.5px var(--env-accent);
  }
  .avatar-initials {
    font-family: var(--font-mono);
    font-size: var(--avatar-font);
    font-weight: 500;
    line-height: 1;
    letter-spacing: 0.02em;
  }
</style>
