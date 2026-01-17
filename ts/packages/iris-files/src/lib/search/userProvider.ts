/**
 * User search provider
 *
 * Wraps existing searchUsers from searchIndex store
 */

import { searchUsers, type UserIndexEntry } from '../../stores/searchIndex';
import type { SearchProvider, SearchResult } from './types';

/** User search provider */
export const userProvider: SearchProvider = {
  id: 'user',
  name: 'Users',
  priority: 8,

  isAvailable(): boolean {
    return true;
  },

  async search(query: string, limit: number): Promise<SearchResult[]> {
    try {
      const users = await searchUsers(query, limit);

      return users.map((user: UserIndexEntry) => ({
        id: `user:${user.pubkey}`,
        type: 'user' as const,
        label: user.displayName || user.name || user.npub.slice(0, 12) + '...',
        sublabel: user.nip05 || undefined,
        path: `/${user.npub}`,
        score: 0.7, // Default score since searchUsers already ranks
        icon: 'i-lucide-user',
        pubkey: user.pubkey,
      }));
    } catch (e) {
      console.warn('[user] Search failed:', e);
      return [];
    }
  },
};
