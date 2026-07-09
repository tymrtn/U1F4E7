import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vitest/config';

// Standalone Vitest config so the browser resolve condition (needed for
// @testing-library/svelte to mount() in jsdom) never leaks into the SvelteKit
// production build in vite.config.ts.
export default defineConfig({
  plugins: [svelte({ compilerOptions: { dev: true } })],
  resolve: {
    conditions: ['browser']
  },
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['./vitest-setup.ts'],
    include: ['src/**/*.{test,spec}.{js,ts}']
  }
});
