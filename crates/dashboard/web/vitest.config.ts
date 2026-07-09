import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vitest/config';
import { fileURLToPath } from 'node:url';

// Standalone Vitest config so the browser resolve condition (needed for
// @testing-library/svelte to mount() in jsdom) never leaks into the SvelteKit
// production build in vite.config.ts.
export default defineConfig({
  plugins: [svelte({ compilerOptions: { dev: true } })],
  resolve: {
    conditions: ['browser'],
    // Mirror SvelteKit's `$lib` alias for component tests. `$app/*` modules are
    // provided by SvelteKit at build time and aren't available under Vitest, so
    // tests that touch them mock `$app/*` explicitly with vi.mock().
    alias: {
      $lib: fileURLToPath(new URL('./src/lib', import.meta.url)),
      '$app/paths': fileURLToPath(new URL('./src/test-stubs/app-paths.ts', import.meta.url)),
      '$app/state': fileURLToPath(new URL('./src/test-stubs/app-state.ts', import.meta.url)),
      '$app/navigation': fileURLToPath(
        new URL('./src/test-stubs/app-navigation.ts', import.meta.url)
      )
    }
  },
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['./vitest-setup.ts'],
    include: ['src/**/*.{test,spec}.{js,ts}']
  }
});
