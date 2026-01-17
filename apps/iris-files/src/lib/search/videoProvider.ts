/**
 * Video search provider
 *
 * Wraps existing searchVideos from searchIndex store
 */

import { searchVideos, type VideoIndexEntry } from '../../stores/searchIndex';
import type { SearchProvider, SearchResult } from './types';

/** Video search provider */
export const videoProvider: SearchProvider = {
  id: 'video',
  name: 'Videos',
  priority: 5,

  isAvailable(): boolean {
    return true;
  },

  async search(query: string, limit: number): Promise<SearchResult[]> {
    try {
      const videos = await searchVideos(query, limit);

      return videos.map((video: VideoIndexEntry) => ({
        id: `video:${video.nhash || video.pubkey + ':' + video.treeName}`,
        type: 'video' as const,
        label: video.title,
        sublabel: video.duration ? formatDuration(video.duration) : undefined,
        path: buildVideoPath(video),
        score: 0.8, // Default score since searchVideos already ranks
        icon: 'i-lucide-play-circle',
        pubkey: video.pubkey,
        timestamp: video.timestamp,
      }));
    } catch (e) {
      console.warn('[video] Search failed:', e);
      return [];
    }
  },
};

/** Build navigation path for a video */
function buildVideoPath(video: VideoIndexEntry): string {
  if (video.nhash) {
    return `/${video.nhash}`;
  }
  // Build path from pubkey/treeName/videoId
  const { nip19 } = require('nostr-tools');
  const npub = nip19.npubEncode(video.pubkey);
  if (video.treeName && video.videoId) {
    return `/${npub}/${video.treeName}/${video.videoId}`;
  }
  if (video.treeName) {
    return `/${npub}/${video.treeName}`;
  }
  return `/${npub}`;
}

/** Format duration in seconds to mm:ss or hh:mm:ss */
function formatDuration(seconds: number): string {
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = Math.floor(seconds % 60);

  if (h > 0) {
    return `${h}:${m.toString().padStart(2, '0')}:${s.toString().padStart(2, '0')}`;
  }
  return `${m}:${s.toString().padStart(2, '0')}`;
}
