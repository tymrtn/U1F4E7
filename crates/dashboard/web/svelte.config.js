import adapter from '@sveltejs/adapter-static';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

/** @type {import('@sveltejs/kit').Config} */
const config = {
  preprocess: vitePreprocess(),
  kit: {
    // SPA mode: no server, every route falls back to index.html so the axum
    // /v2 mount can serve one shell and let the client router take over.
    adapter: adapter({
      fallback: 'index.html',
      precompress: false
    }),
    // Mounted under /v2 by the dashboard. All asset URLs and links are
    // rewritten with this prefix at build time.
    paths: {
      base: '/v2',
      relative: false
    }
  }
};

export default config;
