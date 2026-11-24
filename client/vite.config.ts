import { defineConfig } from 'vite';

const coordinator = process.env.MESH_COORDINATOR_URL || process.env.VITE_COORDINATOR_URL || 'http://127.0.0.1:7871';

export default defineConfig({
  server: {
    port: 4321,
    proxy: {
      '/assign': {
        target: coordinator,
        changeOrigin: true,
      },
      '/servers': {
        target: coordinator,
        changeOrigin: true,
      },
      '/dashboard': {
        target: coordinator,
        changeOrigin: true,
      },
    },
  },
});
