<script lang="ts">
  import '../app.css';
  import { base } from '$app/paths';
  import { page } from '$app/state';
  import { onMount, type Snippet } from 'svelte';
  import { api } from '$lib/api';

  let { children }: { children: Snippet } = $props();

  // The brand tag reports the version of the *running* backend, read from
  // /api/health. A compiled-in string would keep claiming the release the
  // bundle was built from even when a stale launchd service is serving it —
  // exactly the drift /api/health exists to expose. Until health answers (or
  // if it fails) the tag is omitted: no number is honest, a wrong one is not.
  let version = $state<string | null>(null);

  onMount(async () => {
    try {
      const health = await api.health();
      if (health.version) version = health.version;
    } catch {
      // Left null on purpose — the missing tag IS the signal. This probe is
      // decorative; failing the whole shell over it would be worse.
      version = null;
    }
  });
</script>

<div class="app-shell">
  <header class="app-header">
    <a class="brand" href={base || '/'}>
      <span class="brand-mark" aria-hidden="true">
        <svg width="14" height="10" viewBox="0 0 14 10" fill="none">
          <path
            d="M1 1L7 5.5L13 1"
            stroke="currentColor"
            stroke-width="1.5"
            stroke-linecap="round"
            stroke-linejoin="round"
          />
          <rect
            x="0.5"
            y="0.5"
            width="13"
            height="9"
            rx="0.5"
            stroke="currentColor"
            stroke-width="1"
          />
        </svg>
      </span>
      <span class="brand-name">Envelope</span>
      {#if version}
        <span class="brand-tag">v{version}</span>
      {/if}
    </a>
    <nav class="app-nav" aria-label="Primary navigation">
      <a class:is-active={page.url.pathname.startsWith(`${base}/mail`)} href="{base}/mail/unified">Mail</a>
      <a class:is-active={page.url.pathname.startsWith(`${base}/cockpit`)} href="{base}/cockpit">Cockpit</a>
      <a class:is-active={page.url.pathname.startsWith(`${base}/rules`)} href="{base}/rules">Rules</a>
    </nav>
  </header>

  <main class="app-main">
    {@render children()}
  </main>
</div>

<style>
  .app-shell {
    display: flex;
    flex-direction: column;
    height: 100vh;
    min-height: 0;
  }
  .app-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    height: 68px;
    padding: 0 1.125rem;
    border-bottom: 1px solid var(--env-rule);
    background: #fbfaf8;
    flex-shrink: 0;
  }
  .brand {
    display: inline-flex;
    align-items: center;
    gap: 0.55rem;
    text-decoration: none;
    color: var(--env-ink);
  }
  .brand-mark {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 32px;
    height: 32px;
    border: 2px solid var(--env-ink);
  }
  .brand-name {
    font-weight: 600;
    font-size: 1.05rem;
    letter-spacing: 0;
  }
  .brand-tag {
    font-family: var(--font-mono);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.15em;
    color: var(--env-muted);
  }
  .app-nav {
    display: inline-flex;
    gap: 0.35rem;
  }
  .app-nav a {
    min-height: 2rem;
    display: inline-flex;
    align-items: center;
    border: 1px solid transparent;
    padding: 0 0.75rem;
    font-size: 0.8125rem;
    color: var(--env-muted);
    text-decoration: none;
  }
  .app-nav a:hover {
    color: var(--env-accent);
    border-color: var(--env-rule);
    background: var(--env-surface);
  }
  .app-nav a.is-active {
    border-color: var(--env-ink);
    background: var(--env-ink);
    color: var(--env-surface);
  }
  .app-main {
    flex: 1;
    min-height: 0;
    display: flex;
  }
  @media (max-width: 640px) {
    .app-shell {
      height: auto;
      min-height: 100vh;
    }
    .app-header {
      height: auto;
      min-height: 68px;
      align-items: stretch;
      flex-direction: column;
      gap: 0.625rem;
      padding: 0.75rem;
    }
    .app-nav {
      display: grid;
      grid-template-columns: repeat(3, minmax(0, 1fr));
    }
    .app-nav a {
      justify-content: center;
    }
  }
</style>
