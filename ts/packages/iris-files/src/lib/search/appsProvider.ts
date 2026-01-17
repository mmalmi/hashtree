/**
 * Apps search provider
 *
 * Provides suggestions for local apps in Tauri (Files, Video, Docs)
 */

import { isTauri } from '../../tauri';
import type { SearchProvider, SearchResult } from './types';

// Local apps available in Tauri
const localApps: SearchResult[] = [
  {
    id: 'app:files',
    type: 'tree' as const,
    label: 'Files',
    sublabel: 'Browse your files',
    path: '/',
    score: 0.9,
    icon: 'i-lucide-folder',
  },
  {
    id: 'app:video',
    type: 'video' as const,
    label: 'Video',
    sublabel: 'Watch and upload videos',
    path: '/video',
    score: 0.9,
    icon: 'i-lucide-play-circle',
  },
  {
    id: 'app:docs',
    type: 'file' as const,
    label: 'Docs',
    sublabel: 'Create and edit documents',
    path: '/docs',
    score: 0.9,
    icon: 'i-lucide-file-text',
  },
];

/** Apps search provider */
export const appsProvider: SearchProvider = {
  id: 'apps',
  name: 'Apps',
  priority: 3,

  isAvailable(): boolean {
    return isTauri();
  },

  async search(query: string, limit: number): Promise<SearchResult[]> {
    const queryLower = query.toLowerCase();

    const matches = localApps.filter((app) => {
      const labelMatch = app.label.toLowerCase().includes(queryLower);
      const sublabelMatch = app.sublabel?.toLowerCase().includes(queryLower);
      return labelMatch || sublabelMatch;
    });

    return matches.slice(0, limit);
  },
};

/** Get app suggestions (for empty query) */
export function getAppSuggestions(): SearchResult[] {
  if (!isTauri()) return [];
  return localApps;
}
