/**
 * History search provider
 *
 * Tauri: Uses heed-backed history with fuzzy search
 * Web: Falls back to recents store (simple prefix match)
 */

import { isTauri } from '../../tauri';
import { getRecentsSync } from '../../stores/recents';
import { parseKeywords } from '../../stores/searchIndex';
import type { SearchProvider, SearchResult } from './types';

/** History entry from Tauri backend */
interface TauriHistoryEntry {
  path: string;
  label: string;
  entry_type: string;
  npub?: string;
  tree_name?: string;
  visit_count: number;
  last_visited: number;
  first_visited: number;
}

/** Search result from Tauri backend */
interface TauriHistorySearchResult {
  entry: TauriHistoryEntry;
  score: number;
}

/** Record a history visit (Tauri only) */
export async function recordHistoryVisit(
  path: string,
  label: string,
  entryType: string,
  npub?: string,
  treeName?: string
): Promise<void> {
  if (!isTauri()) return;

  try {
    const { invoke } = await import('@tauri-apps/api/core');
    await invoke('record_history_visit', {
      path,
      label,
      entryType,
      npub: npub ?? null,
      treeName: treeName ?? null,
    });
  } catch (e) {
    console.warn('[history] Failed to record visit:', e);
  }
}

/** Search history (Tauri) */
async function searchHistoryTauri(query: string, limit: number): Promise<SearchResult[]> {
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    const results = await invoke<TauriHistorySearchResult[]>('search_history', {
      query,
      limit,
    });

    return results.map((r) => ({
      id: `history:${r.entry.path}`,
      type: mapEntryType(r.entry.entry_type),
      label: r.entry.label,
      sublabel: formatTimeAgo(r.entry.last_visited),
      path: r.entry.path,
      score: normalizeScore(r.score),
      icon: getIconForType(r.entry.entry_type),
      timestamp: r.entry.last_visited,
    }));
  } catch (e) {
    console.warn('[history] Search failed:', e);
    return [];
  }
}

/** Search history (Web fallback - simple prefix match on recents) */
function searchHistoryWeb(query: string, limit: number): SearchResult[] {
  // Filter stop words from query - if no keywords remain, return empty
  const keywords = parseKeywords(query);
  if (keywords.length === 0) {
    return [];
  }

  const recents = getRecentsSync();

  const matches = recents
    .filter((r) => {
      // Check if any query keyword matches label, path, or treeName
      const labelLower = r.label.toLowerCase();
      const pathLower = r.path.toLowerCase();
      const treeLower = r.treeName?.toLowerCase() ?? '';

      return keywords.some(
        (kw) => labelLower.includes(kw) || pathLower.includes(kw) || treeLower.includes(kw)
      );
    })
    .slice(0, limit)
    .map((r, idx) => ({
      id: `history:${r.path}`,
      type: mapEntryType(r.type) as SearchResult['type'],
      label: r.label,
      sublabel: formatTimeAgo(r.timestamp),
      path: r.path,
      score: 1 - idx * 0.05, // Recency-based score
      icon: getIconForType(r.type),
      timestamp: r.timestamp,
    }));

  return matches;
}

/** Map entry type to SearchResult type */
function mapEntryType(entryType: string): SearchResult['type'] {
  switch (entryType) {
    case 'video':
      return 'video';
    case 'user':
      return 'user';
    case 'tree':
    case 'dir':
      return 'tree';
    case 'file':
      return 'file';
    default:
      return 'history';
  }
}

/** Get icon class for entry type */
function getIconForType(entryType: string): string {
  switch (entryType) {
    case 'video':
      return 'i-lucide-play-circle';
    case 'user':
      return 'i-lucide-user';
    case 'tree':
    case 'dir':
      return 'i-lucide-folder';
    case 'file':
      return 'i-lucide-file';
    case 'app':
      return 'i-lucide-layout-grid';
    case 'hash':
      return 'i-lucide-link';
    default:
      return 'i-lucide-clock';
  }
}

/** Normalize Tauri score to 0-1 range */
function normalizeScore(score: number): number {
  // Tauri scores can be 0-10+, normalize to 0-1
  return Math.min(1, score / 10);
}

/** Format timestamp as relative time */
function formatTimeAgo(timestamp: number): string {
  const now = Date.now();
  const diff = now - timestamp;

  const minutes = Math.floor(diff / 60000);
  const hours = Math.floor(diff / 3600000);
  const days = Math.floor(diff / 86400000);

  if (minutes < 1) return 'Just now';
  if (minutes < 60) return `${minutes}m ago`;
  if (hours < 24) return `${hours}h ago`;
  if (days < 7) return `${days}d ago`;
  return new Date(timestamp).toLocaleDateString();
}

/** History search provider */
export const historyProvider: SearchProvider = {
  id: 'history',
  name: 'History',
  priority: 10, // Show history first

  isAvailable(): boolean {
    return true; // Always available (web has fallback)
  },

  async search(query: string, limit: number): Promise<SearchResult[]> {
    if (isTauri()) {
      return searchHistoryTauri(query, limit);
    }
    return searchHistoryWeb(query, limit);
  },
};

/** Get recent history without search (for empty query) */
export async function getRecentHistory(limit: number): Promise<SearchResult[]> {
  if (isTauri()) {
    try {
      const { invoke } = await import('@tauri-apps/api/core');
      const entries = await invoke<TauriHistoryEntry[]>('get_recent_history', { limit });

      return entries.map((entry) => ({
        id: `history:${entry.path}`,
        type: mapEntryType(entry.entry_type),
        label: entry.label,
        sublabel: formatTimeAgo(entry.last_visited),
        path: entry.path,
        score: 1,
        icon: getIconForType(entry.entry_type),
        timestamp: entry.last_visited,
      }));
    } catch (e) {
      console.warn('[history] Failed to get recent:', e);
      return [];
    }
  }

  // Web fallback
  return getRecentsSync()
    .slice(0, limit)
    .map((r) => ({
      id: `history:${r.path}`,
      type: mapEntryType(r.type) as SearchResult['type'],
      label: r.label,
      sublabel: formatTimeAgo(r.timestamp),
      path: r.path,
      score: 1,
      icon: getIconForType(r.type),
      timestamp: r.timestamp,
    }));
}
