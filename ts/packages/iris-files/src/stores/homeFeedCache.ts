/**
 * Persistent cache for home feed videos
 * Survives component unmount for instant back-nav
 */
import { SortedMap } from '../utils/SortedMap';
import { SvelteSet } from 'svelte/reactivity';
import type { VideoItem } from '../components/Video/types';

export interface PlaylistInfo {
  videoCount: number;
  thumbnailUrl?: string;
}

// Videos from followed users
export const videosByKey = new SortedMap<string, VideoItem>(
  (a, b) => (b[1].timestamp || 0) - (a[1].timestamp || 0)
);

// Videos liked/commented by followed users
export const socialVideosByKey = new SortedMap<string, VideoItem>(
  (a, b) => (b[1].timestamp || 0) - (a[1].timestamp || 0)
);

// Seen event IDs for deduplication
export const socialSeenEventIds = new SvelteSet<string>();

// Playlist detection results for feed videos
const feedPlaylistInfoCache: Record<string, PlaylistInfo> = {};

// Track which user the cache is for
let cachedForPubkey: string | null = null;

/**
 * Clear caches when user changes
 */
export function clearCacheIfUserChanged(pubkey: string | null): boolean {
  if (pubkey !== cachedForPubkey) {
    videosByKey.clear();
    socialVideosByKey.clear();
    socialSeenEventIds.clear();
    // Clear playlist info cache
    for (const key of Object.keys(feedPlaylistInfoCache)) {
      delete feedPlaylistInfoCache[key];
    }
    cachedForPubkey = pubkey;
    return true;
  }
  return false;
}

/**
 * Get cached videos for immediate render
 */
export function getCachedVideos(): VideoItem[] {
  return videosByKey.values();
}

/**
 * Get cached social videos for immediate render
 */
export function getCachedSocialVideos(): VideoItem[] {
  return socialVideosByKey.values();
}

/**
 * Get cached playlist info for a video
 */
export function getFeedPlaylistInfo(key: string): PlaylistInfo | undefined {
  return feedPlaylistInfoCache[key];
}

/**
 * Set playlist info for a video
 */
export function setFeedPlaylistInfo(key: string, info: PlaylistInfo): void {
  feedPlaylistInfoCache[key] = info;
}

/**
 * Get all cached playlist info
 */
export function getAllFeedPlaylistInfo(): Record<string, PlaylistInfo> {
  return feedPlaylistInfoCache;
}

/**
 * Clear playlist info for a specific key
 */
export function clearFeedPlaylistInfo(key: string): void {
  delete feedPlaylistInfoCache[key];
}
