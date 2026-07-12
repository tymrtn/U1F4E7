import adapter from '@sveltejs/adapter-static';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

/** @type {import('@sveltejs/kit').Config} */
const config = {
  preprocess: vitePreprocess(),
  kit: {
    // SPA mode: no server, every route falls back to index.html so the axum
    // dashboard can serve one shell and let the client router take over.
    adapter: adapter({
      fallback: 'index.html',
      precompress: false
    }),
    // Served at the site root by the dashboard (v2 is the dashboard as of
    // 1.0.0 — the old /v2 mount and the v1 static dashboard are gone). Empty
    // base keeps every asset URL and link root-relative.
    paths: {
      base: '',
      relative: false
    }
  }
};

export default config;
