import { readFileSync } from 'node:fs';
import adapter from '@sveltejs/adapter-static';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

// The build output is committed and embedded via rust-embed, and CI rebuilds
// it to detect drift, so builds must be byte-for-byte reproducible. SvelteKit
// defaults kit.version.name to Date.now(), which rewrites version.json and
// every content-hashed chunk on each build. Pin it to the workspace version
// from the root Cargo.toml instead: the bundle then only changes when sources
// or the release version change, and client-side update detection still fires
// on real releases.
function workspaceVersion() {
  const cargoToml = readFileSync(new URL('../../../Cargo.toml', import.meta.url), 'utf8');
  const workspacePackage = cargoToml.match(/\[workspace\.package\]([^[]*)/);
  const version = workspacePackage?.[1].match(/^version\s*=\s*"([^"]+)"/m);
  if (!version) {
    throw new Error('svelte.config.js: no [workspace.package] version in root Cargo.toml');
  }
  return version[1];
}

/** @type {import('@sveltejs/kit').Config} */
const config = {
  preprocess: vitePreprocess(),
  kit: {
    version: {
      name: workspaceVersion()
    },
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
