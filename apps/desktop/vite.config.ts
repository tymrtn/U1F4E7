import { defineConfig } from "vite";

// Root is the package dir so index.html's /src/main.ts entry resolves.
// Port is fixed and strict because tauri.conf.json devUrl pins 127.0.0.1:1420,
// and build emits to ./dist to match its frontendDist of "../dist".
export default defineConfig({
  clearScreen: false,
  server: {
    host: "127.0.0.1",
    port: 1420,
    strictPort: true,
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "es2021",
    sourcemap: true,
  },
});
