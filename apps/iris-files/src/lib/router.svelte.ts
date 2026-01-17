/**
 * Simple hash router for Svelte 5 using stores for reliable reactivity
 */
import { writable } from 'svelte/store';

// Get initial path from hash (without query string)
function getHashPath(): string {
  if (typeof window === 'undefined') return '/';
  const hash = window.location.hash.slice(1); // Remove #
  // Remove query string for path matching
  const queryIndex = hash.indexOf('?');
  return queryIndex !== -1 ? hash.slice(0, queryIndex) || '/' : hash || '/';
}

// Get full hash including query string
function getFullHash(): string {
  if (typeof window === 'undefined') return '/';
  return window.location.hash.slice(1) || '/';
}

// Create writable stores
const pathStore = writable<string>(getHashPath());
const fullHashStore = writable<string>(getFullHash());
const refreshKeyStore = writable<number>(0);

// Export the stores for subscription
export const currentPath = {
  subscribe: pathStore.subscribe
};

// Full hash including query params (for detecting ?tab=pulls etc)
export const currentFullHash = {
  subscribe: fullHashStore.subscribe
};

// Refresh key - increment to force re-render of current route
export const refreshKey = {
  subscribe: refreshKeyStore.subscribe
};

// Trigger a refresh of the current route
export function refresh() {
  refreshKeyStore.update(k => k + 1);
}

// Initialize hashchange listener (call once from App.svelte onMount)
// Store the flag on a global to persist across HMR module reloads
const HMR_KEY = '__routerInitialized';
const globalObj = typeof globalThis !== 'undefined' ? globalThis : window;

export function initRouter() {
  if ((globalObj as Record<string, unknown>)[HMR_KEY] || typeof window === 'undefined') return;
  (globalObj as Record<string, unknown>)[HMR_KEY] = true;

  // Update stores with current hash (important for initial page load with hash)
  pathStore.set(getHashPath());
  fullHashStore.set(getFullHash());

  let lastPath = getHashPath();

  window.addEventListener('hashchange', () => {
    const newPath = getHashPath();
    const pathChanged = newPath !== lastPath;
    lastPath = newPath;

    pathStore.set(newPath);
    fullHashStore.set(getFullHash());

    // Scroll to top when path changes (not just query params)
    if (pathChanged) {
      window.scrollTo(0, 0);
    }
  });
}

// Navigate to a new hash route
export function navigate(path: string) {
  // Ensure path starts with /
  const normalizedPath = path.startsWith('/') ? path : '/' + path;

  // Update hash (this will trigger hashchange which updates the store)
  window.location.hash = normalizedPath;

  // Also update store directly for immediate reactivity within same tick
  pathStore.set(normalizedPath);
}

// Alias for navigate
export const push = navigate;

export function replace(path: string) {
  const normalizedPath = path.startsWith('/') ? path : '/' + path;
  window.location.replace('#' + normalizedPath);
  pathStore.set(normalizedPath);
}

// Parse route parameters
export interface RouteParams {
  [key: string]: string;
}

export interface RouteMatch {
  matched: boolean;
  params: RouteParams;
}

// Match a path against a pattern (e.g., '/user/:npub' matches '/user/npub123')
export function matchRoute(pattern: string, path: string): RouteMatch {
  // Don't decode %2F before splitting - tree names with slashes should stay encoded
  // until decodeURIComponent is called for each part
  const patternParts = pattern.split('/').filter(Boolean);
  const pathParts = path.split('/').filter(Boolean);

  // Handle query string (should already be stripped, but just in case)
  const queryIndex = pathParts[pathParts.length - 1]?.indexOf('?');
  if (queryIndex !== -1 && pathParts.length > 0) {
    pathParts[pathParts.length - 1] = pathParts[pathParts.length - 1].substring(0, queryIndex);
  }

  if (patternParts.length !== pathParts.length) {
    // Check for wildcard pattern ending with *
    if (patternParts[patternParts.length - 1] === '*') {
      // Wildcard matches anything remaining
      if (pathParts.length >= patternParts.length - 1) {
        const params: RouteParams = {};
        for (let i = 0; i < patternParts.length - 1; i++) {
          if (patternParts[i].startsWith(':')) {
            params[patternParts[i].slice(1)] = decodeURIComponent(pathParts[i]);
          } else if (patternParts[i] !== pathParts[i]) {
            return { matched: false, params: {} };
          }
        }
        // Capture the rest as 'wild' param (decode each part)
        params['wild'] = pathParts.slice(patternParts.length - 1).map(decodeURIComponent).join('/');
        return { matched: true, params };
      }
    }
    return { matched: false, params: {} };
  }

  const params: RouteParams = {};
  for (let i = 0; i < patternParts.length; i++) {
    if (patternParts[i] === '*') {
      // Wildcard captures remaining path (decode each part)
      params['wild'] = pathParts.slice(i).map(decodeURIComponent).join('/');
      return { matched: true, params };
    } else if (patternParts[i].startsWith(':')) {
      params[patternParts[i].slice(1)] = decodeURIComponent(pathParts[i]);
    } else if (patternParts[i] !== pathParts[i]) {
      return { matched: false, params: {} };
    }
  }

  return { matched: true, params };
}

// Get current query params
export function getQueryParamsFromHash(hash: string): URLSearchParams {
  const hashValue = hash.startsWith('#') ? hash.slice(1) : hash;
  const queryIndex = hashValue.indexOf('?');
  if (queryIndex === -1) return new URLSearchParams();
  return new URLSearchParams(hashValue.slice(queryIndex + 1));
}

export function getQueryParams(): URLSearchParams {
  return getQueryParamsFromHash(window.location.hash);
}
