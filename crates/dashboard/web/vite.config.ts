import { sveltekit } from '@sveltejs/kit/vite';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';

export default defineConfig({
  plugins: [tailwindcss(), sveltekit()],
  server: {
    port: 5173,
    proxy: {
      // Forward the dashboard REST surface to the running axum backend so the
      // CSRF cookie flow works end-to-end in dev. Cookies are forwarded because
      // the origin (localhost:5173) and target (127.0.0.1:3141) are both
      // loopback and the backend sets SameSite=Strict on plain HTTP.
      '/api': {
        target: 'http://127.0.0.1:3141',
        changeOrigin: false
      }
    }
  }
});
