/**
 * Playlist store and utilities
 *
 * A playlist is detected when a video tree has NO video file at root.
 * If root has video.mp4 etc → single video. Otherwise → playlist of subdirectories.
 *
 * Thresholds:
 * - MIN_VIDEOS_FOR_STRUCTURE (1): Minimum to consider it a playlist
 * - MIN_VIDEOS_FOR_SIDEBAR (2): Minimum to show playlist sidebar
 */

import { writable, get } from 'svelte/store';
import { nip19 } from 'nostr-tools';
import { getTree } from '../store';
import { getRefResolver } from '../refResolver';
import { getLocalRootCache, getLocalRootKey, onCacheUpdate } from '../treeRootCache';
import { LRUCache } from '../utils/lruCache';
import { indexVideo } from './searchIndex';
import { clearFeedPlaylistInfo } from './homeFeedCache';
import { getHtreePrefix } from '../lib/mediaUrl';
import type { CID } from 'hashtree';

// Cache playlist detection results to avoid layout shift on revisit
// Key: "npub/treeName", Value: PlaylistCardInfo or null (for single videos)
const playlistCache = new LRUCache<string, PlaylistCardInfo | null>(200);

// Invalidate playlist caches when video trees are updated
onCacheUpdate((npub: string, treeName: string) => {
  if (treeName.startsWith('videos/')) {
    const cacheKey = `${npub}/${treeName}`;
    playlistCache.delete(cacheKey);
    clearFeedPlaylistInfo(cacheKey);
  }
});

// ============================================================================
// Constants and utilities
// ============================================================================

/** Minimum videos to show playlist sidebar (1 video has nowhere to navigate) */
export const MIN_VIDEOS_FOR_SIDEBAR = 2;

/** Minimum videos to consider a playlist structure */
export const MIN_VIDEOS_FOR_STRUCTURE = 1;

/** Video file extensions we recognize */
export const VIDEO_EXTENSIONS = ['.mp4', '.webm', '.mkv', '.mov', '.avi', '.m4v'] as const;

/** Check if entries contain a video file */
export function hasVideoFile(entries: { name: string }[]): boolean {
  return entries.some(e =>
    e.name.startsWith('video.') ||
    VIDEO_EXTENSIONS.some(ext => e.name.endsWith(ext))
  );
}

/** Check if root is a playlist (no video file at root = playlist) */
export function isPlaylistStructure(entries: { name: string }[]): boolean {
  return !hasVideoFile(entries);
}

/** Find thumbnail entry in a directory */
export function findThumbnailEntry(entries: { name: string }[]): { name: string } | undefined {
  return entries.find(e =>
    e.name.startsWith('thumbnail.') ||
    e.name.endsWith('.jpg') ||
    e.name.endsWith('.webp') ||
    e.name.endsWith('.png')
  );
}

/** Build SW URL for a thumbnail */
export function buildThumbnailUrl(
  npub: string,
  treeName: string,
  videoDir: string,
  thumbName: string
): string {
  const path = videoDir
    ? `${encodeURIComponent(videoDir)}/${encodeURIComponent(thumbName)}`
    : encodeURIComponent(thumbName);
  return `${getHtreePrefix()}/htree/${npub}/${encodeURIComponent(treeName)}/${path}`;
}

/**
 * Find the first video entry in a playlist directory.
 * Returns the first directory entry name (assumes subdirs are videos).
 */
export async function findFirstVideoEntry(rootCid: CID): Promise<string | null> {
  const tree = getTree();

  try {
    const entries = await tree.listDirectory(rootCid);
    if (!entries || entries.length === 0) return null;

    // If root has video file, it's a single video, not a playlist
    if (!isPlaylistStructure(entries)) return null;

    // Return first entry (sorted for consistency)
    const sorted = [...entries].sort((a, b) => a.name.localeCompare(b.name));
    return sorted[0]?.name ?? null;
  } catch {
    return null;
  }
}

/** Info returned by detectPlaylistForCard */
export interface PlaylistCardInfo {
  videoCount: number;
  thumbnailUrl?: string;
  /** Duration in seconds (for single videos) */
  duration?: number;
  /** Created timestamp in seconds (for single videos) */
  createdAt?: number;
  /** Title from metadata (for single videos) */
  title?: string;
}

/** Helper to add timeout to promises */
function withTimeout<T>(promise: Promise<T>, ms: number): Promise<T | null> {
  return Promise.race([
    promise,
    new Promise<null>(resolve => setTimeout(() => resolve(null), ms))
  ]);
}

/**
 * Get cached playlist info synchronously.
 * Returns undefined if not cached, null if known to be single video,
 * or PlaylistCardInfo if known to be playlist.
 */
export function getCachedPlaylistInfo(npub: string, treeName: string): PlaylistCardInfo | null | undefined {
  return playlistCache.get(`${npub}/${treeName}`);
}

/**
 * Detect if a tree is a playlist and get card display info.
 * For single videos, returns { videoCount: 0, duration, thumbnailUrl }.
 * For playlists, returns { videoCount: N, thumbnailUrl }.
 * Used by VideoHome and VideoProfileView for card display.
 * Results are cached to avoid layout shift on revisit.
 */
export async function detectPlaylistForCard(
  rootCid: CID,
  npub: string,
  treeName: string
): Promise<PlaylistCardInfo | null> {
  const cacheKey = `${npub}/${treeName}`;

  // Check cache first
  const cached = playlistCache.get(cacheKey);
  if (cached !== undefined) return cached;

  const tree = getTree();

  try {
    const entries = await withTimeout(tree.listDirectory(rootCid), 5000);
    if (!entries || entries.length === 0) {
      playlistCache.set(cacheKey, null);
      return null;
    }

    // If root has video file, it's a single video - fetch duration, createdAt, and title
    if (hasVideoFile(entries)) {
      let duration: number | undefined;
      let createdAt: number | undefined;
      let thumbnailUrl: string | undefined;
      let title: string | undefined;

      // Try video file's link entry meta first (new format)
      const videoEntry = entries.find(e =>
        e.name.startsWith('video.') ||
        e.name.endsWith('.webm') ||
        e.name.endsWith('.mp4') ||
        e.name.endsWith('.mov')
      );
      if (videoEntry?.meta) {
        const videoMeta = videoEntry.meta as Record<string, unknown>;
        if (typeof videoMeta.duration === 'number') {
          duration = videoMeta.duration;
        }
        if (typeof videoMeta.createdAt === 'number') {
          createdAt = videoMeta.createdAt;
        }
        if (typeof videoMeta.title === 'string') {
          title = videoMeta.title;
        }
      }

      // Fall back to metadata.json (legacy format)
      if (!duration || !createdAt || !title) {
        const metadataEntry = entries.find(e => e.name === 'metadata.json');
        if (metadataEntry) {
          try {
            const metadataData = await withTimeout(tree.readFile(metadataEntry.cid), 2000);
            if (metadataData) {
              const metadata = JSON.parse(new TextDecoder().decode(metadataData));
              if (!duration && typeof metadata.duration === 'number') {
                duration = metadata.duration;
              }
              if (!createdAt && typeof metadata.createdAt === 'number') {
                createdAt = metadata.createdAt;
              }
              if (!title && typeof metadata.title === 'string') {
                title = metadata.title;
              }
            }
          } catch { /* ignore */ }
        }
      }

      // Try info.json if no duration or title yet
      if (!duration || !title) {
        const infoEntry = entries.find(e => e.name === 'info.json');
        if (infoEntry) {
          try {
            const infoData = await withTimeout(tree.readFile(infoEntry.cid), 2000);
            if (infoData) {
              const info = JSON.parse(new TextDecoder().decode(infoData));
              if (!duration && typeof info.duration === 'number') {
                duration = info.duration;
              }
              if (!title && typeof info.title === 'string') {
                title = info.title;
              }
            }
          } catch { /* ignore */ }
        }
      }

      // Find thumbnail
      const thumbEntry = findThumbnailEntry(entries);
      if (thumbEntry) {
        thumbnailUrl = buildThumbnailUrl(npub, treeName, '', thumbEntry.name);
      }

      const info: PlaylistCardInfo = { videoCount: 0, duration, thumbnailUrl, createdAt, title };
      playlistCache.set(cacheKey, info);
      return info;
    }

    // It's a playlist - find first thumbnail in parallel
    const sorted = [...entries].sort((a, b) => a.name.localeCompare(b.name));

    // Check first few entries in parallel for speed
    const results = await Promise.all(
      sorted.slice(0, 5).map(async (entry) => {
        try {
          const subEntries = await withTimeout(tree.listDirectory(entry.cid), 2000);
          if (subEntries) {
            const thumbEntry = findThumbnailEntry(subEntries);
            if (thumbEntry) {
              return buildThumbnailUrl(npub, treeName, entry.name, thumbEntry.name);
            }
          }
        } catch { /* skip */ }
        return null;
      })
    );

    const thumbnailUrl = results.find(r => r !== null) ?? undefined;
    const info: PlaylistCardInfo = { videoCount: entries.length, thumbnailUrl };
    playlistCache.set(cacheKey, info);
    return info;
  } catch {
    playlistCache.set(cacheKey, null);
    return null;
  }
}

// ============================================================================
// Types
// ============================================================================

export interface PlaylistItem {
  id: string;           // Directory name (e.g., video ID)
  title: string;        // From info.json or title.txt
  thumbnailUrl?: string; // SW URL to thumbnail
  duration?: number;    // From info.json (seconds)
  cid: CID;            // CID of the video subdirectory
}

export interface Playlist {
  name: string;         // Channel/playlist name
  items: PlaylistItem[];
  currentIndex: number;
  npub: string;
  treeName: string;     // e.g., "videos/Channel Name"
}

// Current playlist state
export const currentPlaylist = writable<Playlist | null>(null);

// Repeat modes: 'none' = stop at end, 'all' = loop playlist, 'one' = loop current video
export type RepeatMode = 'none' | 'all' | 'one';
export const repeatMode = writable<RepeatMode>('none');

// Shuffle mode: when enabled, playNext picks a random video
export const shuffleEnabled = writable<boolean>(false);

// Cycle through repeat modes
export function cycleRepeatMode(): RepeatMode {
  let newMode: RepeatMode = 'none';
  repeatMode.update(mode => {
    if (mode === 'none') newMode = 'all';
    else if (mode === 'all') newMode = 'one';
    else newMode = 'none';
    return newMode;
  });
  return newMode;
}

// Toggle shuffle
export function toggleShuffle(): boolean {
  let enabled = false;
  shuffleEnabled.update(v => {
    enabled = !v;
    return enabled;
  });
  return enabled;
}

/**
 * Load playlist from a video tree that has subdirectories
 * Shows sidebar immediately with folder names, then progressively loads metadata.
 *
 * @param npub Owner's npub
 * @param treeName Full tree name (e.g., "videos/Channel Name")
 * @param rootCid Root CID of the tree
 * @param currentVideoId Currently playing video's directory name
 */
export async function loadPlaylist(
  npub: string,
  treeName: string,
  rootCid: CID,
  currentVideoId?: string
): Promise<Playlist | null> {
  // Check if this playlist is already loaded - just update currentIndex
  const existing = get(currentPlaylist);
  if (existing && existing.npub === npub && existing.treeName === treeName) {
    if (currentVideoId) {
      const idx = existing.items.findIndex(v => v.id === currentVideoId);
      if (idx !== -1 && idx !== existing.currentIndex) {
        currentPlaylist.update(p => p ? { ...p, currentIndex: idx } : null);
      }
    }
    return existing;
  }

  const tree = getTree();

  try {
    // List root directory
    const entries = await tree.listDirectory(rootCid);
    if (!entries || entries.length === 0) return null;

    // Quick check: identify which entries are video directories
    // Use short timeout for initial detection
    const quickChecks = await Promise.all(
      entries.map(async (entry): Promise<{ entry: typeof entries[0]; isVideo: boolean }> => {
        try {
          const subEntries = await Promise.race([
            tree.listDirectory(entry.cid),
            new Promise<null>((_, reject) => setTimeout(() => reject(new Error('timeout')), 1500))
          ]);
          return { entry, isVideo: subEntries ? hasVideoFile(subEntries) : false };
        } catch {
          return { entry, isVideo: false };
        }
      })
    );

    const videoEntries = quickChecks.filter(c => c.isVideo).map(c => c.entry);

    // Only show playlist sidebar if we have enough videos
    if (videoEntries.length < MIN_VIDEOS_FOR_SIDEBAR) return null;

    // Sort entries by name for consistent ordering
    videoEntries.sort((a, b) => a.name.localeCompare(b.name));

    // Create skeleton items with folder names as titles (shown immediately)
    const skeletonItems: PlaylistItem[] = videoEntries.map(entry => ({
      id: entry.name,
      title: entry.name, // Default to folder name
      cid: entry.cid,
    }));

    // Find current index
    let currentIndex = 0;
    if (currentVideoId) {
      const idx = skeletonItems.findIndex(v => v.id === currentVideoId);
      if (idx !== -1) currentIndex = idx;
    }

    // Extract playlist name
    const name = treeName.replace(/^videos\//, '');

    // Set playlist immediately with skeleton items (sidebar appears now!)
    const playlist: Playlist = {
      name,
      items: skeletonItems,
      currentIndex,
      npub,
      treeName,
    };
    currentPlaylist.set(playlist);

    // Load metadata in background, updating store progressively
    loadPlaylistMetadata(npub, treeName, videoEntries, currentIndex);

    return playlist;
  } catch (e) {
    console.error('Failed to load playlist:', e);
    return null;
  }
}

/**
 * Load metadata for playlist items in background
 * Updates the store progressively as metadata loads
 */
async function loadPlaylistMetadata(
  npub: string,
  treeName: string,
  entries: Array<{ name: string; cid: CID }>,
  currentIndex: number
): Promise<void> {
  const tree = getTree();

  // Load current video first for better UX
  const orderedEntries = [...entries];
  if (currentIndex > 0 && currentIndex < orderedEntries.length) {
    const current = orderedEntries.splice(currentIndex, 1)[0];
    orderedEntries.unshift(current);
  }

  // Process entries with limited concurrency to avoid overwhelming the system
  const CONCURRENCY = 3;
  let inFlight = 0;
  let entryIndex = 0;

  const processEntry = async (entry: typeof entries[0]): Promise<void> => {
    try {
      let title = entry.name;
      let duration: number | undefined;
      let thumbnailUrl: string | undefined;

      // Check parent entry metadata first (new optimized format - no subdirectory reads needed)
      const entryMeta = entry.meta as Record<string, unknown> | undefined;
      if (entryMeta?.title && typeof entryMeta.title === 'string') {
        title = entryMeta.title;
      }
      if (entryMeta?.duration && typeof entryMeta.duration === 'number') {
        duration = entryMeta.duration;
      }
      if (entryMeta?.thumbnail && typeof entryMeta.thumbnail === 'string') {
        // Thumbnail stored as nhash - use direct nhash URL
        thumbnailUrl = `${getHtreePrefix()}/htree/${entryMeta.thumbnail}`;
      }

      // If we have title from parent entry meta, skip subdirectory reads
      if (title !== entry.name && duration && thumbnailUrl) {
        currentPlaylist.update(playlist => {
          if (!playlist) return playlist;
          const items = playlist.items.map(item =>
            item.id === entry.name
              ? { ...item, title, duration, thumbnailUrl }
              : item
          );
          return { ...playlist, items };
        });
        return;
      }

      // Read subdirectory for title and fallback thumbnail
      const subEntries = await Promise.race([
        tree.listDirectory(entry.cid),
        new Promise<null>((_, reject) => setTimeout(() => reject(new Error('timeout')), 2000))
      ]);
      if (!subEntries) return;

      // Try video file's link entry meta first (new format)
      if (title === entry.name) {
        const videoEntry = subEntries.find(e =>
          e.name.startsWith('video.') ||
          e.name.endsWith('.webm') ||
          e.name.endsWith('.mp4') ||
          e.name.endsWith('.mov')
        );
        if (videoEntry?.meta) {
          const videoMeta = videoEntry.meta as Record<string, unknown>;
          if (videoMeta.title && typeof videoMeta.title === 'string') {
            title = videoMeta.title;
          }
          if (!duration && videoMeta.duration && typeof videoMeta.duration === 'number') {
            duration = videoMeta.duration;
          }
        }
      }

      // Fall back to metadata.json (legacy format)
      if (title === entry.name) {
        const metadataEntry = subEntries.find(e => e.name === 'metadata.json');
        if (metadataEntry) {
          try {
            const metadataData = await Promise.race([
              tree.readFile(metadataEntry.cid),
              new Promise<null>((_, reject) => setTimeout(() => reject(new Error('timeout')), 1500))
            ]);
            if (metadataData) {
              const metadata = JSON.parse(new TextDecoder().decode(metadataData));
              title = metadata.title || title;
              if (!duration && typeof metadata.duration === 'number') {
                duration = metadata.duration;
              }
            }
          } catch {}
        }
      }

      // Try info.json (yt-dlp format with duration)
      if (!duration || title === entry.name) {
        const infoEntry = subEntries.find(e => e.name === 'info.json');
        if (infoEntry) {
          try {
            const infoData = await Promise.race([
              tree.readFile(infoEntry.cid),
              new Promise<null>((_, reject) => setTimeout(() => reject(new Error('timeout')), 1500))
            ]);
            if (infoData) {
              const info = JSON.parse(new TextDecoder().decode(infoData));
              if (title === entry.name) title = info.title || title;
              if (!duration) duration = info.duration;
            }
          } catch {}
        }
      }

      // Try title.txt if still no title (legacy format)
      if (title === entry.name) {
        const titleEntry = subEntries.find(e => e.name === 'title.txt');
        if (titleEntry) {
          try {
            const titleData = await Promise.race([
              tree.readFile(titleEntry.cid),
              new Promise<null>((_, reject) => setTimeout(() => reject(new Error('timeout')), 1500))
            ]);
            if (titleData) {
              title = new TextDecoder().decode(titleData).trim();
            }
          } catch {}
        }
      }

      // Find thumbnail from subdirectory if not in entry meta
      if (!thumbnailUrl) {
        const thumbEntry = findThumbnailEntry(subEntries);
        if (thumbEntry) {
          thumbnailUrl = buildThumbnailUrl(npub, treeName, entry.name, thumbEntry.name);
        }
      }

      // Update the store with this item's metadata
      currentPlaylist.update(playlist => {
        if (!playlist) return playlist;
        const items = playlist.items.map(item =>
          item.id === entry.name
            ? { ...item, title, duration, thumbnailUrl }
            : item
        );
        return { ...playlist, items };
      });

      // Index for search (only if we have a real title, not just folder name)
      if (title !== entry.name) {
        try {
          const pubkey = nip19.decode(npub).data as string;
          indexVideo({
            title,
            pubkey,
            treeName,
            videoId: entry.name,
            nhash: '', // Not available here, search by path instead
            timestamp: Date.now(),
            duration,
          });
        } catch {}
      }
    } catch {}
  };

  // Process with limited concurrency
  const processNext = async (): Promise<void> => {
    while (entryIndex < orderedEntries.length) {
      if (inFlight >= CONCURRENCY) {
        await new Promise(r => setTimeout(r, 50));
        continue;
      }
      const entry = orderedEntries[entryIndex++];
      inFlight++;
      processEntry(entry).finally(() => { inFlight--; });
    }
  };

  await processNext();
}

/**
 * Navigate to next video in playlist
 * @param options.shuffle Override shuffle setting (for auto-play)
 * @param options.wrap Whether to wrap around to start (for repeat all)
 */
export function playNext(options?: { shuffle?: boolean; wrap?: boolean }): string | null {
  const playlist = get(currentPlaylist);
  if (!playlist || playlist.items.length === 0) return null;

  const shuffle = options?.shuffle ?? get(shuffleEnabled);
  const wrap = options?.wrap ?? true;

  let nextIndex: number;

  if (shuffle) {
    // Pick random video (different from current if possible)
    if (playlist.items.length === 1) {
      nextIndex = 0;
    } else {
      do {
        nextIndex = Math.floor(Math.random() * playlist.items.length);
      } while (nextIndex === playlist.currentIndex);
    }
  } else {
    // Sequential: go to next
    nextIndex = playlist.currentIndex + 1;
    if (nextIndex >= playlist.items.length) {
      if (wrap) {
        nextIndex = 0;
      } else {
        return null; // End of playlist
      }
    }
  }

  const nextItem = playlist.items[nextIndex];
  currentPlaylist.update(p => p ? { ...p, currentIndex: nextIndex } : null);

  // Return URL hash for navigation
  return `#/${playlist.npub}/${encodeURIComponent(playlist.treeName)}/${encodeURIComponent(nextItem.id)}`;
}

/**
 * Navigate to previous video in playlist
 */
export function playPrevious(): string | null {
  const playlist = get(currentPlaylist);
  if (!playlist || playlist.items.length === 0) return null;

  const prevIndex = playlist.currentIndex === 0
    ? playlist.items.length - 1
    : playlist.currentIndex - 1;
  const prevItem = playlist.items[prevIndex];

  currentPlaylist.update(p => p ? { ...p, currentIndex: prevIndex } : null);

  return `#/${playlist.npub}/${encodeURIComponent(playlist.treeName)}/${encodeURIComponent(prevItem.id)}`;
}

/**
 * Navigate to specific video by index
 */
export function playAt(index: number): string | null {
  const playlist = get(currentPlaylist);
  if (!playlist || index < 0 || index >= playlist.items.length) return null;

  const item = playlist.items[index];
  currentPlaylist.update(p => p ? { ...p, currentIndex: index } : null);

  return `#/${playlist.npub}/${encodeURIComponent(playlist.treeName)}/${encodeURIComponent(item.id)}`;
}

/**
 * Load playlist when viewing a video inside a playlist
 * Resolves the parent tree and loads the playlist
 * @param npub Owner's npub
 * @param parentTreeName Parent tree name (e.g., "videos/Channel Name")
 * @param currentVideoId Current video's directory name
 */
export async function loadPlaylistFromVideo(
  npub: string,
  parentTreeName: string,
  currentVideoId: string
): Promise<Playlist | null> {
  try {
    let parentRoot: CID | null = null;

    // Check local cache first (for recently uploaded playlists)
    const localHash = getLocalRootCache(npub, parentTreeName);
    if (localHash) {
      const localKey = getLocalRootKey(npub, parentTreeName);
      parentRoot = { hash: localHash, key: localKey };
      console.log('[Playlist] Found in local cache:', parentTreeName);
    }

    // If not in local cache, try resolver
    if (!parentRoot) {
      const resolver = getRefResolver();
      parentRoot = await resolver.resolve(npub, parentTreeName);
    }

    if (!parentRoot) {
      console.log('[Playlist] Could not resolve parent tree:', parentTreeName);
      return null;
    }

    // Load the playlist from the parent tree
    return loadPlaylist(npub, parentTreeName, parentRoot, currentVideoId);
  } catch (e) {
    console.error('Failed to load playlist from video:', e);
    return null;
  }
}

/**
 * Clear current playlist
 */
export function clearPlaylist() {
  currentPlaylist.set(null);
}
