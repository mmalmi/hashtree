/**
 * Route parsing utilities
 * Parses URL hash to extract route info without needing React Router context
 */
import { getQueryParamsFromHash } from '../lib/router.svelte';

/** CID in hex format for routing (hash + optional key) */
export interface RouteCid {
  hash: string;
  key?: string;
}

export interface RouteInfo {
  npub: string | null;
  treeName: string | null;
  /** CID for permalink routes (hash + optional decrypt key) */
  cid: RouteCid | null;
  path: string[];
  /** True when viewing a permalink (nhash route) */
  isPermalink: boolean;
  /** All query params from URL - use params.get('k'), params.get('t'), etc. */
  params: URLSearchParams;
  /** Branches to compare (from ?compare=base...head or ?merge=1&base=&head=) */
  compareBranches: { base: string; head: string } | null;
}

/**
 * Parse route info from window.location.hash
 * Handles:
 * - #/npub/treeName/path/to/file
 * - #/nhash1.../path/to/file
 * - #/npub (user view)
 * - #/npub/profile
 */
export function parseRoute(): RouteInfo {
  // Get hash path and query params
  const fullHash = window.location.hash.slice(2); // Remove #/
  const [hashPath] = fullHash.split('?');
  const parts = hashPath.split('/').filter(Boolean).map(decodeURIComponent);

  // Parse query params
  const params = getQueryParamsFromHash(window.location.hash);
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

  // nhash route: #/nhash1.../path...
  if (parts[0]?.startsWith('nhash1')) {
    // Decode nhash to extract CID (hash and optional key as Uint8Array)
    try {
      const { nhashDecode } = require('hashtree');
      const cid = nhashDecode(parts[0]);
      return {
        npub: null,
        treeName: null,
        cid,
        path: parts.slice(1),
        isPermalink: true,
        params,
        compareBranches,
      };
    } catch {
      // Fall through if decode fails
    }
  }

  // Special routes (no tree context)
  if (['settings', 'wallet'].includes(parts[0])) {
    return { npub: null, treeName: null, cid: null, path: [], isPermalink: false, params: emptyParams, compareBranches: null };
  }

  // User routes
  if (parts[0]?.startsWith('npub')) {
    const npub = parts[0];

    // Special user routes (profile, follows, followers, edit)
    if (['profile', 'follows', 'followers', 'edit'].includes(parts[1])) {
      return { npub, treeName: null, cid: null, path: [], isPermalink: false, params: emptyParams, compareBranches: null };
    }

    // Tree route: #/npub/treeName/path...
    if (parts[1] && !['profile', 'follows', 'followers', 'edit'].includes(parts[1])) {
      return {
        npub,
        treeName: parts[1],
        cid: null,
        path: parts.slice(2),
        isPermalink: false,
        params,
        compareBranches,
      };
    }

    // User view: #/npub
    return { npub, treeName: null, cid: null, path: [], isPermalink: false, params: emptyParams, compareBranches: null };
  }

  // Home route
  return { npub: null, treeName: null, cid: null, path: [], isPermalink: false, params: emptyParams, compareBranches: null };
}
