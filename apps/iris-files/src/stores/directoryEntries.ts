/**
 * Hook to fetch directory entries from tree
 * Simple - just reads from tree, no caching or global state
 * Supports both encrypted and public directories via CID
 * Svelte version using stores
 */
import { writable, get, type Readable } from 'svelte/store';
import { type CID, type TreeEntry, toHex, LinkType } from 'hashtree';
import { getTree } from '../store';
import { markFilesChanged } from './recentlyChanged';

// Sort entries: directories first, then alphabetically
function sortEntries(entries: TreeEntry[]): TreeEntry[] {
  return [...entries].sort((a, b) => {
    const aIsDir = a.type === LinkType.Dir;
    const bIsDir = b.type === LinkType.Dir;
    if (aIsDir !== bIsDir) return aIsDir ? -1 : 1;
    return a.name.localeCompare(b.name);
  });
}

export interface DirectoryEntriesState {
  entries: TreeEntry[];
  loading: boolean;
  isDirectory: boolean;
}

/**
 * Create a store for directory entries for a given CID
 */
export function createDirectoryEntriesStore(locationStore: Readable<CID | null>, trackChanges = false): Readable<DirectoryEntriesState> {
  const state = writable<DirectoryEntriesState>({
    entries: [],
    loading: false,
    isDirectory: true,
  });

  let prevHashKey: string | null = null;
  let prevEncKey: string | null = null;
  // Track previous entries by name -> CID hash to detect changes
  let prevEntryCids: Map<string, string> = new Map();

  // Subscribe to location changes
  locationStore.subscribe(async (location) => {
    const hashKey = location?.hash ? toHex(location.hash) : null;
    const encKey = location?.key ? toHex(location.key) : null;

    // Skip if no change
    if (hashKey === prevHashKey && encKey === prevEncKey) return;
    prevHashKey = hashKey;
    prevEncKey = encKey;

    if (!location || !hashKey) {
      state.set({ entries: [], loading: false, isDirectory: true });
      prevEntryCids.clear();
      return;
    }

    state.update(s => ({ ...s, loading: true }));
    const tree = getTree();

    try {
      let newEntries: TreeEntry[] = [];

      if (location.key) {
        // Encrypted - try to list directory with key
        newEntries = await tree.listDirectory(location);
      } else {
        // Public - first check if it's a directory
        const isDir = await tree.isDirectory(location);

        if (isDir) {
          newEntries = await tree.listDirectory(location);
        } else {
          state.set({ entries: [], loading: false, isDirectory: false });
          prevEntryCids.clear();
          return;
        }
      }

      // Detect changed files (CID changed for same filename)
      if (trackChanges && prevEntryCids.size > 0) {
        const changedFiles = new Set<string>();
        for (const entry of newEntries) {
          const prevCid = prevEntryCids.get(entry.name);
          const newCid = entry.cid?.hash ? toHex(entry.cid.hash) : null;
          if (prevCid && newCid && prevCid !== newCid) {
            changedFiles.add(entry.name);
          }
        }
        if (changedFiles.size > 0) {
          markFilesChanged(changedFiles);
        }
      }

      // Update prev entry CIDs for next comparison
      const newEntryCids = new Map<string, string>();
      for (const entry of newEntries) {
        if (entry.cid?.hash) {
          newEntryCids.set(entry.name, toHex(entry.cid.hash));
        }
      }

      // Only update state if entries actually changed (avoids unnecessary re-renders)
      const entriesChanged = newEntryCids.size !== prevEntryCids.size ||
        [...newEntryCids].some(([name, cid]) => prevEntryCids.get(name) !== cid);

      prevEntryCids = newEntryCids;

      if (entriesChanged) {
        state.set({ entries: sortEntries(newEntries), loading: false, isDirectory: true });
      } else {
        // Just clear loading state without changing entries
        state.update(s => s.loading ? { ...s, loading: false } : s);
      }
    } catch {
      state.set({ entries: [], loading: false, isDirectory: false });
      prevEntryCids.clear();
    }
  });

  return { subscribe: state.subscribe };
}

/**
 * Synchronous function to use directory entries (React-style hook compatibility)
 * For components that need to use the entries synchronously
 */
export function directoryEntries(location: CID | null): DirectoryEntriesState {
  const locationStore = writable(location);
  const store = createDirectoryEntriesStore(locationStore);
  return get(store);
}

// Import currentDirCidStore for global entries store
import { currentDirCidStore } from './currentDirHash';
import { routeStore } from './route';
import { nostrStore } from '../nostr';

/**
 * Create the global directory entries store with test helper
 */
function createGlobalDirectoryEntriesStore() {
  const state = writable<DirectoryEntriesState>({
    entries: [],
    loading: false,
    isDirectory: true,
  });

  let prevHashKey: string | null = null;
  let prevEncKey: string | null = null;
  let prevEntryCids: Map<string, string> = new Map();
  let retryTimer: ReturnType<typeof setTimeout> | null = null;
  let retryAttempts = 0;
  let forceRetry = false;
  let lastLocation: CID | null = null;
  let lastIsPermalink = false;
  let lastIsRemote = false;
  const ENTRY_RETRY_DELAY_MS = 1500;
  const PERMALINK_MAX_RETRIES = 8;
  const REMOTE_MAX_RETRIES = 24;

  async function loadEntries(location: CID | null) {
    const route = get(routeStore);
    lastIsPermalink = route.isPermalink;
    const userNpub = get(nostrStore).npub;
    lastIsRemote = !!route.npub && route.npub !== userNpub;
    lastLocation = location;
    const hashKey = location?.hash ? toHex(location.hash) : null;
    const encKey = location?.key ? toHex(location.key) : null;

    const hasChanged = hashKey !== prevHashKey || encKey !== prevEncKey;
    if (hasChanged) {
      retryAttempts = 0;
      if (retryTimer) {
        clearTimeout(retryTimer);
        retryTimer = null;
      }
    }

    const shouldForce = forceRetry;
    forceRetry = false;

    if (!shouldForce && !hasChanged) return;
    prevHashKey = hashKey;
    prevEncKey = encKey;

    if (!location || !hashKey) {
      state.set({ entries: [], loading: false, isDirectory: true });
      prevEntryCids.clear();
      return;
    }

    state.update(s => ({ ...s, loading: true }));
    const tree = getTree();

    try {
      let newEntries: TreeEntry[] = [];

      if (location.key) {
        newEntries = await tree.listDirectory(location);
      } else {
        const isDir = await tree.isDirectory(location);
        if (isDir) {
          newEntries = await tree.listDirectory(location);
        } else {
          state.set({ entries: [], loading: false, isDirectory: false });
          prevEntryCids.clear();
          scheduleEntryRetry();
          return;
        }
      }

      // Detect changed files
      if (prevEntryCids.size > 0) {
        const changedFiles = new Set<string>();
        for (const entry of newEntries) {
          const prevCid = prevEntryCids.get(entry.name);
          const newCid = entry.cid?.hash ? toHex(entry.cid.hash) : null;
          if (prevCid && newCid && prevCid !== newCid) {
            changedFiles.add(entry.name);
          }
        }
        if (changedFiles.size > 0) {
          markFilesChanged(changedFiles);
        }
      }

      const newEntryCids = new Map<string, string>();
      for (const entry of newEntries) {
        if (entry.cid?.hash) {
          newEntryCids.set(entry.name, toHex(entry.cid.hash));
        }
      }

      const entriesChanged = newEntryCids.size !== prevEntryCids.size ||
        [...newEntryCids].some(([name, cid]) => prevEntryCids.get(name) !== cid);

      prevEntryCids = newEntryCids;

      if (entriesChanged) {
        state.set({ entries: sortEntries(newEntries), loading: false, isDirectory: true });
      } else {
        state.update(s => s.loading ? { ...s, loading: false } : s);
      }

      if ((lastIsPermalink || lastIsRemote) && newEntries.length === 0 && prevEntryCids.size === 0) {
        scheduleEntryRetry();
      }
    } catch {
      state.set({ entries: [], loading: false, isDirectory: false });
      prevEntryCids.clear();
      scheduleEntryRetry();
    }
  }

  function scheduleEntryRetry(): void {
    const maxRetries = lastIsPermalink ? PERMALINK_MAX_RETRIES : lastIsRemote ? REMOTE_MAX_RETRIES : 0;
    if (maxRetries === 0 || retryTimer || retryAttempts >= maxRetries) return;
    retryAttempts += 1;
    retryTimer = setTimeout(() => {
      retryTimer = null;
      forceRetry = true;
      if (lastLocation) {
        void loadEntries(lastLocation);
      }
    }, ENTRY_RETRY_DELAY_MS);
  }

  // Subscribe to location changes
  currentDirCidStore.subscribe((location) => {
    void loadEntries(location);
  });

  return {
    subscribe: state.subscribe,
    // Expose setter for testing only
    __testSet: (value: DirectoryEntriesState) => state.set(value),
  };
}

/**
 * Global directory entries store based on current directory CID
 * trackChanges=true to detect when files change (for LIVE indicator)
 */
export const directoryEntriesStore = createGlobalDirectoryEntriesStore();

// Expose test helper on window for E2E tests
if (typeof window !== 'undefined') {
  window.__testSetDirectoryEntries = (entries: TreeEntry[]) => {
    (directoryEntriesStore as { __testSet: (value: DirectoryEntriesState) => void }).__testSet({
      entries: sortEntries(entries),
      loading: false,
      isDirectory: true,
    });
  };
}
