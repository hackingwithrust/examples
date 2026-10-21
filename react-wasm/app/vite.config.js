import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  worker: {
    // The worker starts its engine with a top-level await, and Vite's build
    // bundles workers as IIFE by default — which does not allow it.
    format: 'es',
  },
  server: {
    fs: {
      // pkg sits outside the app root, and the dev server refuses to serve
      // files it has not been told about: without this, every wasm fetch
      // comes back 403 Forbidden.
      allow: ['..'],
    },
  },
});
