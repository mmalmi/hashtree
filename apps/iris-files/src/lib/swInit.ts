/**
 * Service Worker initialization utilities
 * Shared between all app entry points (main, docs, video)
 */

import { registerSW } from 'virtual:pwa-register';
import { setupSwFileHandler } from './swFileHandler';
import { getHtreePrefix } from './mediaUrl';
import { isTauri } from '../tauri';

interface InitOptions {
  /** Require cross-origin isolation (for SharedArrayBuffer/FFmpeg) */
  requireCrossOriginIsolation?: boolean;
}

/**
 * Initialize service worker and wait for it to be ready
 * Returns a promise that resolves when SW is controlling the page
 */
export async function initServiceWorker(options: InitOptions = {}): Promise<void> {
  const isTestMode = !!import.meta.env.VITE_TEST_MODE;

  if (isTestMode && 'serviceWorker' in navigator) {
    try {
      const registrations = await navigator.serviceWorker.getRegistrations();
      await Promise.all(registrations.map(reg => reg.unregister().catch(() => {})));
    } catch (err) {
      console.warn('[SW] Failed to unregister service workers in test mode:', err);
    }
    try {
      if ('caches' in window) {
        const keys = await caches.keys();
        await Promise.all(keys.map(key => caches.delete(key).catch(() => {})));
      }
    } catch (err) {
      console.warn('[SW] Failed to clear caches in test mode:', err);
    }
  }

  // Skip service worker when local htree server is available (Tauri host or desktop app)
  if (isTauri() || getHtreePrefix()) {
    console.log('[SW] Skipping service worker (native htree server)');
    setupSwFileHandler();
    return;
  }

  // Register service worker
  const updateSW = registerSW({
    immediate: true,
    onRegisteredSW(swUrl) {
      console.log('[SW] Registered:', swUrl);
    },
    onNeedRefresh() {
      console.log('[SW] Update available, activating...');
      if (!isTestMode) {
        updateSW(true);
      }
    },
    onRegisterError(error) {
      console.error('[SW] Registration error:', error);
    },
  });

  // Setup file request handler - listens for SW file requests
  setupSwFileHandler();

  if (!('serviceWorker' in navigator)) {
    return;
  }

  // Wait for SW to be active and controlling this page
  if (!navigator.serviceWorker.controller) {
    console.log('[SW] Waiting for controller...');

    // Wait for SW to be ready (active)
    await navigator.serviceWorker.ready;

    // If still no controller after SW is ready, wait for controllerchange or reload
    if (!navigator.serviceWorker.controller) {
      const gotController = await Promise.race([
        new Promise<boolean>((resolve) => {
          navigator.serviceWorker.addEventListener('controllerchange', () => {
            console.log('[SW] Controller now active');
            resolve(true);
          }, { once: true });
        }),
        // Small timeout just in case controllerchange already fired
        new Promise<boolean>((resolve) => setTimeout(() => resolve(false), 100)),
      ]);

      // If no controller after SW is ready, reload to let SW take control
      if (!gotController && !navigator.serviceWorker.controller) {
        console.log('[SW] No controller after SW ready, reloading...');
        window.location.reload();
        // Return a never-resolving promise since we're reloading
        return new Promise(() => {});
      }
    }
  }

  // Reload once on controller change to ensure fresh assets after SW update.
  navigator.serviceWorker.addEventListener('controllerchange', () => {
    const reloadKey = 'sw-reload-on-update';
    if (!sessionStorage.getItem(reloadKey)) {
      sessionStorage.setItem(reloadKey, '1');
      console.log('[SW] Controller changed, reloading for fresh assets...');
      if (!isTestMode) {
        window.location.reload();
      }
    }
  }, { once: true });

  // Check if cross-origin isolation is required (for SharedArrayBuffer/FFmpeg)
  if (options.requireCrossOriginIsolation) {
    const coiReloadKey = 'coi-reload-attempted';
    if (navigator.serviceWorker.controller && !self.crossOriginIsolated) {
      if (!sessionStorage.getItem(coiReloadKey)) {
        sessionStorage.setItem(coiReloadKey, '1');
        console.log('[SW] Not cross-origin isolated, reloading for COOP/COEP headers...');
        window.location.reload();
        return new Promise(() => {});
      } else {
        console.log('[SW] Cross-origin isolation not available after reload - FFmpeg transcoding disabled');
      }
    } else if (self.crossOriginIsolated) {
      sessionStorage.removeItem(coiReloadKey);
    }
  }
}
