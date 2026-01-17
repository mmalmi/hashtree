/**
 * Media Streaming Setup
 *
 * Sets up the MessageChannel between service worker and hashtree worker
 * to enable media streaming via /media/{cid}/{path} URLs.
 */

import { getWorkerAdapter } from './workerInit';

let isSetup = false;
let setupPromise: Promise<boolean> | null = null;

/**
 * Setup media streaming by connecting service worker to hashtree worker
 *
 * This creates a MessageChannel and:
 * 1. Sends one port to the service worker
 * 2. Sends the other port to the hashtree worker
 *
 * The service worker can then request media data directly from the worker.
 */
export async function setupMediaStreaming(): Promise<boolean> {
  if (isSetup) return true;
  if (setupPromise) return setupPromise;

  setupPromise = doSetup();
  return setupPromise;
}

async function doSetup(): Promise<boolean> {
  // Check service worker support
  if (!('serviceWorker' in navigator)) {
    console.warn('[MediaStreaming] Service workers not supported');
    return false;
  }

  try {
    // Get current registration status
    const currentReg = await navigator.serviceWorker.getRegistration();
    console.log('[MediaStreaming] Current registration:', currentReg?.scope, 'active:', !!currentReg?.active);

    // If no registration, wait for it with retries
    let registration: ServiceWorkerRegistration | null = null;
    for (let i = 0; i < 10; i++) {
      registration = await navigator.serviceWorker.getRegistration();
      if (registration?.active) {
        break;
      }
      console.log('[MediaStreaming] Waiting for SW activation, attempt', i + 1);
      await new Promise((r) => setTimeout(r, 500));
    }

    if (!registration?.active) {
      // Try navigator.serviceWorker.ready as last resort
      const timeoutPromise = new Promise<null>((resolve) => {
        setTimeout(() => resolve(null), 3000);
      });
      registration = (await Promise.race([
        navigator.serviceWorker.ready,
        timeoutPromise,
      ])) as ServiceWorkerRegistration | null;
    }

    if (!registration?.active) {
      console.warn('[MediaStreaming] No active service worker after retries');
      return false;
    }

    console.log('[MediaStreaming] SW active:', registration.scope);

    // Get the worker adapter
    const adapter = getWorkerAdapter();
    if (!adapter) {
      console.warn('[MediaStreaming] Worker adapter not initialized');
      return false;
    }

    // Create a MessageChannel
    const channel = new MessageChannel();

    // Send one port to the service worker
    registration.active.postMessage(
      { type: 'REGISTER_WORKER_PORT', port: channel.port1 },
      [channel.port1]
    );

    // Send the other port to the hashtree worker
    adapter.registerMediaPort(channel.port2);

    isSetup = true;
    console.log('[MediaStreaming] Setup complete');
    return true;
  } catch (error) {
    console.error('[MediaStreaming] Setup failed:', error);
    return false;
  }
}

/**
 * Check if media streaming is set up
 */
export function isMediaStreamingSetup(): boolean {
  return isSetup;
}

/**
 * Reset media streaming (for testing/cleanup)
 */
export function resetMediaStreaming(): void {
  isSetup = false;
  setupPromise = null;
}
