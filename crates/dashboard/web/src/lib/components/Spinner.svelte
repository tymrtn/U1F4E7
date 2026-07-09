<script lang="ts">
  // Indeterminate loading spinner. Purely decorative; carries an aria-label so
  // assistive tech announces the loading state.
  let { label = 'Loading' }: { label?: string } = $props();
</script>

<span class="env-spinner" role="status" aria-label={label}></span>

<style>
  .env-spinner {
    display: inline-block;
    width: 1rem;
    height: 1rem;
    border: 2px solid var(--env-rule);
    border-top-color: var(--env-accent);
    border-radius: 50%;
    animation: env-spin 0.7s linear infinite;
  }
  @keyframes env-spin {
    to {
      transform: rotate(360deg);
    }
  }
</style>
