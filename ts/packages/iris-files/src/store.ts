/**
 * Shared state and store instances using Svelte stores
 *
 * Storage architecture:
 * - WorkerStore: Primary storage, proxies to worker thread
 * - Worker owns: DexieStore (IndexedDB), Blossom fallback
 * - Main thread: UI coordination only
 */
import { writable, get } from 'svelte/store';
import { HashTree, LinkType } from 'hashtree';
import { getWorkerStore } from './stores/workerStore';
import { closeWorkerAdapter } from './workerAdapter';
import { getWorkerAdapter } from './lib/workerInit';

// Re-export LinkType for e2e tests that can't import 'hashtree' directly
export { LinkType };

// Export localStore - always uses WorkerStore (no fallback)
// Worker MUST be initialized before using storage
export const localStore = {
  async put(hash: Uint8Array, data: Uint8Array): Promise<boolean> {
    return getWorkerStore().put(hash, data);
  },
  async get(hash: Uint8Array): Promise<Uint8Array | null> {
    return getWorkerStore().get(hash);
  },
  async has(hash: Uint8Array): Promise<boolean> {
    return getWorkerStore().has(hash);
  },
  async delete(hash: Uint8Array): Promise<boolean> {
    return getWorkerStore().delete(hash);
  },
  async count(): Promise<number> {
    const adapter = getWorkerAdapter();
    if (!adapter) return 0;
    try {
      const stats = await adapter.getStorageStats();
      return stats.items;
    } catch {
      return 0;
    }
  },
  async totalBytes(): Promise<number> {
    const adapter = getWorkerAdapter();
    if (!adapter) return 0;
    try {
      const stats = await adapter.getStorageStats();
      return stats.bytes;
    } catch {
      return 0;
    }
  },
};

// HashTree instance - uses localStore which routes to worker
const _tree = new HashTree({ store: localStore });

// Getter for tree - always returns current instance
export function getTree(): HashTree {
  return _tree;
}

// Storage stats
export interface StorageStats {
  items: number;
  bytes: number;
}

// Peer info for connectivity indicator
export interface PeerInfo {
  peerId: string;
  pubkey: string;
  state: 'connected' | 'disconnected';
  pool: 'follows' | 'others';
  bytesSent: number;
  bytesReceived: number;
}

// Detailed peer stats for getStats()
export interface DetailedPeerStats {
  peerId: string;
  pubkey: string;
  connected: boolean;
  pool: 'follows' | 'other';
  requestsSent: number;
  requestsReceived: number;
  responsesSent: number;
  responsesReceived: number;
  bytesSent: number;
  bytesReceived: number;
}

// App state store interface (simplified - WebRTC stats come from worker)
interface AppState {
  // Storage stats
  stats: StorageStats;
  // WebRTC peer count (from worker)
  peerCount: number;
  // Peer list for connectivity indicator
  peers: PeerInfo[];
}

// Create Svelte store for app state
function createAppStore() {
  const { subscribe, update } = writable<AppState>({
    stats: { items: 0, bytes: 0 },
    peerCount: 0,
    peers: [],
  });

  return {
    subscribe,

    setStats: (stats: StorageStats) => {
      update(state => ({ ...state, stats }));
    },

    setPeerCount: (count: number) => {
      update(state => ({ ...state, peerCount: count }));
    },

    setPeers: (peers: PeerInfo[]) => {
      update(state => ({ ...state, peers, peerCount: peers.filter(p => p.state === 'connected').length }));
    },

    // Get current state synchronously (for compatibility)
    getState: (): AppState => get(appStore),
  };
}

export const appStore = createAppStore();

// Legacy compatibility alias
export const useAppStore = appStore;

// Expose for debugging in tests
if (typeof window !== 'undefined') {
  const win = window as Window & { __appStore?: typeof appStore; __localStore?: typeof localStore };
  win.__appStore = appStore;
  win.__localStore = localStore;
}

// Format bytes
export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(1)} GB`;
}

// Format bandwidth (bytes per second)
export function formatBandwidth(bytesPerSecond: number): string {
  if (bytesPerSecond < 1) return '0 B/s';
  if (bytesPerSecond < 1024) return `${Math.round(bytesPerSecond)} B/s`;
  if (bytesPerSecond < 1024 * 1024) return `${(bytesPerSecond / 1024).toFixed(1)} KB/s`;
  return `${(bytesPerSecond / 1024 / 1024).toFixed(1)} MB/s`;
}

// Update storage stats from IDB
export async function updateStorageStats(): Promise<void> {
  try {
    const items = await localStore.count();
    const bytes = await localStore.totalBytes();
    appStore.setStats({ items, bytes });
  } catch {
    // Ignore errors
  }
}

// Decode content as text
export function decodeAsText(data: Uint8Array): string | null {
  if (data.length === 0) return '';
  try {
    const text = new TextDecoder('utf-8', { fatal: true }).decode(data);
    if (!/[\x00-\x08\x0E-\x1F]/.test(text.slice(0, 1000))) {
      return text;
    }
  } catch {}
  return null;
}

// Stub functions for compatibility - WebRTC is now in worker
// These are called from nostr.ts on login/logout

export function initWebRTC(): void {
  // WebRTC is handled by worker - nothing to do here
  console.log('[Store] WebRTC initialization delegated to worker');
}

export function stopWebRTC(): void {
  // Close worker (clears identity, stops WebRTC, Nostr)
  closeWorkerAdapter();
}

// Legacy exports for compatibility - WebRTC is now in worker
// Create a proxy object that forwards to worker for test compatibility
const webrtcStoreProxy = {
  getPeers: () => get(appStore).peers.map(p => ({
    ...p,
    isConnected: p.state === 'connected',
  })),
  getConnectedCount: () => get(appStore).peers.filter(p => p.state === 'connected').length,
  get: async (hash: string) => {
    const adapter = getWorkerAdapter();
    if (!adapter) return null;
    return adapter.get(hash);
  },
  setPoolConfig: (_config: unknown) => {
    // Pool config is managed by worker, no-op for now
  },
  setRelays: (_relays: string[]) => {
    // Relays are managed by worker, no-op for now
  },
  sendHello: () => {
    const adapter = getWorkerAdapter();
    adapter?.sendHello();
  },
  isFollowing: async (pubkey: string): Promise<boolean> => {
    const adapter = getWorkerAdapter();
    if (!adapter) return false;
    // Get current user's pubkey
    const myPubkey = get(appStore).pubkey;
    if (!myPubkey) return false;
    try {
      return await adapter.isFollowing(myPubkey, pubkey);
    } catch {
      return false;
    }
  },
  getStats: async () => {
    const adapter = getWorkerAdapter();
    if (!adapter) {
      return {
        aggregate: { requestsSent: 0, requestsReceived: 0, responsesSent: 0, responsesReceived: 0, bytesSent: 0, bytesReceived: 0 },
        perPeer: new Map(),
      };
    }

    const peerStats = await adapter.getPeerStats();

    // Aggregate stats from all peers
    const aggregate = {
      requestsSent: 0,
      requestsReceived: 0,
      responsesSent: 0,
      responsesReceived: 0,
      bytesSent: 0,
      bytesReceived: 0,
    };

    const perPeer = new Map<string, DetailedPeerStats>();

    for (const p of peerStats) {
      aggregate.requestsSent += p.requestsSent;
      aggregate.requestsReceived += p.requestsReceived;
      aggregate.responsesSent += p.responsesSent;
      aggregate.responsesReceived += p.responsesReceived;
      aggregate.bytesSent += p.bytesSent;
      aggregate.bytesReceived += p.bytesReceived;

      perPeer.set(p.peerId, {
        peerId: p.peerId,
        pubkey: p.pubkey,
        connected: p.connected,
        pool: 'other', // Worker doesn't track pool in stats
        requestsSent: p.requestsSent,
        requestsReceived: p.requestsReceived,
        responsesSent: p.responsesSent,
        responsesReceived: p.responsesReceived,
        bytesSent: p.bytesSent,
        bytesReceived: p.bytesReceived,
      });
    }

    return { aggregate, perPeer };
  },
};
export const webrtcStore = webrtcStoreProxy;
export function getWebRTCStore() { return webrtcStoreProxy; }

export async function blockPeer(pubkey: string): Promise<void> {
  // Import dynamically to avoid circular dependency
  const { settingsStore } = await import('./stores/settings');
  settingsStore.blockPeer(pubkey);
  // Disconnect the peer via worker
  const adapter = getWorkerAdapter();
  if (adapter) {
    try {
      await adapter.blockPeer(pubkey);
    } catch {
      // Worker may not support blocking yet
    }
  }
}

export async function unblockPeer(pubkey: string): Promise<void> {
  // Import dynamically to avoid circular dependency
  const { settingsStore } = await import('./stores/settings');
  settingsStore.unblockPeer(pubkey);
}

// Expose webrtcStore on window for test compatibility
// Use defineProperty to allow testHelpers to override if needed
if (typeof window !== 'undefined' && !('webrtcStore' in window)) {
  Object.defineProperty(window, 'webrtcStore', {
    value: webrtcStoreProxy,
    writable: true,
    configurable: true,
  });
}

// Refresh WebRTC stats from worker
export async function refreshWebRTCStats(): Promise<void> {
  const adapter = getWorkerAdapter();
  if (!adapter) return;

  try {
    const stats = await adapter.getPeerStats();
    // Use pool from worker stats directly - worker has authoritative follows set
    const peers: PeerInfo[] = stats.map(p => ({
      peerId: p.peerId,
      pubkey: p.pubkey,
      state: p.connected ? 'connected' : 'disconnected',
      pool: p.pool === 'follows' ? 'follows' : 'others',
      bytesSent: p.bytesSent,
      bytesReceived: p.bytesReceived,
    }));
    appStore.setPeers(peers);
  } catch {
    // Worker not ready or other error
  }
}

export function getLifetimeStats() {
  return { bytesSent: 0, bytesReceived: 0, bytesForwarded: 0 };
}
