/**
 * Route store for Svelte
 * Provides route information from current URL
 */
import { writable, derived, get } from 'svelte/store';
import { getQueryParamsFromHash } from '../lib/router.svelte';
import { isNHash, isNPath, nhashDecode, npathDecode } from 'hashtree';
import { nip19 } from 'nostr-tools';
import type { RouteInfo } from '../utils/route';

// Store for the current hash
export const currentHash = writable<string>(window.location.hash);

// Initialize hash listener
if (typeof window !== 'undefined') {
  const syncHash = () => {
    currentHash.set(window.location.hash);
  };

  window.addEventListener('hashchange', syncHash);

  // Ensure the initial hash is captured after the app is mounted.
  // This avoids missing the direct-nav hash if module load happens too early.
  if (document.readyState === 'loading') {
    window.addEventListener('DOMContentLoaded', syncHash, { once: true });
  } else {
    requestAnimationFrame(syncHash);
  }
}

/**
 * Parse route info from hash
 */
export function parseRouteFromHash(hash: string): RouteInfo {
  // Remove #/ prefix
  const hashPath = hash.replace(/^#\/?/, '');

  // Parse query params
  let path = hashPath;
  const qIdx = hashPath.indexOf('?');
  const params = getQueryParamsFromHash(hash);
  if (qIdx !== -1) {
    path = hashPath.slice(0, qIdx);
  }

  // Parse compareBranches (complex derived field)
  let compareBranches: { base: string; head: string } | null = null;
  const compare = params.get('compare');
  const mergeBase = params.get('base');
  const mergeHead = params.get('head');
  const isMergeView = params.get('merge') === '1';
  if (compare?.includes('...')) {
    const [base, head] = compare.split('...');
    if (base && head) compareBranches = { base, head };
  } else if (isMergeView && mergeBase && mergeHead) {
    compareBranches = { base: mergeBase, head: mergeHead };
  }

  const emptyParams = new URLSearchParams();
  const parts = path.split('/').filter(Boolean).map(decodeURIComponent);

  // nhash route: /nhash1.../path...
  if (parts[0] && isNHash(parts[0])) {
    try {
      const cid = nhashDecode(parts[0]);  // Returns CID with Uint8Array fields
      return { npub: null, treeName: null, cid, path: parts.slice(1), isPermalink: true, params, compareBranches };
    } catch {
      // Invalid nhash, fall through
    }
  }

  // npath route: /npath1...
  if (parts[0] && isNPath(parts[0])) {
    try {
      const decoded = npathDecode(parts[0]);
      const npub = nip19.npubEncode(decoded.pubkey);
      return { npub, treeName: decoded.treeName, cid: null, path: decoded.path || [], isPermalink: false, params, compareBranches };
    } catch {
      // Invalid npath, fall through
    }
  }

  // Special routes (no tree context)
  if (['settings', 'wallet', 'users'].includes(parts[0])) {
    return { npub: null, treeName: null, cid: null, path: [], isPermalink: false, params: emptyParams, compareBranches: null };
  }

  // User routes
  if (parts[0]?.startsWith('npub')) {
    const npub = parts[0];

    // Special user routes (profile, follows, edit)
    if (['profile', 'follows', 'edit'].includes(parts[1])) {
      return { npub, treeName: null, cid: null, path: [], isPermalink: false, params: emptyParams, compareBranches: null };
    }

    // Tree route: /npub/treeName/path...
    if (parts[1] && !['profile', 'follows', 'edit'].includes(parts[1])) {
      return { npub, treeName: parts[1], cid: null, path: parts.slice(2), isPermalink: false, params, compareBranches };
    }

    // User view: /npub
    return { npub, treeName: null, cid: null, path: [], isPermalink: false, params: emptyParams, compareBranches: null };
  }

  // Home route
  return { npub: null, treeName: null, cid: null, path: [], isPermalink: false, params: emptyParams, compareBranches: null };
}

/**
 * Derived store for route info
 */
export const routeStore = derived(currentHash, ($hash) => parseRouteFromHash($hash));

/**
 * Get current route synchronously
 */
export function getRouteSync(): RouteInfo {
  return parseRouteFromHash(get(currentHash));
}

/**
 * Derived store for just the current path
 */
export const currentPathStore = derived(routeStore, ($route) => $route.path);
