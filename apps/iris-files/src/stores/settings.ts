/**
 * Settings store with Dexie persistence (Svelte version)
 */
import { writable, get } from 'svelte/store';
import Dexie, { type Table } from 'dexie';

// Pool configuration
export interface PoolSettings {
  followsMax: number;
  followsSatisfied: number;
  otherMax: number;
  otherSatisfied: number;
  // Header display settings
  showConnectivity: boolean;
  showBandwidth: boolean;
}

// Gitignore behavior for directory uploads
export type GitignoreBehavior = 'ask' | 'always' | 'never';

export interface UploadSettings {
  /** How to handle .gitignore files in directory uploads */
  gitignoreBehavior: GitignoreBehavior;
}

/**
 * Test mode configuration
 * When VITE_TEST_MODE is set, the app uses test-specific settings:
 * - Local relay only (from VITE_TEST_RELAY)
 * - No Blossom HTTP fallback
 * - Others pool disabled (prevents WebRTC cross-talk between parallel tests)
 */
const isTestMode = !!import.meta.env.VITE_TEST_MODE;
const testRelay = import.meta.env.VITE_TEST_RELAY as string | undefined;
const testRelayOverride = typeof window !== 'undefined'
  ? (window as Window & { __testRelayUrl?: string }).__testRelayUrl
  : undefined;
const effectiveTestRelay = testRelayOverride ?? testRelay;

// Default pool settings
export const DEFAULT_POOL_SETTINGS: PoolSettings = {
  followsMax: 20,
  followsSatisfied: 10,
  // Disable others pool in test mode to prevent WebRTC interference between parallel tests
  otherMax: isTestMode ? 0 : 16,
  otherSatisfied: isTestMode ? 0 : 8,
  // Header display settings
  showConnectivity: true,
  showBandwidth: false,
};

// Default upload settings
export const DEFAULT_UPLOAD_SETTINGS: UploadSettings = {
  gitignoreBehavior: 'ask',
};

// Editor settings
export interface EditorSettings {
  /** Whether to auto-save changes while editing */
  autoSave: boolean;
}

// Default editor settings
export const DEFAULT_EDITOR_SETTINGS: EditorSettings = {
  autoSave: true,
};

// Video player settings
export interface VideoSettings {
  /** Theater mode (full width) vs normal mode (sidebar visible) */
  theaterMode: boolean;
  /** Volume level (0-1) */
  volume: number;
  /** Muted state */
  muted: boolean;
}

// Default video settings
export const DEFAULT_VIDEO_SETTINGS: VideoSettings = {
  theaterMode: false,
  volume: 1,
  muted: false,
};

// Imgproxy settings
export interface ImgproxySettings {
  /** Whether imgproxy is enabled */
  enabled: boolean;
  /** Imgproxy server URL */
  url: string;
  /** HMAC key (hex) */
  key: string;
  /** HMAC salt (hex) */
  salt: string;
}

// Default imgproxy settings (uses iris imgproxy - same as iris-client)
export const DEFAULT_IMGPROXY_SETTINGS: ImgproxySettings = {
  enabled: true,
  url: 'https://imgproxy.iris.to',
  key: 'f66233cb160ea07078ff28099bfa3e3e654bc10aa4a745e12176c433d79b8996',
  salt: '5e608e60945dcd2a787e8465d76ba34149894765061d39287609fb9d776caa0c',
};

// Storage settings for IndexedDB quota management
export interface StorageSettings {
  /** Maximum bytes to store in IndexedDB (default 1GB) */
  maxBytes: number;
}

// Default storage settings - 1GB limit
export const DEFAULT_STORAGE_SETTINGS: StorageSettings = {
  maxBytes: 1024 * 1024 * 1024, // 1GB
};

// Blossom server configuration
export interface BlossomServerConfig {
  url: string;
  read: boolean;
  write: boolean;
}

// Network settings for relays and blossom servers
export interface NetworkSettings {
  /** Nostr relay URLs */
  relays: string[];
  /** Blossom server configurations */
  blossomServers: BlossomServerConfig[];
  /** Whether negentropy sync is enabled */
  negentropyEnabled: boolean;
}

// Default network settings - test mode uses local relay only and no Blossom
export const DEFAULT_NETWORK_SETTINGS: NetworkSettings = {
  relays: isTestMode && effectiveTestRelay
    ? [effectiveTestRelay]
    : [
        'wss://relay.damus.io',
        'wss://relay.primal.net',
        'wss://nos.lol',
        'wss://relay.nostr.band',
        'wss://relay.snort.social',
        'wss://temp.iris.to',
      ],
  blossomServers: isTestMode
    ? []
    : [
        { url: 'https://upload.iris.to', read: false, write: true },
        { url: 'https://cdn.iris.to', read: true, write: false },
        { url: 'https://hashtree.iris.to', read: true, write: false },
      ],
  negentropyEnabled: false,
};

const LOCAL_HOSTNAMES = new Set(['localhost', '127.0.0.1', '[::1]']);

function isLocalHostname(hostname: string | undefined): boolean {
  return !!hostname && LOCAL_HOSTNAMES.has(hostname);
}

function isLocalRelay(url: string): boolean {
  try {
    const parsed = new URL(url);
    return isLocalHostname(parsed.hostname);
  } catch {
    return false;
  }
}

function shouldApplyProductionFallback(): boolean {
  if (isTestMode) return false;
  if (typeof window === 'undefined') return false;
  if ('__TAURI_INTERNALS__' in window || '__TAURI__' in window) return false;
  return !isLocalHostname(window.location.hostname);
}

function applyProductionNetworkFallback(network: NetworkSettings): NetworkSettings {
  if (!shouldApplyProductionFallback()) return network;

  const relays = network.relays ?? [];
  const hasPublicRelay = relays.some(relay => !isLocalRelay(relay));
  const effectiveRelays = hasPublicRelay ? relays : DEFAULT_NETWORK_SETTINGS.relays;

  const blossomServers = network.blossomServers ?? [];
  const effectiveBlossom = blossomServers.length > 0 ? blossomServers : DEFAULT_NETWORK_SETTINGS.blossomServers;

  if (!hasPublicRelay || blossomServers.length === 0) {
    console.warn('[settings] Using default network fallbacks for this session');
  }

  return {
    ...network,
    relays: effectiveRelays,
    blossomServers: effectiveBlossom,
  };
}

// Dexie database for settings persistence
class SettingsDB extends Dexie {
  settings!: Table<{ key: string; value: unknown }>;

  constructor() {
    super('hashtree-settings');
    this.version(1).stores({
      settings: '&key',
    });
  }
}

const db = new SettingsDB();

export interface SettingsState {
  // Legacy settings (kept for compatibility)
  appearance: Record<string, unknown>;
  content: Record<string, unknown>;
  notifications: Record<string, unknown>;
  desktop: Record<string, unknown>;
  debug: Record<string, unknown>;
  legal: Record<string, unknown>;

  // Imgproxy settings
  imgproxy: ImgproxySettings;

  // Pool settings
  pools: PoolSettings;
  poolsLoaded: boolean;

  // Upload settings
  upload: UploadSettings;

  // Editor settings
  editor: EditorSettings;

  // Video settings
  video: VideoSettings;

  // Network settings
  network: NetworkSettings;
  networkLoaded: boolean;

  // Storage settings
  storage: StorageSettings;
  storageLoaded: boolean;

  // Blocked peers (pubkeys)
  blockedPeers: string[];
}

function createSettingsStore() {
  const { subscribe, update } = writable<SettingsState>({
    // Legacy settings
    appearance: {},
    content: {},
    notifications: {},
    desktop: {},
    debug: {},
    legal: {},

    // Imgproxy settings
    imgproxy: DEFAULT_IMGPROXY_SETTINGS,

    // Pool settings
    pools: DEFAULT_POOL_SETTINGS,
    poolsLoaded: false,

    // Upload settings
    upload: DEFAULT_UPLOAD_SETTINGS,

    // Editor settings
    editor: DEFAULT_EDITOR_SETTINGS,

    // Video settings (loaded from localStorage synchronously)
    video: (() => {
      try {
        const saved = localStorage.getItem('video-settings');
        if (saved) {
          const parsed = JSON.parse(saved);
          return {
            theaterMode: parsed.theaterMode ?? DEFAULT_VIDEO_SETTINGS.theaterMode,
            volume: parsed.volume ?? DEFAULT_VIDEO_SETTINGS.volume,
            muted: parsed.muted ?? DEFAULT_VIDEO_SETTINGS.muted,
          };
        }
      } catch {}
      return DEFAULT_VIDEO_SETTINGS;
    })(),

    // Network settings
    network: DEFAULT_NETWORK_SETTINGS,
    networkLoaded: false,

    // Storage settings
    storage: DEFAULT_STORAGE_SETTINGS,
    storageLoaded: false,

    // Blocked peers
    blockedPeers: [],
  });

  return {
    subscribe,

    setPoolSettings: (pools: Partial<PoolSettings>) => {
      update(state => {
        const updated = { ...state.pools, ...pools };
        // Persist to Dexie
        db.settings.put({ key: 'pools', value: updated }).catch(console.error);
        return { ...state, pools: updated };
      });
    },

    resetPoolSettings: () => {
      update(state => {
        db.settings.put({ key: 'pools', value: DEFAULT_POOL_SETTINGS }).catch(console.error);
        return { ...state, pools: DEFAULT_POOL_SETTINGS };
      });
    },

    setUploadSettings: (upload: Partial<UploadSettings>) => {
      update(state => {
        const updated = { ...state.upload, ...upload };
        db.settings.put({ key: 'upload', value: updated }).catch(console.error);
        return { ...state, upload: updated };
      });
    },

    setEditorSettings: (editor: Partial<EditorSettings>) => {
      update(state => {
        const updated = { ...state.editor, ...editor };
        db.settings.put({ key: 'editor', value: updated }).catch(console.error);
        return { ...state, editor: updated };
      });
    },

    setVideoSettings: (video: Partial<VideoSettings>) => {
      update(state => {
        const updated = { ...state.video, ...video };
        try {
          localStorage.setItem('video-settings', JSON.stringify(updated));
        } catch {}
        return { ...state, video: updated };
      });
    },

    setImgproxySettings: (imgproxy: Partial<ImgproxySettings>) => {
      update(state => {
        const updated = { ...state.imgproxy, ...imgproxy };
        db.settings.put({ key: 'imgproxy', value: updated }).catch(console.error);
        return { ...state, imgproxy: updated };
      });
    },

    resetImgproxySettings: () => {
      update(state => {
        db.settings.put({ key: 'imgproxy', value: DEFAULT_IMGPROXY_SETTINGS }).catch(console.error);
        return { ...state, imgproxy: DEFAULT_IMGPROXY_SETTINGS };
      });
    },

    setNetworkSettings: (network: Partial<NetworkSettings>) => {
      update(state => {
        const updated = { ...state.network, ...network };
        db.settings.put({ key: 'network', value: updated }).catch(console.error);
        return { ...state, network: updated };
      });
    },

    resetNetworkSettings: () => {
      update(state => {
        db.settings.put({ key: 'network', value: DEFAULT_NETWORK_SETTINGS }).catch(console.error);
        return { ...state, network: DEFAULT_NETWORK_SETTINGS };
      });
    },

    setStorageSettings: (storage: Partial<StorageSettings>) => {
      update(state => {
        const updated = { ...state.storage, ...storage };
        db.settings.put({ key: 'storage', value: updated }).catch(console.error);
        return { ...state, storage: updated };
      });
    },

    resetStorageSettings: () => {
      update(state => {
        db.settings.put({ key: 'storage', value: DEFAULT_STORAGE_SETTINGS }).catch(console.error);
        return { ...state, storage: DEFAULT_STORAGE_SETTINGS };
      });
    },

    blockPeer: (pubkey: string) => {
      update(state => {
        if (state.blockedPeers.includes(pubkey)) return state;
        const updated = [...state.blockedPeers, pubkey];
        db.settings.put({ key: 'blockedPeers', value: updated }).catch(console.error);
        return { ...state, blockedPeers: updated };
      });
    },

    unblockPeer: (pubkey: string) => {
      update(state => {
        const updated = state.blockedPeers.filter(p => p !== pubkey);
        db.settings.put({ key: 'blockedPeers', value: updated }).catch(console.error);
        return { ...state, blockedPeers: updated };
      });
    },

    isPeerBlocked: (pubkey: string): boolean => {
      return get(settingsStore).blockedPeers.includes(pubkey);
    },

    // Get current state synchronously
    getState: (): SettingsState => get(settingsStore),

    // Set state directly
    setState: (newState: Partial<SettingsState>) => {
      update(state => ({ ...state, ...newState }));
    },
  };
}

export const settingsStore = createSettingsStore();

// Legacy compatibility alias
export const useSettingsStore = settingsStore;

// Load settings from Dexie on startup
async function loadSettings() {
  try {
    const [poolsRow, uploadRow, editorRow, videoRow, networkRow, imgproxyRow, storageRow, blockedPeersRow] = await Promise.all([
      db.settings.get('pools'),
      db.settings.get('upload'),
      db.settings.get('editor'),
      db.settings.get('video'),
      db.settings.get('network'),
      db.settings.get('imgproxy'),
      db.settings.get('storage'),
      db.settings.get('blockedPeers'),
    ]);

    const updates: Partial<SettingsState> = { poolsLoaded: true, networkLoaded: true, storageLoaded: true };

    if (poolsRow?.value) {
      const pools = poolsRow.value as PoolSettings;
      updates.pools = {
        followsMax: pools.followsMax ?? DEFAULT_POOL_SETTINGS.followsMax,
        followsSatisfied: pools.followsSatisfied ?? DEFAULT_POOL_SETTINGS.followsSatisfied,
        otherMax: pools.otherMax ?? DEFAULT_POOL_SETTINGS.otherMax,
        otherSatisfied: pools.otherSatisfied ?? DEFAULT_POOL_SETTINGS.otherSatisfied,
        showConnectivity: pools.showConnectivity ?? DEFAULT_POOL_SETTINGS.showConnectivity,
        showBandwidth: pools.showBandwidth ?? DEFAULT_POOL_SETTINGS.showBandwidth,
      };
    }

    if (uploadRow?.value) {
      const upload = uploadRow.value as UploadSettings;
      updates.upload = {
        gitignoreBehavior: upload.gitignoreBehavior ?? DEFAULT_UPLOAD_SETTINGS.gitignoreBehavior,
      };
    }

    if (editorRow?.value) {
      const editor = editorRow.value as EditorSettings;
      updates.editor = {
        autoSave: editor.autoSave ?? DEFAULT_EDITOR_SETTINGS.autoSave,
      };
    }

    if (videoRow?.value) {
      const video = videoRow.value as VideoSettings;
      updates.video = {
        theaterMode: video.theaterMode ?? DEFAULT_VIDEO_SETTINGS.theaterMode,
      };
    }

    if (networkRow?.value) {
      const network = networkRow.value as NetworkSettings;
      // Handle backwards compatibility: convert old string[] format to BlossomServerConfig[]
      let blossomServers = DEFAULT_NETWORK_SETTINGS.blossomServers;
      if (network.blossomServers && Array.isArray(network.blossomServers)) {
        blossomServers = network.blossomServers.map(s =>
          typeof s === 'string' ? { url: s, read: true, write: false } : { ...s, read: s.read ?? true }
        );
      }
      const nextNetwork = {
        relays: network.relays ?? DEFAULT_NETWORK_SETTINGS.relays,
        blossomServers,
        negentropyEnabled: network.negentropyEnabled ?? DEFAULT_NETWORK_SETTINGS.negentropyEnabled,
      };
      updates.network = isTestMode ? nextNetwork : applyProductionNetworkFallback(nextNetwork);
    } else if (isTestMode) {
      updates.network = DEFAULT_NETWORK_SETTINGS;
    }

    if (imgproxyRow?.value) {
      const imgproxy = imgproxyRow.value as ImgproxySettings;
      updates.imgproxy = {
        enabled: imgproxy.enabled ?? DEFAULT_IMGPROXY_SETTINGS.enabled,
        url: imgproxy.url ?? DEFAULT_IMGPROXY_SETTINGS.url,
        key: imgproxy.key ?? DEFAULT_IMGPROXY_SETTINGS.key,
        salt: imgproxy.salt ?? DEFAULT_IMGPROXY_SETTINGS.salt,
      };
    }

    if (storageRow?.value) {
      const storage = storageRow.value as StorageSettings;
      updates.storage = {
        maxBytes: storage.maxBytes ?? DEFAULT_STORAGE_SETTINGS.maxBytes,
      };
    }

    if (blockedPeersRow?.value && Array.isArray(blockedPeersRow.value)) {
      updates.blockedPeers = blockedPeersRow.value as string[];
    }

    settingsStore.setState(updates);
  } catch (err) {
    console.error('[settings] error loading:', err);
    settingsStore.setState({ poolsLoaded: true, networkLoaded: true, storageLoaded: true });
  }
}

// Promise that resolves when settings are loaded
let settingsLoadedResolve: (() => void) | null = null;
const settingsLoadedPromise = new Promise<void>((resolve) => {
  settingsLoadedResolve = resolve;
});

// Initialize on module load
loadSettings().then(() => {
  settingsLoadedResolve?.();
});

/**
 * Wait for settings to be loaded from IndexedDB.
 * Use this before initializing components that need correct settings from the start.
 */
export function waitForSettingsLoaded(): Promise<void> {
  return settingsLoadedPromise;
}

// Expose for e2e tests to configure settings without module duplication issues
// Track all store instances to handle Vite module duplication
if (typeof window !== 'undefined') {
  type SettingsStoreInstance = { setNetworkSettings: (settings: Partial<NetworkSettings>) => void };
  const win = window as unknown as {
    __settingsStoreInstances?: SettingsStoreInstance[];
    __configureBlossomServers?: (servers: BlossomServerConfig[]) => void;
  };

  // Register this store instance
  if (!win.__settingsStoreInstances) {
    win.__settingsStoreInstances = [];
  }
  win.__settingsStoreInstances.push(settingsStore);

  // Update all instances when called
  win.__configureBlossomServers = (servers: BlossomServerConfig[]) => {
    for (const store of win.__settingsStoreInstances || []) {
      store.setNetworkSettings({ blossomServers: servers });
    }
  };
}
