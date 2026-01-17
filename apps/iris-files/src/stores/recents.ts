/**
 * Store for managing recently visited locations
 * Persists to localStorage, uses Svelte stores
 */
import { writable, get } from 'svelte/store';
import type { TreeVisibility } from 'hashtree';
import { routeStore } from './route';

const STORAGE_KEY = 'hashtree:recents';
const MAX_RECENTS = 20;

export interface RecentItem {
  type: 'tree' | 'file' | 'dir' | 'hash';
  /** Display label */
  label: string;
  /** URL path to navigate to (without query params) */
  path: string;
  /** Timestamp of last visit */
  timestamp: number;
  /** Optional npub for tree/file types */
  npub?: string;
  /** Optional tree name */
  treeName?: string;
  /** Optional video ID for playlist videos (folder name within playlist tree) */
  videoId?: string;
  /** Optional visibility for tree/file types */
  visibility?: TreeVisibility;
  /** Optional link key for link-visible trees */
  linkKey?: string;
  /** For hash type: whether it has an encryption key */
  hasKey?: boolean;
  /** Video playback position in seconds (for resume) */
  videoPosition?: number;
  /** Video duration in seconds */
  duration?: number;
}

function loadRecents(): RecentItem[] {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (!stored) return [];
    const items: RecentItem[] = JSON.parse(stored);
    // Clean up: hash type items shouldn't have npub
    let cleaned = false;
    const cleanedItems = items.map(item => {
      if (item.type === 'hash' && item.npub) {
        cleaned = true;
        // eslint-disable-next-line @typescript-eslint/no-unused-vars
        const { npub, ...rest } = item;
        return rest;
      }
      return item;
    });
    // Persist cleaned data
    if (cleaned) {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(cleanedItems));
    }
    return cleanedItems;
  } catch {
    return [];
  }
}

function saveRecents(items: RecentItem[]) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(items));
  } catch {
    // Ignore storage errors
  }
}

// Svelte store for recents
export const recentsStore = writable<RecentItem[]>(loadRecents());

/**
 * Add or update a recent item
 * Moves existing items to top, deduplicates by normalized path
 */
export function addRecent(item: Omit<RecentItem, 'timestamp'>) {
  recentsStore.update(current => {
    // Find existing item to preserve videoPosition
    const existing = current.find(r => r.path === item.path);

    const newItem: RecentItem = {
      ...item,
      timestamp: Date.now(),
      // Preserve video position from existing item
      videoPosition: existing?.videoPosition,
    };

    // Remove existing item with same path
    const filtered = current.filter(r => r.path !== item.path);

    // Add to front, trim to max
    const updated = [newItem, ...filtered].slice(0, MAX_RECENTS);
    saveRecents(updated);
    return updated;
  });
}

/**
 * Update a recent item's visibility by path
 */
export function updateRecentVisibility(path: string, visibility: TreeVisibility) {
  recentsStore.update(current => {
    const updated = current.map(item =>
      item.path === path ? { ...item, visibility } : item
    );
    saveRecents(updated);
    return updated;
  });
}

/**
 * Update a recent item's label by path
 * Used to update video title after metadata loads asynchronously
 */
export function updateRecentLabel(path: string, label: string) {
  recentsStore.update(current => {
    const updated = current.map(item =>
      item.path === path ? { ...item, label } : item
    );
    saveRecents(updated);
    return updated;
  });
}

/**
 * Update a recent item's duration by path
 * Used to store video duration after it's loaded from video element
 */
export function updateRecentDuration(path: string, duration: number) {
  recentsStore.update(current => {
    const updated = current.map(item =>
      item.path === path && !item.duration
        ? { ...item, duration: Math.round(duration) }
        : item
    );
    saveRecents(updated);
    return updated;
  });
}

/**
 * Remove a recent item by tree name (for when trees are deleted)
 */
export function removeRecentByTreeName(npub: string, treeName: string) {
  recentsStore.update(current => {
    const filtered = current.filter(item =>
      !(item.npub === npub && item.treeName === treeName)
    );
    saveRecents(filtered);
    return filtered;
  });
}

/**
 * Clear all recents
 */
export function clearRecents() {
  recentsStore.set([]);
  saveRecents([]);
}

/**
 * Clear recents by tree name prefix (e.g., 'docs/' or 'videos/')
 */
export function clearRecentsByPrefix(prefix: string) {
  recentsStore.update(current => {
    const filtered = current.filter(item => !item.treeName?.startsWith(prefix));
    saveRecents(filtered);
    return filtered;
  });
}

/**
 * Get current recents synchronously
 */
export function getRecentsSync(): RecentItem[] {
  return get(recentsStore);
}

// In-memory position cache (updated frequently, flushed less often)
const positionCache = new Map<string, number>();
let flushTimeout: ReturnType<typeof setTimeout> | null = null;
const FLUSH_DELAY = 5000; // Flush to localStorage every 5 seconds

// Reactive signal for position cache updates (triggers re-renders)
export const positionCacheVersion = writable(0);

function flushPositions() {
  if (positionCache.size === 0) return;

  recentsStore.update(current => {
    let changed = false;
    const updated = current.map(item => {
      const pos = positionCache.get(item.path);
      if (pos !== undefined && pos !== item.videoPosition) {
        changed = true;
        return { ...item, videoPosition: pos };
      }
      return item;
    });
    if (changed) saveRecents(updated);
    return changed ? updated : current;
  });

  // Don't clear cache - keep positions in memory for reliable restoration
  // Cache acts as primary source, store is just persistence
}

// Flush on page unload
if (typeof window !== 'undefined') {
  window.addEventListener('beforeunload', flushPositions);
  window.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'hidden') flushPositions();
  });
}

/**
 * Update video playback position for a recent item
 * Updates in-memory cache immediately, flushes to storage periodically
 */
export function updateVideoPosition(path: string, position: number) {
  // Don't save if at the very beginning
  if (position < 3) return;

  positionCache.set(path, position);

  // Trigger reactive updates for components watching position changes
  positionCacheVersion.update(v => v + 1);

  // Debounce flush to localStorage
  if (!flushTimeout) {
    flushTimeout = setTimeout(() => {
      flushTimeout = null;
      flushPositions();
    }, FLUSH_DELAY);
  }
}

/**
 * Clear video position for a recent item (e.g., when finished watching)
 */
export function clearVideoPosition(path: string) {
  positionCache.delete(path);
  recentsStore.update(current => {
    const updated = current.map(item => {
      if (item.path === path && item.videoPosition !== undefined) {
        const { videoPosition: _unused, ...rest } = item;
        void _unused; // Destructure to remove property
        return rest;
      }
      return item;
    });
    saveRecents(updated);
    return updated;
  });
}

/**
 * Get video position for a path (checks in-memory cache first)
 */
export function getVideoPosition(path: string): number {
  // Check in-memory cache first (most recent)
  const cached = positionCache.get(path);
  if (cached !== undefined) return cached;

  // Fall back to stored value
  const recents = get(recentsStore);
  const item = recents.find(r => r.path === path);
  return item?.videoPosition ?? 0;
}

// Track nhash visits automatically
const HMR_KEY = '__recentsNhashInitialized';
const globalObj = typeof globalThis !== 'undefined' ? globalThis : window;

if (!(globalObj as Record<string, unknown>)[HMR_KEY]) {
  (globalObj as Record<string, unknown>)[HMR_KEY] = true;

  routeStore.subscribe((route) => {
    if (route.isPermalink && route.cid?.hash) {
      // Build nhash from current URL
      const hashPath = window.location.hash.replace(/^#\/?/, '');
      const nhash = hashPath.split('/')[0];

      if (!nhash) return;

      addRecent({
        type: 'hash',
        label: nhash.slice(0, 16) + '...',
        path: '/' + nhash,
        hasKey: !!route.cid.key,
      });
    }
  });
}
