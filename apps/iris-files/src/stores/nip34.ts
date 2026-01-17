/**
 * Stores for NIP-34 Pull Requests and Issues
 */
import { writable, type Readable } from 'svelte/store';
import {
  fetchPullRequests,
  fetchIssues,
  type PullRequest,
  type Issue,
  type ItemStatus,
} from '../nip34';

export interface PullRequestsState {
  items: PullRequest[];
  loading: boolean;
  error: string | null;
  filter: ItemStatus | 'all';
}

export interface IssuesState {
  items: Issue[];
  loading: boolean;
  error: string | null;
  filter: ItemStatus | 'all';
}

/**
 * Create a store for pull requests for a specific repo
 */
export function createPullRequestsStore(npub: string | null, repoName: string | null): Readable<PullRequestsState> & {
  refresh: () => Promise<void>;
  setFilter: (filter: ItemStatus | 'all') => void;
} {
  const { subscribe, set, update } = writable<PullRequestsState>({
    items: [],
    loading: true,
    error: null,
    filter: 'all',
  });

  async function refresh() {
    if (!npub || !repoName) {
      set({ items: [], loading: false, error: null, filter: 'all' });
      return;
    }

    update(s => ({ ...s, loading: true, error: null }));

    try {
      const prs = await fetchPullRequests(npub, repoName);
      update(s => ({ ...s, items: prs, loading: false }));
    } catch (err) {
      update(s => ({
        ...s,
        loading: false,
        error: err instanceof Error ? err.message : 'Failed to fetch pull requests',
      }));
    }
  }

  function setFilter(filter: ItemStatus | 'all') {
    update(s => ({ ...s, filter }));
  }

  // Initial fetch
  if (npub && repoName) {
    refresh();
  } else {
    set({ items: [], loading: false, error: null, filter: 'all' });
  }

  return {
    subscribe,
    refresh,
    setFilter,
  };
}

/**
 * Create a store for issues for a specific repo
 */
export function createIssuesStore(npub: string | null, repoName: string | null): Readable<IssuesState> & {
  refresh: () => Promise<void>;
  setFilter: (filter: ItemStatus | 'all') => void;
} {
  const { subscribe, set, update } = writable<IssuesState>({
    items: [],
    loading: true,
    error: null,
    filter: 'all',
  });

  async function refresh() {
    if (!npub || !repoName) {
      set({ items: [], loading: false, error: null, filter: 'all' });
      return;
    }

    update(s => ({ ...s, loading: true, error: null }));

    try {
      const issues = await fetchIssues(npub, repoName);
      update(s => ({ ...s, items: issues, loading: false }));
    } catch (err) {
      update(s => ({
        ...s,
        loading: false,
        error: err instanceof Error ? err.message : 'Failed to fetch issues',
      }));
    }
  }

  function setFilter(filter: ItemStatus | 'all') {
    update(s => ({ ...s, filter }));
  }

  // Initial fetch
  if (npub && repoName) {
    refresh();
  } else {
    set({ items: [], loading: false, error: null, filter: 'all' });
  }

  return {
    subscribe,
    refresh,
    setFilter,
  };
}

/**
 * Filter items by status
 */
export function filterByStatus<T extends { status: ItemStatus }>(
  items: T[],
  filter: ItemStatus | 'all'
): T[] {
  if (filter === 'all') return items;
  return items.filter(item => item.status === filter);
}

/**
 * Count items by status
 */
export function countByStatus<T extends { status: ItemStatus }>(items: T[]): Record<ItemStatus | 'all', number> {
  const counts: Record<ItemStatus | 'all', number> = {
    all: items.length,
    open: 0,
    merged: 0,
    closed: 0,
    draft: 0,
  };

  for (const item of items) {
    counts[item.status]++;
  }

  return counts;
}
