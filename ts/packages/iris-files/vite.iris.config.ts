import { defineConfig, type Plugin } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import UnoCSS from 'unocss/vite';
import { VitePWA } from 'vite-plugin-pwa';
import { resolve } from 'path';
import { rename } from 'fs/promises';

function irisEntryPlugin(): Plugin {
  return {
    name: 'iris-entry',
    configureServer(server) {
      server.middlewares.use((req, _res, next) => {
        if (req.url === '/') {
          req.url = '/iris.html';
        }
        next();
      });
    },
    async closeBundle() {
      // Preserve Iris Files entry, then rename iris.html to index.html for production
      try {
        await rename(
          resolve(__dirname, 'dist-iris/index.html'),
          resolve(__dirname, 'dist-iris/files.html')
        );
      } catch {
        // Ignore if file doesn't exist (dev mode)
      }
      try {
        await rename(
          resolve(__dirname, 'dist-iris/iris.html'),
          resolve(__dirname, 'dist-iris/index.html')
        );
      } catch {
        // Ignore if file doesn't exist (dev mode)
      }
    },
  };
}

export default defineConfig({
  define: {
    'import.meta.env.VITE_BUILD_TIME': JSON.stringify(new Date().toISOString()),
  },
  plugins: [
    irisEntryPlugin(),
    UnoCSS(),
    svelte(),
    VitePWA({
      registerType: 'autoUpdate',
      strategies: 'injectManifest',
      srcDir: 'src',
      filename: 'sw.ts',
      includeAssets: ['iris-favicon.png', 'apple-touch-icon.png'],
      devOptions: {
        enabled: true,
        type: 'module',
      },
      manifest: {
        name: 'Iris',
        short_name: 'Iris',
        description: 'Nostr & Hashtree browser',
        theme_color: '#0d1117',
        background_color: '#0d1117',
        display: 'standalone',
        icons: [
          {
            src: 'iris-logo.png',
            sizes: '192x192',
            type: 'image/png',
          },
          {
            src: 'iris-logo.png',
            sizes: '512x512',
            type: 'image/png',
          },
          {
            src: 'iris-logo.png',
            sizes: '512x512',
            type: 'image/png',
            purpose: 'any maskable',
          },
        ],
      },
      injectManifest: {
        globPatterns: ['**/*.{js,css,html,ico,png,svg,wasm}'],
        globIgnores: ['**/ffmpeg-core.*'],
        maximumFileSizeToCacheInBytes: 5 * 1024 * 1024,
      },
    }),
  ],
  root: resolve(__dirname),
  resolve: {
    alias: {
      'hashtree': resolve(__dirname, '../hashtree/src/index.ts'),
      'hashtree-index': resolve(__dirname, '../hashtree-index/src/index.ts'),
      '$lib': resolve(__dirname, 'src/lib'),
      'wasm-git': resolve(__dirname, 'public/lg2_async.js'),
    },
  },
  build: {
    outDir: 'dist-iris',
    emptyOutDir: true,
    reportCompressedSize: true,
    chunkSizeWarningLimit: 2000,
    rollupOptions: {
      input: {
        main: resolve(__dirname, 'iris.html'),
        files: resolve(__dirname, 'index.html'),
        video: resolve(__dirname, 'video.html'),
        docs: resolve(__dirname, 'docs.html'),
      },
      onLog(level, log, handler) {
        if (log.code === 'CIRCULAR_DEPENDENCY') return;
        const message = typeof log.message === 'string' ? log.message : '';
        if (message.includes('dynamic import will not move module into another chunk')) return;
        if (message.includes('Use of eval in') && message.includes('tseep')) return;
        if (message.includes('has been externalized for browser compatibility')) return;
        handler(level, log);
      },
      output: {
        assetFileNames: (assetInfo) => {
          if (assetInfo.name?.endsWith('.wasm')) {
            return 'assets/[name][extname]';
          }
          return 'assets/[name]-[hash][extname]';
        },
        manualChunks: (id) => {
          // Cashu wallet
          if (id.includes('coco-cashu') || id.includes('cashu-ts')) {
            return 'wallet';
          }

          // NDK
          if (id.includes('@nostr-dev-kit/ndk')) {
            return 'ndk';
          }

          // Dexie
          if (id.includes('dexie')) {
            return 'dexie';
          }

          // Core vendor libraries
          const vendorLibs = [
            'svelte',
            'nostr-tools',
            '@noble/hashes',
            '@noble/curves',
            '@scure/base',
            'idb-keyval',
          ];
          if (vendorLibs.some((lib) => id.includes(`node_modules/${lib}`))) {
            return 'vendor';
          }
        },
      },
    },
  },
  server: {
    host: '0.0.0.0',
    port: 5174, // Different port for iris dev
    hmr: {
      overlay: true,
    },
  },
  optimizeDeps: {
    exclude: ['wasm-git'],
  },
  assetsInclude: ['**/*.wasm'],
});
