/**
 * Route helper functions for actions
 */
import { get } from 'svelte/store';
import { navigate } from '../utils/navigate';
import { parseRoute } from '../utils/route';
import type { CID } from 'hashtree';
import { getTreeRootSync } from '../stores/treeRoot';
import { isViewingFileStore } from '../stores/currentDirHash';

// Helper to get current rootCid from route via resolver cache
export function getCurrentRootCid(): CID | null {
  const route = parseRoute();
  return getTreeRootSync(route.npub, route.treeName);
}

// Build route URL, preserving linkKey if present
export function buildRouteUrl(npub: string | null, treeName: string | null, path: string[], fileName?: string, linkKey?: string | null): string {
  const parts: string[] = [];

  if (npub && treeName) {
    parts.push(npub, treeName);
  }

  parts.push(...path);

  if (fileName) {
    parts.push(fileName);
  }

  let url = '/' + parts.map(encodeURIComponent).join('/');
  if (linkKey) {
    url += `?k=${linkKey}`;
  }
  return url;
}

// Get current directory path from URL (excludes file if selected)
export function getCurrentPathFromUrl(): string[] {
  const route = parseRoute();
  const urlPath = route.path;
  if (urlPath.length === 0) return [];

  // When streaming, the last path segment is the file being created (not a directory)
  if (route.params.get('stream') === '1' && urlPath.length > 0) {
    return urlPath.slice(0, -1);
  }

  // Use actual isDirectory check from store
  const isViewingFile = get(isViewingFileStore);
  return isViewingFile ? urlPath.slice(0, -1) : urlPath;
}

// Update URL to reflect current state
export function updateRoute(fileName?: string, options?: { edit?: boolean }) {
  const route = parseRoute();
  const currentPath = getCurrentPathFromUrl();
  let url = buildRouteUrl(route.npub, route.treeName, currentPath, fileName, route.params.get('k'));
  if (options?.edit) {
    // Append edit param, preserving existing query string
    url += url.includes('?') ? '&edit=1' : '?edit=1';
  }
  navigate(url);
}
