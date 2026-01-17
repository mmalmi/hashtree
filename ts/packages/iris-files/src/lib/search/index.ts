/**
 * Unified search service
 *
 * Aggregates results from multiple providers with deduplication and ranking.
 */

import type { SearchProvider, SearchResult, SearchOptions } from './types';
import { historyProvider, getRecentHistory } from './historyProvider';
import { videoProvider } from './videoProvider';
import { userProvider } from './userProvider';
import { appsProvider, getAppSuggestions } from './appsProvider';

export type { SearchProvider, SearchResult, SearchOptions } from './types';
export { recordHistoryVisit, getRecentHistory } from './historyProvider';

// Provider registry
const providers = new Map<string, SearchProvider>();

/** Register a search provider */
export function registerProvider(provider: SearchProvider): void {
  providers.set(provider.id, provider);
}

/** Unregister a search provider */
export function unregisterProvider(id: string): void {
  providers.delete(id);
}

/** Get all registered providers */
export function getProviders(): SearchProvider[] {
  return Array.from(providers.values());
}

// Register default providers
registerProvider(historyProvider);
registerProvider(videoProvider);
registerProvider(userProvider);
registerProvider(appsProvider);

/** Search all providers and return merged results */
export async function search(
  query: string,
  options: SearchOptions = {}
): Promise<SearchResult[]> {
  const { limit = 20, sources, minScore = 0 } = options;

  // Get providers to search
  const providersToSearch = sources
    ? sources.map((id) => providers.get(id)).filter(Boolean) as SearchProvider[]
    : Array.from(providers.values()).filter((p) => p.isAvailable());

  if (providersToSearch.length === 0) {
    return [];
  }

  // Search all providers in parallel
  const perProviderLimit = Math.ceil(limit / providersToSearch.length) + 5; // Extra for dedup
  const resultArrays = await Promise.all(
    providersToSearch.map(async (provider) => {
      try {
        return await provider.search(query, perProviderLimit);
      } catch (e) {
        console.warn(`[search] Provider ${provider.id} failed:`, e);
        return [];
      }
    })
  );

  // Flatten and merge results
  const allResults = resultArrays.flat();

  // Deduplicate by path (keep highest score)
  const byPath = new Map<string, SearchResult>();
  for (const result of allResults) {
    const existing = byPath.get(result.path);
    if (!existing || result.score > existing.score) {
      byPath.set(result.path, result);
    }
  }

  // Filter by minimum score and sort
  const filtered = Array.from(byPath.values())
    .filter((r) => r.score >= minScore)
    .sort((a, b) => {
      // Sort by score descending
      if (b.score !== a.score) return b.score - a.score;
      // Then by timestamp descending (most recent first)
      const aTime = a.timestamp ?? 0;
      const bTime = b.timestamp ?? 0;
      return bTime - aTime;
    });

  return filtered.slice(0, limit);
}

/** Search with grouping by type */
export async function searchGrouped(
  query: string,
  options: SearchOptions = {}
): Promise<Map<string, SearchResult[]>> {
  const results = await search(query, { ...options, limit: (options.limit ?? 20) * 2 });

  const grouped = new Map<string, SearchResult[]>();

  for (const result of results) {
    const group = grouped.get(result.type) ?? [];
    group.push(result);
    grouped.set(result.type, group);
  }

  // Apply per-group limits
  const perGroupLimit = Math.ceil((options.limit ?? 20) / grouped.size);
  for (const [type, group] of grouped) {
    grouped.set(type, group.slice(0, perGroupLimit));
  }

  return grouped;
}

/** Get suggestions for empty query (recent history + apps) */
export async function getSuggestions(limit = 10): Promise<SearchResult[]> {
  const history = await getRecentHistory(limit);

  // If no history, show app suggestions
  if (history.length === 0) {
    return getAppSuggestions().slice(0, limit);
  }

  return history;
}
