<script lang="ts">
  import '../app.css';
  import { base } from '$app/paths';
  import type { Snippet } from 'svelte';

  let { children }: { children: Snippet } = $props();
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
      <span class="brand-tag">v2</span>
    </a>
    <nav class="app-nav">
      <a href="{base}/mail/unified">Mail</a>
      <a href="{base}/kitchen-sink">Kitchen sink</a>
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
    height: 56px;
    padding: 0 1rem;
    border-bottom: 1px solid var(--env-rule);
    background: var(--env-paper);
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
    width: 28px;
    height: 28px;
    border: 2px solid var(--env-ink);
  }
  .brand-name {
    font-weight: 600;
    font-size: 1.05rem;
    letter-spacing: -0.01em;
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
    gap: 1rem;
  }
  .app-nav a {
    font-size: 0.8125rem;
    color: var(--env-muted);
    text-decoration: none;
  }
  .app-nav a:hover {
    color: var(--env-accent);
  }
  .app-main {
    flex: 1;
    min-height: 0;
    display: flex;
  }
</style>
