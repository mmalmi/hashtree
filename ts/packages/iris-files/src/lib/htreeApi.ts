/**
 * window.htree API
 *
 * Provides a unified API for guest apps running inside Iris (Tauri or web).
 * Guest apps can check for window.htree to:
 * - Skip duplicate login/wallet UI (use host's identity)
 * - Use correct media URLs (htreeBaseUrl for /htree paths)
 * - Use host's NIP-07 implementation for signing
 * - Know which relays are connected
 */

import { get } from 'svelte/store';
import { isTauri } from '../tauri';
import { nostrStore } from '../nostr/store';
import { getHtreePrefix, initHtreePrefix, onHtreePrefixReady } from './mediaUrl';

export interface HtreeAPI {
  /** API version for compatibility checks */
  version: string;

  /** Whether running in Tauri desktop app */
  isTauri: boolean;

  /**
   * Base URL for /htree/* paths
   * - Web (SW): "" (empty string, use relative paths)
   * - Tauri: "http://127.0.0.1:PORT" (absolute URL to local HTTP server)
   *
   * Usage: `${window.htree.htreeBaseUrl}/htree/npub1.../treeName/file.mp4`
   */
  htreeBaseUrl: string;

  /** Current user's npub (null if not logged in) */
  npub: string | null;

  /** Current user's hex pubkey (null if not logged in) */
  pubkey: string | null;

  /** Whether user is currently logged in */
  isLoggedIn: boolean;

  /** Connected relay URLs */
  relays: string[];

  /**
   * NIP-07 compatible interface for signing
   * Proxies to host's nostr implementation (Tauri native or browser extension)
   */
  nostr: {
    getPublicKey: () => Promise<string>;
    signEvent: (event: { kind: number; content: string; tags: string[][]; created_at: number }) => Promise<{
      id: string;
      pubkey: string;
      created_at: number;
      kind: number;
      tags: string[][];
      content: string;
      sig: string;
    }>;
  } | null;

  /**
   * Detect if a local Nostr relay is running
   * Returns the relay URL if found, null otherwise
   */
  detectLocalRelay: () => Promise<string | null>;

  /**
   * Subscribe to state changes
   * Returns unsubscribe function
   */
  subscribe: (callback: (api: HtreeAPI) => void) => () => void;
}

// Subscribers for state changes
const subscribers: Set<(api: HtreeAPI) => void> = new Set();

/**
 * Detect if a local Nostr relay is running on common ports
 */
async function detectLocalRelay(): Promise<string | null> {
  const candidates = [
    'ws://127.0.0.1:7777',
    'ws://localhost:7777',
    'ws://127.0.0.1:4869', // nostr-relay-sqlite default
    'ws://127.0.0.1:8080',
  ];

  for (const url of candidates) {
    try {
      const ws = new WebSocket(url);
      const connected = await Promise.race([
        new Promise<boolean>((resolve) => {
          ws.onopen = () => resolve(true);
          ws.onerror = () => resolve(false);
        }),
        new Promise<boolean>((resolve) => setTimeout(() => resolve(false), 500)),
      ]);
      ws.close();
      if (connected) {
        console.log('[htree] Local relay detected:', url);
        return url;
      }
    } catch {
      // Ignore connection errors
    }
  }

  return null;
}

/**
 * Build the current API state
 */
function buildApi(): HtreeAPI {
  const state = get(nostrStore);

  return {
    version: '1.0.0',
    isTauri: isTauri(),
    htreeBaseUrl: getHtreePrefix(),
    npub: state.npub,
    pubkey: state.pubkey,
    isLoggedIn: state.isLoggedIn,
    relays: state.relays,
    nostr: typeof window !== 'undefined' && window.nostr ? window.nostr : null,
    detectLocalRelay,
    subscribe: (callback: (api: HtreeAPI) => void) => {
      subscribers.add(callback);
      return () => subscribers.delete(callback);
    },
  };
}

/**
 * Notify all subscribers of state changes
 */
function notifySubscribers(): void {
  const api = buildApi();
  subscribers.forEach((cb) => {
    try {
      cb(api);
    } catch (e) {
      console.error('[htree] Subscriber error:', e);
    }
  });
}

/**
 * Initialize the window.htree API
 * Should be called early in app initialization (prefix resolves in background).
 */
export async function initHtreeApi(): Promise<void> {
  // Initialize prefix asynchronously; update window.htree when ready.
  void initHtreePrefix();

  // Set initial API on window
  const api = buildApi();

  // Use Object.defineProperty for better control
  Object.defineProperty(window, 'htree', {
    value: api,
    writable: true,
    configurable: true,
    enumerable: true,
  });

  console.log('[htree] API initialized:', {
    version: api.version,
    isTauri: api.isTauri,
    htreeBaseUrl: api.htreeBaseUrl,
    isLoggedIn: api.isLoggedIn,
    npub: api.npub?.slice(0, 20) + '...',
  });

  // Subscribe to nostr store changes and update window.htree
  nostrStore.subscribe(() => {
    if (typeof window !== 'undefined' && window.htree) {
      const newApi = buildApi();
      Object.assign(window.htree, newApi);
      notifySubscribers();
    }
  });

  onHtreePrefixReady(() => {
    if (typeof window !== 'undefined' && window.htree) {
      const newApi = buildApi();
      Object.assign(window.htree, newApi);
      notifySubscribers();
    }
  });
}

/**
 * Get the current htree API (for use within the app)
 */
export function getHtreeApi(): HtreeAPI {
  return buildApi();
}

// Type augmentation for window.htree
declare global {
  interface Window {
    htree?: HtreeAPI;
  }
}
