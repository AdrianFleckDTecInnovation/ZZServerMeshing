import { defineConfig } from 'vite';

export default defineConfig({
  server: {
    port: 4321,
    proxy: {
      '/assign': {
        target: process.env.VITE_COORDINATOR_URL || 'http://localhost:7871',
        changeOrigin: true,
      },
      '/servers': {
        target: process.env.VITE_COORDINATOR_URL || 'http://localhost:7871',
        changeOrigin: true,
      },
      '/dashboard': {
        target: process.env.VITE_COORDINATOR_URL || 'http://localhost:7871',
        changeOrigin: true,
      },
    },
  },
});
