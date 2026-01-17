/**
 * File URL Helper
 *
 * Generates URLs for streaming files through the service worker (web)
 * or native HTTP server (Tauri desktop).
 *
 * URL formats:
 * - Web:   /htree/{npub}/{treeName}/{path} or /htree/{nhash}/{filename}
 * - Tauri: http://127.0.0.1:21417/htree/{...} (same path structure, fixed port)
 */

import { nhashEncode, type CID } from 'hashtree';
import { isTauri } from '../tauri';
import { logHtreeDebug } from './htreeDebug';

/** Fixed port for Tauri htree server */
const TAURI_HTREE_PORT = 21417;
const LOCAL_PROBE_TIMEOUT_MS = 500;
const LOCAL_PROBE_INTERVAL_MS = 1000;
const PREFIX_READY_TIMEOUT_MS = 15000;
const LOCAL_HTREE_PREFIXES = new Set([
  `http://127.0.0.1:${TAURI_HTREE_PORT}`,
  `http://localhost:${TAURI_HTREE_PORT}`,
]);

let cachedPrefix = '';
let loggedMissingPrefix = false;
const prefixListeners = new Set<(prefix: string) => void>();
let localProbePromise: Promise<boolean> | null = null;
let prefixReady = false;
let prefixEpoch = 0;

async function probeLocalHtreeServer(baseUrl: string): Promise<boolean> {
  if (typeof window === 'undefined') return false;
  if (localProbePromise) return localProbePromise;

  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), LOCAL_PROBE_TIMEOUT_MS);

  localProbePromise = fetch(`${baseUrl}/htree/test`, {
    method: 'HEAD',
    signal: controller.signal,
  })
    .then(() => true)
    .catch(() => false)
    .finally(() => {
      clearTimeout(timeout);
      localProbePromise = null;
    });

  return localProbePromise;
}

function isLocalHtreePrefix(prefix: string): boolean {
  return LOCAL_HTREE_PREFIXES.has(prefix);
}

function notifyPrefixReady(source: string): void {
  if (prefixReady) return;
  if (!cachedPrefix) return;
  prefixReady = true;
  prefixEpoch += 1;
  logHtreeDebug('prefix:ready', { prefix: cachedPrefix, source, epoch: prefixEpoch });
  prefixListeners.forEach((callback) => {
    try {
      callback(cachedPrefix);
    } catch (err) {
      console.warn('[mediaUrl] Failed to notify prefix listener:', err);
    }
  });
  prefixListeners.clear();
}

function updateCachedPrefix(next: string, source: string): void {
  const normalized = next.trim().replace(/\/$/, '');
  if (normalized === cachedPrefix) return;
  cachedPrefix = normalized;
  logHtreeDebug('prefix:update', { prefix: normalized, source });
}

declare global {
  interface Window {
    __HTREE_SERVER_URL__?: string;
  }
}

function getHtreeServerOverride(): string | null {
  if (typeof window === 'undefined') return null;
  const override = window.__HTREE_SERVER_URL__;
  if (typeof override !== 'string') return null;
  const trimmed = override.trim();
  return trimmed ? trimmed.replace(/\/$/, '') : null;
}

/**
 * Get the URL prefix based on runtime environment
 * - Web: "" (uses relative /htree paths, service worker intercepts)
 * - Tauri: http://127.0.0.1:21417 (fixed htree server URL)
 */
export function getHtreePrefix(): string {
  const override = getHtreeServerOverride();
  if (override) {
    updateCachedPrefix(override, 'override');
    if (!prefixReady && !isLocalHtreePrefix(cachedPrefix)) {
      notifyPrefixReady('override');
    }
    return cachedPrefix;
  }
  if (cachedPrefix) {
    return cachedPrefix;
  }
  if (typeof window !== 'undefined' && window.htree?.htreeBaseUrl) {
    const prefix = window.htree.htreeBaseUrl;
    if (typeof prefix === 'string' && prefix.trim()) {
      updateCachedPrefix(prefix, 'window.htree');
      if (!prefixReady && !isLocalHtreePrefix(cachedPrefix)) {
        notifyPrefixReady('window.htree');
      }
      return cachedPrefix;
    }
  }
  const tauri = isTauri();
  if (tauri) {
    const prefix = `http://127.0.0.1:${TAURI_HTREE_PORT}`;
    updateCachedPrefix(prefix, 'tauri');
    return cachedPrefix;
  }
  if (!loggedMissingPrefix && typeof window !== 'undefined') {
    const protocol = window.location?.protocol || '';
    const hasTauriGlobals = '__TAURI_INTERNALS__' in window || '__TAURI__' in window;
    if (protocol === 'tauri:' || hasTauriGlobals) {
      loggedMissingPrefix = true;
      logHtreeDebug('prefix:missing', {
        protocol,
        hasTauriGlobals,
      });
    }
  }
  return '';
}

/**
 * Async version for compatibility - just returns sync result
 */
export async function getHtreePrefixAsync(): Promise<string> {
  const prefix = getHtreePrefix();
  if (!isLocalHtreePrefix(prefix)) {
    return prefix;
  }
  if (prefixReady) {
    return prefix;
  }
  void initHtreePrefix();
  return await new Promise((resolve) => {
    const timeoutId = setTimeout(() => resolve(prefix), PREFIX_READY_TIMEOUT_MS);
    onHtreePrefixReady((readyPrefix) => {
      clearTimeout(timeoutId);
      resolve(readyPrefix);
    });
  });
}

/**
 * Subscribe to prefix ready - fires when a usable prefix is confirmed.
 */
export function onHtreePrefixReady(callback: (prefix: string) => void): void {
  if (prefixReady && cachedPrefix) {
    callback(cachedPrefix);
    return;
  }
  prefixListeners.add(callback);
}

/**
 * Initialize htree prefix - waits for Tauri server to be reachable.
 */
export async function initHtreePrefix(): Promise<void> {
  if (typeof window === 'undefined') return;
  if (prefixReady) return;
  const prefix = getHtreePrefix();
  if (prefix && !isLocalHtreePrefix(prefix)) {
    notifyPrefixReady('init');
    return;
  }

  // Tauri globals may appear slightly after page load; poll briefly.
  const start = Date.now();
  const maxWaitMs = 60000;
  const intervalMs = 100;
  const maxAttempts = Math.ceil(maxWaitMs / intervalMs);
  let lastProbeAt = 0;
  const localBaseUrl = `http://127.0.0.1:${TAURI_HTREE_PORT}`;
  for (let i = 0; i < maxAttempts; i++) {
    await new Promise(resolve => setTimeout(resolve, intervalMs));
    const nextPrefix = getHtreePrefix();
    if (nextPrefix && !isLocalHtreePrefix(nextPrefix)) {
      notifyPrefixReady('init');
      return;
    }
    const now = Date.now();
    if (now - lastProbeAt >= LOCAL_PROBE_INTERVAL_MS) {
      lastProbeAt = now;
      const reachable = await probeLocalHtreeServer(localBaseUrl);
      if (reachable) {
        updateCachedPrefix(localBaseUrl, 'local-probe');
        notifyPrefixReady('local-probe');
        return;
      }
    }
  }
  logHtreeDebug('prefix:init-timeout', { waitedMs: Math.round(Date.now() - start) });
}

export function appendHtreeCacheBust(url: string): string {
  if (!prefixEpoch) return url;
  const separator = url.includes('?') ? '&' : '?';
  return `${url}${separator}htree_p=${prefixEpoch}`;
}

/**
 * Generate a file URL for npub-based access
 *
 * @param npub - The npub of the user
 * @param treeName - The tree name (e.g., 'public' or 'videos/My Video')
 * @param path - File path within the tree
 * @returns URL string like /htree/npub1.../public/video.mp4
 */
export function getNpubFileUrl(npub: string, treeName: string, path: string): string {
  const encodedTreeName = encodeURIComponent(treeName);
  const encodedPath = path.split('/').map(encodeURIComponent).join('/');
  const url = `${getHtreePrefix()}/htree/${npub}/${encodedTreeName}/${encodedPath}`;
  return appendHtreeCacheBust(url);
}

/**
 * Generate a file URL for npub-based access (async version)
 */
export async function getNpubFileUrlAsync(npub: string, treeName: string, path: string): Promise<string> {
  return getNpubFileUrl(npub, treeName, path);
}

/**
 * Generate a file URL for direct nhash access (content-addressed)
 *
 * @param cid - The content ID (with Uint8Array fields)
 * @param filename - Optional filename (for MIME type detection)
 * @returns URL string like /htree/nhash1...
 */
export function getNhashFileUrl(cid: CID, filename?: string): string {
  const nhash = nhashEncode(cid);
  const prefix = getHtreePrefix();
  if (filename) {
    return appendHtreeCacheBust(`${prefix}/htree/${nhash}/${encodeURIComponent(filename)}`);
  }
  return appendHtreeCacheBust(`${prefix}/htree/${nhash}`);
}

/**
 * Legacy alias for getNhashFileUrl (backwards compatibility)
 */
export function getCidFileUrl(cid: CID, filename: string = 'file'): string {
  return getNhashFileUrl(cid, filename);
}

/**
 * Legacy alias for getNhashFileUrl (backwards compatibility)
 */
export function getMediaUrl(cid: CID, path: string = ''): string {
  return getNhashFileUrl(cid, path);
}

/**
 * Generate a thumbnail URL for a video/content
 *
 * @param npub - The npub of the owner
 * @param treeName - The tree name
 * @param videoId - Optional video ID subdirectory
 * @param hashPrefix - Optional hash prefix for cache busting
 * @returns URL string like /htree/npub1.../treeName/videoId/thumbnail?v=abc123
 */
export function getThumbnailUrl(npub: string, treeName: string, videoId?: string, hashPrefix?: string): string {
  const encodedTreeName = encodeURIComponent(treeName);
  const path = videoId
    ? `${videoId.split('/').map(encodeURIComponent).join('/')}/thumbnail`
    : 'thumbnail';
  const base = `${getHtreePrefix()}/htree/${npub}/${encodedTreeName}/${path}`;
  const url = hashPrefix ? `${base}?v=${hashPrefix}` : base;
  return appendHtreeCacheBust(url);
}

/**
 * Check if file streaming is available
 * - Tauri: Always available (server runs on fixed port)
 * - Web: Requires service worker to be ready
 */
export async function isFileStreamingAvailable(): Promise<boolean> {
  if (isTauri()) {
    return true; // Server always runs on fixed port
  }

  // In browser, check service worker
  if (!('serviceWorker' in navigator)) return false;
  const registration = await navigator.serviceWorker.ready;
  return !!registration.active;
}
