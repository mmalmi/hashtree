/**
 * Hook to compute current directory CID from rootCid + URL path
 * For encrypted trees, resolves through the path collecting keys at each level
 * Svelte version using stores
 */
import { writable, derived, get, type Readable } from 'svelte/store';
import { toHex, LinkType } from 'hashtree';
import { appStore, getTree } from '../store';
import { routeStore } from './route';
import { treeRootStore } from './treeRoot';
import { isWorkerReady } from '../lib/workerInit';
import type { CID, Hash } from 'hashtree';

// Store for current directory CID
const currentDirCidStore = writable<CID | null>(null);

// Store for whether we're viewing a file (not a directory)
const isViewingFileStore = writable<boolean>(false);

// Store for whether path resolution is in progress (prevents flash of wrong content)
// This is true when:
// 1. We have path segments but no root CID yet (waiting for tree to load)
// 2. We're actively resolving the path to determine if it's a file or directory
const resolvingPathStore = writable<boolean>(false);

// Track previous values to avoid redundant recalculations
let prevRootHash: string | null = null;
let prevRootKey: string | null = null;
let prevPathKey: string | null = null;
let lastPeerCount = 0;
let pathRetryTimer: ReturnType<typeof setTimeout> | null = null;
let pathRetryAttempts = 0;
let forceRetry = false;
const PATH_RETRY_DELAY_MS = 1500;
const PERMALINK_MAX_RETRIES = 8;
const TREE_PATH_MAX_RETRIES = 24;

// Wait for worker to be ready (with timeout to avoid blocking forever)
async function waitForWorker(timeoutMs = 10000): Promise<boolean> {
  if (isWorkerReady()) return true;

  const startTime = Date.now();
  while (Date.now() - startTime < timeoutMs) {
    await new Promise(resolve => setTimeout(resolve, 100));
    if (isWorkerReady()) return true;
  }
  return false;
}

// Reactive update based on rootCid and path changes
async function updateCurrentDirCid() {
  const rootCid = get(treeRootStore);
  const route = get(routeStore);
  const urlPath = route.path;

  const rootHash = rootCid?.hash ? toHex(rootCid.hash) : null;
  const rootKey = rootCid?.key ? toHex(rootCid.key) : null;
  const pathKey = urlPath.join('/');

  const hasChanged = rootHash !== prevRootHash || rootKey !== prevRootKey || pathKey !== prevPathKey;

  if (hasChanged) {
    pathRetryAttempts = 0;
    if (pathRetryTimer) {
      clearTimeout(pathRetryTimer);
      pathRetryTimer = null;
    }
  }

  const shouldForce = forceRetry;
  forceRetry = false;

  // Skip if no change and not forced retry
  if (!shouldForce && !hasChanged) {
    return;
  }

  // Check if only root changed (not path) - this is a merkle root update, don't show loading
  const isRootOnlyChange = pathKey === prevPathKey && prevRootHash !== null;

  prevRootHash = rootHash;
  prevRootKey = rootKey;
  prevPathKey = pathKey;

  // If we have path segments but no root CID, we're waiting for tree to load
  if (!rootCid || !rootHash) {
    currentDirCidStore.set(null);
    isViewingFileStore.set(false);
    // Set resolving=true if we have path segments AND this is a path change (not just root update)
    if (!isRootOnlyChange) {
      resolvingPathStore.set(urlPath.length > 0);
    }
    if (route.isPermalink) {
      schedulePathRetry(route);
    }
    return;
  }

  if (urlPath.length === 0) {
    if (route.isPermalink) {
      // For directory permalinks, assume directory and let entries store resolve.
      // This avoids misclassifying directories as files when data isn't local yet.
      currentDirCidStore.set(rootCid);
      isViewingFileStore.set(false);
      resolvingPathStore.set(false);
      return;
    }

    currentDirCidStore.set(rootCid);
    isViewingFileStore.set(false);
    resolvingPathStore.set(false);
    return;
  }

  // Mark as resolving before async work - but only on path changes, not root updates
  // This prevents flicker when viewing a livestream and merkle root updates
  if (!isRootOnlyChange) {
    resolvingPathStore.set(true);
  }

  // Wait for worker to be ready before resolving paths
  const workerReady = await waitForWorker();
  if (!workerReady) {
    console.warn('[currentDirHash] Worker not ready after timeout, cannot resolve path');
    resolvingPathStore.set(false);
    if (urlPath.length > 0) {
      schedulePathRetry(route);
    }
    return;
  }

  const tree = getTree();

  try {
    // For permalinks with a single path segment, check if rootCid is a file or directory
    // - If file: the path segment is just a MIME type hint, use rootCid directly
    // - If directory: the path segment is a filename to look up in the directory
    const route = get(routeStore);
    if (route.isPermalink && urlPath.length === 1) {
      const isDir = await tree.isDirectory(rootCid);
      if (!isDir) {
        // rootCid points to a file - path is just a filename hint for MIME type
        currentDirCidStore.set(null);
        isViewingFileStore.set(true);
        resolvingPathStore.set(false);
        return;
      }
      // rootCid is a directory - path is a file to look up within it
      // Fall through to resolvePath below
    }

    // Resolve full path first - returns { cid, type } with LinkType
    const result = await tree.resolvePath(rootCid, urlPath);
    if (!result) {
      // Keep resolvingPath=true - root might be stale and updating
      // The currentDirCid stays null but we don't mark as "resolved" yet
      // If a new root comes in, we'll try again via the subscription
      if (urlPath.length > 0) {
        schedulePathRetry(route);
      }
      return;
    }

    // Use type from resolvePath result (no extra store fetch needed)
    const isDir = result.type === LinkType.Dir;

    if (isDir) {
      // Path points to a directory
      currentDirCidStore.set(result.cid);
      isViewingFileStore.set(false);
    } else {
      // Path points to a file - get parent directory
      isViewingFileStore.set(true);
      if (urlPath.length === 1) {
        // File is in root
        currentDirCidStore.set(rootCid);
      } else {
        // Resolve parent directory
        const parentPath = urlPath.slice(0, -1);
        const parentResult = await tree.resolvePath(rootCid, parentPath);
        currentDirCidStore.set(parentResult?.cid ?? null);
      }
    }
    resolvingPathStore.set(false);
  } catch {
    currentDirCidStore.set(null);
    isViewingFileStore.set(false);
    resolvingPathStore.set(false);
    if (urlPath.length > 0) {
      schedulePathRetry(route);
    }
  }
}

function schedulePathRetry(route: { isPermalink: boolean; path: string[] }): void {
  if (route.path.length === 0) return;
  const maxRetries = route.isPermalink ? PERMALINK_MAX_RETRIES : TREE_PATH_MAX_RETRIES;
  if (pathRetryTimer || pathRetryAttempts >= maxRetries) {
    return;
  }
  pathRetryAttempts += 1;
  pathRetryTimer = setTimeout(() => {
    pathRetryTimer = null;
    forceRetry = true;
    updateCurrentDirCid();
  }, PATH_RETRY_DELAY_MS);
}

// Subscribe to changes in root and route - use lazy initialization for HMR compatibility
// Store the flag on a global to persist across HMR module reloads
const HMR_KEY = '__currentDirHashInitialized';
const globalObj = typeof globalThis !== 'undefined' ? globalThis : window;

function initSubscriptions() {
  if ((globalObj as Record<string, unknown>)[HMR_KEY]) return;
  (globalObj as Record<string, unknown>)[HMR_KEY] = true;

  treeRootStore.subscribe(() => {
    updateCurrentDirCid();
  });
  routeStore.subscribe(() => {
    updateCurrentDirCid();
  });
  appStore.subscribe((state) => {
    const nextCount = state.peerCount ?? 0;
    const hadNewPeer = nextCount > lastPeerCount;
    lastPeerCount = nextCount;
    if (hadNewPeer) {
      const route = get(routeStore);
      const hasPath = route.path.length > 0;
      if (hasPath && !get(currentDirCidStore)) {
        forceRetry = true;
        updateCurrentDirCid();
      }
    }
  });
}

// Initialize on first access
initSubscriptions();

/**
 * Store for current directory hash
 */
export const currentDirHashStore: Readable<Hash | null> = derived(
  currentDirCidStore,
  ($cid) => $cid?.hash ?? null
);

/**
 * Store for current directory CID
 */
export { currentDirCidStore };

/**
 * Store for whether current URL path points to a file (not a directory)
 */
export { isViewingFileStore };

/**
 * Store for whether path resolution is in progress
 * Use to wait before rendering to avoid flash of wrong content
 */
export { resolvingPathStore };

// Compatibility functions for React-style usage
export function currentDirHash(): Hash | null {
  return get(currentDirHashStore);
}

export function useCurrentDirCid(): CID | null {
  return get(currentDirCidStore);
}
