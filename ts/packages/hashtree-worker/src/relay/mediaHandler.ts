// @ts-nocheck
/**
 * Media Streaming Handler for Hashtree Worker
 *
 * Handles media requests from the service worker via MessagePort.
 * Supports both direct CID-based requests and path-based requests with live streaming.
 */

import type { HashTree, CID } from '@hashtree/core';
import type { MediaRequestByCid, MediaRequestByPath, MediaResponse } from './protocol';
import { getCachedRoot, onCachedRootUpdate } from './treeRootCache';
import { resolveTreeRootNow, subscribeToTreeRoots } from './treeRootSubscription';
import { getErrorMessage } from './utils/errorMessage';
import { nhashDecode, toHex } from '@hashtree/core';
import { readTreeEventSnapshot, resolveSnapshotRootCid } from '@hashtree/nostr';
import { nip19 } from 'nostr-tools';
import { LRUCache } from './utils/lruCache';

// Thumbnail filename patterns to look for (in priority order)
const THUMBNAIL_PATTERNS = ['thumbnail.jpg', 'thumbnail.webp', 'thumbnail.png', 'thumbnail.jpeg'];
const PLAYABLE_MEDIA_EXTENSION_SET = new Set([
  '.mp4',
  '.webm',
  '.mkv',
  '.mov',
  '.avi',
  '.m4v',
  '.ogv',
  '.3gp',
  '.mp3',
  '.wav',
  '.flac',
  '.m4a',
  '.aac',
  '.ogg',
  '.oga',
  '.opus',
]);

const MIME_TYPES: Record<string, string> = {
  mp4: 'video/mp4',
  m4v: 'video/mp4',
  webm: 'video/webm',
  ogg: 'video/ogg',
  ogv: 'video/ogg',
  mov: 'video/quicktime',
  avi: 'video/x-msvideo',
  mkv: 'video/x-matroska',
  mp3: 'audio/mpeg',
  wav: 'audio/wav',
  flac: 'audio/flac',
  m4a: 'audio/mp4',
  aac: 'audio/aac',
  oga: 'audio/ogg',
  opus: 'audio/ogg',
  jpg: 'image/jpeg',
  jpeg: 'image/jpeg',
  png: 'image/png',
  webp: 'image/webp',
};

/**
 * SW FileRequest format (from service worker)
 */
interface SwFileRequest {
  type: 'hashtree-file';
  requestId: string;
  npub?: string;
  nhash?: string;
  snapshot?: boolean;
  linkKey?: string | null;
  treeName?: string;
  path: string;
  start: number;
  end?: number;
  rangeHeader?: string | null;
  mimeType: string;
  download?: boolean;
}

/**
 * Extended response with HTTP headers for SW
 */
interface SwFileResponse {
  type: 'headers' | 'chunk' | 'done' | 'error';
  requestId: string;
  status?: number;
  headers?: Record<string, string>;
  totalSize?: number;
  data?: Uint8Array;
  message?: string;
}

interface ResolvedRootEntry {
  cid: CID;
  size?: number;
  path?: string;
}

interface ResolvedThumbnailLookup extends ResolvedRootEntry {
  path: string;
}

interface CachedLookupValue<T> {
  value: T;
  expiresAt: number;
}

interface AsyncLookupCache<T> {
  values: LRUCache<string, CachedLookupValue<T>>;
  inflight: Map<string, Promise<T>>;
}

// Timeout for considering a stream "done" (no updates)
const LIVE_STREAM_TIMEOUT = 10000; // 10 seconds
const ROOT_WAIT_TIMEOUT_MS = 15000;
const DIRECTORY_PROBE_TIMEOUT_MS = 1000;
const IMMUTABLE_LOOKUP_CACHE_HIT_TTL_MS = 5 * 60 * 1000;
const IMMUTABLE_LOOKUP_CACHE_MISS_TTL_MS = 1000;
const DIRECTORY_CACHE_SIZE = 1024;
const RESOLVED_ENTRY_CACHE_SIZE = 2048;
const MUTABLE_RESOLVED_ENTRY_CACHE_SIZE = 2048;
const FILE_SIZE_CACHE_SIZE = 1024;
const THUMBNAIL_PATH_CACHE_SIZE = 1024;

// Chunk size for streaming to media port
const MEDIA_CHUNK_SIZE = 256 * 1024; // 256KB chunks - matches videoChunker's firstChunkSize

// Active media streams (for live streaming - can receive updates)
interface ActiveStream {
  requestId: string;
  npub: string;
  path: string;
  offset: number;
  cancelled: boolean;
}

const activeMediaStreams = new Map<string, ActiveStream>();
const inflightRootWaits = new Map<string, Promise<CID | null>>();
const directoryLookupCache = createAsyncLookupCache(DIRECTORY_CACHE_SIZE);
const resolvedEntryLookupCache = createAsyncLookupCache(RESOLVED_ENTRY_CACHE_SIZE);
const mutableResolvedEntryLookupCache = createAsyncLookupCache(MUTABLE_RESOLVED_ENTRY_CACHE_SIZE);
const fileSizeLookupCache = createAsyncLookupCache(FILE_SIZE_CACHE_SIZE);
const thumbnailPathLookupCache = createAsyncLookupCache(THUMBNAIL_PATH_CACHE_SIZE);

let mediaPort: MessagePort | null = null;
let tree: HashTree | null = null;
let mediaDebugEnabled = false;

function logMediaDebug(event: string, data?: Record<string, unknown>): void {
  if (!mediaDebugEnabled) return;
  if (data) {
    console.log(`[WorkerMedia] ${event}`, data);
  } else {
    console.log(`[WorkerMedia] ${event}`);
  }
}

function createAsyncLookupCache<T>(maxEntries: number): AsyncLookupCache<T> {
  return {
    values: new LRUCache<string, CachedLookupValue<T>>(maxEntries),
    inflight: new Map<string, Promise<T>>(),
  };
}

function clearAsyncLookupCache<T>(cache: AsyncLookupCache<T>): void {
  cache.values.clear();
  cache.inflight.clear();
}

function clearMediaLookupCaches(): void {
  inflightRootWaits.clear();
  clearAsyncLookupCache(directoryLookupCache);
  clearAsyncLookupCache(resolvedEntryLookupCache);
  clearAsyncLookupCache(mutableResolvedEntryLookupCache);
  clearAsyncLookupCache(fileSizeLookupCache);
  clearAsyncLookupCache(thumbnailPathLookupCache);
}

function getLookupTtlMs<T>(value: T | null | undefined): number {
  return value == null ? IMMUTABLE_LOOKUP_CACHE_MISS_TTL_MS : IMMUTABLE_LOOKUP_CACHE_HIT_TTL_MS;
}

async function loadCachedLookup<T>(
  cache: AsyncLookupCache<T>,
  key: string,
  loader: () => Promise<T>,
  ttlMsForValue: (value: T) => number,
): Promise<T> {
  const now = Date.now();
  const cached = cache.values.get(key);
  if (cached && cached.expiresAt > now) {
    return cached.value;
  }
  if (cached) {
    cache.values.delete(key);
  }

  const inflight = cache.inflight.get(key);
  if (inflight) {
    return inflight;
  }

  const pending = loader()
    .then((value) => {
      const ttlMs = ttlMsForValue(value);
      if (ttlMs > 0) {
        cache.values.set(key, {
          value,
          expiresAt: Date.now() + ttlMs,
        });
      }
      return value;
    })
    .finally(() => {
      cache.inflight.delete(key);
    });

  cache.inflight.set(key, pending);
  return pending;
}

function cidCacheKey(cid: CID): string {
  return cid.key ? `${toHex(cid.hash)}?k=${toHex(cid.key)}` : toHex(cid.hash);
}

function sameCid(a: CID | null | undefined, b: CID | null | undefined): boolean {
  if (!a && !b) return true;
  if (!a || !b) return false;
  return cidCacheKey(a) === cidCacheKey(b);
}

async function resolveSnapshotRoot(nhash: string, linkKey?: string | null): Promise<CID | null> {
  if (!tree) return null;

  const snapshotCid = nhashDecode(nhash);
  const snapshot = await readTreeEventSnapshot(tree, nip19, snapshotCid);
  if (!snapshot) {
    return null;
  }

  return resolveSnapshotRootCid(snapshot, linkKey);
}

/**
 * Initialize the media handler with the HashTree instance
 */
export function initMediaHandler(hashTree: HashTree): void {
  tree = hashTree;
  clearMediaLookupCaches();
}

/**
 * Register a MessagePort from the service worker for media streaming
 */
export function registerMediaPort(port: MessagePort, debug?: boolean): void {
  mediaPort = port;
  mediaDebugEnabled = !!debug;
  port.start?.();

  port.onmessage = async (e: MessageEvent) => {
    const req = e.data;

    if (req.type === 'hashtree-file') {
      // SW file request format (direct from service worker)
      await handleSwFileRequest(req);
    } else if (req.type === 'media') {
      await handleMediaRequestByCid(req);
    } else if (req.type === 'mediaByPath') {
      await handleMediaRequestByPath(req);
    } else if (req.type === 'cancelMedia') {
      // Cancel an active stream
      const stream = activeMediaStreams.get(req.requestId);
      if (stream) {
        stream.cancelled = true;
        activeMediaStreams.delete(req.requestId);
      }
    }
  };

  console.log('[Worker] Media port registered');
  logMediaDebug('port:registered', { debug: mediaDebugEnabled });
}

/**
 * Handle direct CID-based media request
 */
async function handleMediaRequestByCid(req: MediaRequestByCid): Promise<void> {
  if (!tree || !mediaPort) return;

  const { requestId, cid: cidHex, start, end, mimeType } = req;

  try {
    // Convert hex CID to proper CID object
    const hash = new Uint8Array(cidHex.length / 2);
    for (let i = 0; i < hash.length; i++) {
      hash[i] = parseInt(cidHex.substr(i * 2, 2), 16);
    }
    const cid = { hash };

    // Get file size first
    const totalSize = await tree.getSize(hash);

    // Send headers
    mediaPort.postMessage({
      type: 'headers',
      requestId,
      totalSize,
      mimeType: mimeType || 'application/octet-stream',
      isLive: false,
    } as MediaResponse);

    // Read range and stream chunks
    const data = await tree.readFileRange(cid, start, end);
    if (data) {
      await streamChunksToPort(requestId, data);
    } else {
      mediaPort.postMessage({
        type: 'error',
        requestId,
        message: 'File not found',
      } as MediaResponse);
    }
  } catch (err) {
    mediaPort.postMessage({
      type: 'error',
      requestId,
      message: getErrorMessage(err),
    } as MediaResponse);
  }
}

/**
 * Handle npub/path-based media request (supports live streaming)
 */
async function handleMediaRequestByPath(req: MediaRequestByPath): Promise<void> {
  if (!tree || !mediaPort) return;

  const { requestId, npub, path, start, mimeType } = req;

  try {
    // Parse path to get tree name
    const pathParts = path.split('/').filter(Boolean);
    const treeName = pathParts[0] || 'public';
    const filePath = pathParts.slice(1).join('/');

    // Resolve npub to current CID
    let cid = await waitForCachedRoot(npub, treeName);
    if (!cid) {
      mediaPort.postMessage({
        type: 'error',
        requestId,
        message: `Tree root not found for ${npub}/${treeName}`,
      } as MediaResponse);
      return;
    }

    // Navigate to file within tree if path specified
    if (filePath) {
      const resolved = await resolveMutableTreeEntry(npub, treeName, filePath, {
        allowSingleSegmentRootFallback: false,
        expectedMimeType: mimeType,
      });
      if (!resolved) {
        mediaPort.postMessage({
          type: 'error',
          requestId,
          message: `File not found: ${filePath}`,
        } as MediaResponse);
        return;
      }
      cid = resolved.cid;
    }

    // Get file size
    const totalSize = await tree.getSize(cid.hash);

    // Send headers (isLive will be determined by watching for updates)
    mediaPort.postMessage({
      type: 'headers',
      requestId,
      totalSize,
      mimeType: mimeType || 'application/octet-stream',
      isLive: false, // Will update if we detect changes
    } as MediaResponse);

    // Stream initial content
    const data = await tree.readFileRange(cid, start);
    let offset = start;

    if (data) {
      await streamChunksToPort(requestId, data, false); // Don't close yet
      offset += data.length;
    }

    // Register for live updates
    const streamInfo: ActiveStream = {
      requestId,
      npub,
      path,
      offset,
      cancelled: false,
    };
    activeMediaStreams.set(requestId, streamInfo);

    // Set up tree root watcher for this npub
    // When root changes, we'll check if this file has new data
    watchTreeRootForStream(npub, treeName, filePath, streamInfo);
  } catch (err) {
    mediaPort.postMessage({
      type: 'error',
      requestId,
      message: getErrorMessage(err),
    } as MediaResponse);
  }
}

/**
 * Stream data chunks to media port
 */
async function streamChunksToPort(
  requestId: string,
  data: Uint8Array,
  sendDone = true
): Promise<void> {
  if (!mediaPort) return;

  for (let offset = 0; offset < data.length; offset += MEDIA_CHUNK_SIZE) {
    const chunk = data.slice(offset, offset + MEDIA_CHUNK_SIZE);
    mediaPort.postMessage(
      { type: 'chunk', requestId, data: chunk } as MediaResponse,
      [chunk.buffer]
    );
  }

  if (sendDone) {
    mediaPort.postMessage({ type: 'done', requestId } as MediaResponse);
  }
}

/**
 * Watch for tree root updates and push new data to stream
 */
function watchTreeRootForStream(
  npub: string,
  treeName: string,
  filePath: string,
  streamInfo: ActiveStream
): void {
  let lastActivity = Date.now();
  let timeoutId: ReturnType<typeof setTimeout> | null = null;

  const checkForUpdates = async () => {
    if (streamInfo.cancelled || !tree || !mediaPort) {
      cleanup();
      return;
    }

    // Check if stream timed out
    if (Date.now() - lastActivity > LIVE_STREAM_TIMEOUT) {
      // No updates for a while, close the stream
      mediaPort.postMessage({
        type: 'done',
        requestId: streamInfo.requestId,
      } as MediaResponse);
      cleanup();
      return;
    }

    try {
      // Get current root
      const cid = await getCachedRoot(npub, treeName);
      if (!cid) {
        scheduleNext();
        return;
      }

      // Navigate to file
      let fileCid: CID = cid;
      if (filePath) {
        const resolved = await resolvePathFromDirectoryListings(cid, filePath);
        if (!resolved) {
          scheduleNext();
          return;
        }
        fileCid = resolved.cid;
      }

      // Check for new data
      const totalSize = await tree.getSize(fileCid.hash);
      if (totalSize > streamInfo.offset) {
        // New data available!
        lastActivity = Date.now();
        const newData = await tree.readFileRange(fileCid, streamInfo.offset);
        if (newData && newData.length > 0) {
          await streamChunksToPort(streamInfo.requestId, newData, false);
          streamInfo.offset += newData.length;
        }
      }
    } catch {
      // Ignore errors, just try again
    }

    scheduleNext();
  };

  const scheduleNext = () => {
    if (!streamInfo.cancelled) {
      timeoutId = setTimeout(checkForUpdates, 1000); // Check every second
    }
  };

  const cleanup = () => {
    if (timeoutId) clearTimeout(timeoutId);
    activeMediaStreams.delete(streamInfo.requestId);
  };

  // Start watching
  scheduleNext();
}

/**
 * Handle file request from service worker (hashtree-file format)
 * This is the main entry point for direct SW → Worker communication
 */
async function handleSwFileRequest(req: SwFileRequest): Promise<void> {
  if (!tree || !mediaPort) return;

  const { requestId, npub, nhash, snapshot, linkKey, treeName, path, start, end, rangeHeader, mimeType, download } = req;
  logMediaDebug('sw:request', {
    requestId,
    npub: npub ?? null,
    nhash: nhash ?? null,
    snapshot: !!snapshot,
    hasLinkKey: !!linkKey,
    treeName: treeName ?? null,
    path,
    start,
    end: end ?? null,
    rangeHeader: rangeHeader ?? null,
    mimeType,
    download: !!download,
  });

  try {
    let resolvedEntry: ResolvedRootEntry | null = null;

    if (nhash) {
      // Direct nhash request - decode to CID or resolve signed snapshot permalink.
      const rootCid = snapshot
        ? await resolveSnapshotRoot(nhash, linkKey)
        : nhashDecode(nhash);
      if (!rootCid) {
        sendSwError(requestId, snapshot ? 403 : 404, snapshot ? 'Tree snapshot could not be resolved' : `File not found: ${path}`);
        return;
      }
      resolvedEntry = await resolveEntryWithinRoot(rootCid, path || '', {
        allowSingleSegmentRootFallback: true,
        expectedMimeType: mimeType,
      });
      if (!resolvedEntry) {
        sendSwError(requestId, 404, `File not found: ${path}`);
        return;
      }
    } else if (npub && treeName) {
      // Npub-based request - resolve through mutable root history when needed
      resolvedEntry = await resolveMutableTreeEntry(npub, treeName, path || '', {
        allowSingleSegmentRootFallback: false,
        expectedMimeType: mimeType,
      });
      if (!resolvedEntry) {
        sendSwError(requestId, 404, 'File not found');
        return;
      }
    }

    if (!resolvedEntry?.cid) {
      sendSwError(requestId, 400, 'Invalid request');
      return;
    }

    // Get file size
    // Directory listings may report 0 for non-empty files when the actual byte size
    // is not embedded in the tree node metadata. Treat that as unknown and resolve
    // the real size from the file itself before building HTTP headers.
    const knownSize = typeof resolvedEntry.size === 'number' && resolvedEntry.size > 0
      ? resolvedEntry.size
      : null;
    const effectivePath = resolvedEntry.path ?? path;
    const effectiveMimeType = (
      mimeType === 'application/octet-stream'
      || isThumbnailAliasPath(path)
      || isVideoAliasPath(path)
    )
      ? guessMimeTypeFromPath(effectivePath)
      : mimeType;

    const totalSize = knownSize ?? await getFileSize(resolvedEntry.cid);
    if (totalSize === null) {
      const canStreamUnknownSizeDownload = !!download
        && !rangeHeader
        && start === 0
        && end === undefined
        && (typeof tree.readFileStream === 'function' || typeof tree.readFileRange === 'function');
      if (canStreamUnknownSizeDownload) {
        await streamUnknownSizeSwResponse(requestId, resolvedEntry.cid, {
          npub,
          path: effectivePath,
          mimeType: effectiveMimeType,
          download,
        });
        return;
      }

      const canBufferWholeFile = !rangeHeader && start === 0 && end === undefined;
      if (!canBufferWholeFile || typeof tree.readFile !== 'function') {
        sendSwError(requestId, 404, 'File data not found');
        return;
      }
      const fullData = await tree.readFile(resolvedEntry.cid);
      if (!fullData) {
        sendSwError(requestId, 404, 'File data not found');
        return;
      }
      sendBufferedSwResponse(requestId, fullData, {
        npub,
        path: effectivePath,
        mimeType: effectiveMimeType,
        download,
      });
      return;
    }

    // Stream the content
    await streamSwResponse(requestId, resolvedEntry.cid, totalSize, {
      npub,
      path: effectivePath,
      start,
      end,
      rangeHeader,
      mimeType: effectiveMimeType,
      download,
    });
  } catch (err) {
    sendSwError(requestId, 500, getErrorMessage(err));
  }
}

async function waitForCachedRoot(npub: string, treeName: string): Promise<CID | null> {
  const cached = await getCachedRoot(npub, treeName);
  if (cached) return cached;

  const cacheKey = `${npub}/${treeName}`;
  const inflight = inflightRootWaits.get(cacheKey);
  if (inflight) {
    return inflight;
  }

  const pubkey = decodeNpubToPubkey(npub);
  if (pubkey) {
    subscribeToTreeRoots(pubkey);
  }

  const pending = new Promise<CID | null>((resolve) => {
    let settled = false;
    const finish = (cid: CID | null) => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      unsubscribe();
      resolve(cid);
    };

    const unsubscribe = onCachedRootUpdate((updatedNpub, updatedTreeName, cid) => {
      if (updatedNpub === npub && updatedTreeName === treeName && cid) {
        finish(cid);
      }
    });

    const timeout = setTimeout(() => {
      logMediaDebug('root:timeout', { npub, treeName });
      finish(null);
    }, ROOT_WAIT_TIMEOUT_MS);

    void getCachedRoot(npub, treeName).then((current) => {
      if (current) {
        finish(current);
      }
    });
    void resolveTreeRootNow(npub, treeName, ROOT_WAIT_TIMEOUT_MS).then((resolved) => {
      if (resolved) {
        finish(resolved);
      }
    }).catch(() => {});
  }).finally(() => {
    inflightRootWaits.delete(cacheKey);
  });

  inflightRootWaits.set(cacheKey, pending);
  return pending;
}

async function resolveMutableTreeEntry(
  npub: string,
  treeName: string,
  path: string,
  options?: { allowSingleSegmentRootFallback?: boolean; expectedMimeType?: string },
): Promise<ResolvedRootEntry | null> {
  const currentRoot = await waitForCachedRoot(npub, treeName);
  const cacheKey = [
    npub,
    treeName,
    currentRoot ? cidCacheKey(currentRoot) : 'none',
    options?.allowSingleSegmentRootFallback ? 'root-fallback' : 'strict',
    options?.expectedMimeType ?? '',
    path,
  ].join('|');

  return loadCachedLookup(
    mutableResolvedEntryLookupCache,
    cacheKey,
    async () => {
      if (currentRoot) {
        const currentEntry = await resolveEntryWithinRoot(currentRoot, path, options);
        if (currentEntry) {
          return currentEntry;
        }
      }

      return null;
    },
    (value) => getLookupTtlMs(value),
  );
}

function decodeNpubToPubkey(npub: string): string | null {
  if (!npub.startsWith('npub1')) return null;
  try {
    const decoded = nip19.decode(npub);
    if (decoded.type !== 'npub') return null;
    return decoded.data as string;
  } catch {
    return null;
  }
}

async function resolveEntryWithinRoot(
  rootCid: CID,
  path: string,
  options?: { allowSingleSegmentRootFallback?: boolean; expectedMimeType?: string }
): Promise<ResolvedRootEntry | null> {
  if (!tree) return null;

  const cacheKey = [
    cidCacheKey(rootCid),
    options?.allowSingleSegmentRootFallback ? 'root-fallback' : 'strict',
    path,
  ].join('|');

  return loadCachedLookup(
    resolvedEntryLookupCache,
    cacheKey,
    async () => {
      if (!path) {
        return { cid: rootCid };
      }

      const resolvedThumbnail = await resolveThumbnailAliasEntry(rootCid, path);
      if (resolvedThumbnail) {
        return resolvedThumbnail;
      }
      if (isThumbnailAliasPath(path)) {
        return null;
      }

      const resolvedPlayable = await resolvePlayableAliasEntry(rootCid, path);
      if (resolvedPlayable) {
        return resolvedPlayable;
      }
      if (isVideoAliasPath(path)) {
        return null;
      }

      if (
        options?.allowSingleSegmentRootFallback &&
        await canFallbackToRootBlob(rootCid, path, path, options?.expectedMimeType)
      ) {
        const isDirectory = await canListDirectory(rootCid);
        if (!isDirectory) {
          return { cid: rootCid };
        }
      }

      if (
        options?.allowSingleSegmentRootFallback &&
        !path.includes('/') &&
        isExactThumbnailFilenamePath(path) &&
        !(await canListDirectory(rootCid))
      ) {
        return null;
      }

      const entry = await resolvePathFromDirectoryListings(rootCid, path);
      if (entry) {
        return entry;
      }

      if (
        options?.allowSingleSegmentRootFallback &&
        await canFallbackToRootBlob(rootCid, path, path, options?.expectedMimeType)
      ) {
        return { cid: rootCid };
      }

      return null;
    },
    (value) => getLookupTtlMs(value),
  );
}

async function resolveCidWithinRoot(
  rootCid: CID,
  path: string,
  options?: { allowSingleSegmentRootFallback?: boolean; expectedMimeType?: string }
): Promise<CID | null> {
  return (await resolveEntryWithinRoot(rootCid, path, options))?.cid ?? null;
}

async function resolvePathFromDirectoryListings(
  rootCid: CID,
  path: string
): Promise<ResolvedRootEntry | null> {
  if (!tree) return null;

  const parts = path.split('/').filter(Boolean);
  if (parts.length === 0) {
    return { cid: rootCid };
  }

  let currentCid = rootCid;
  for (let i = 0; i < parts.length; i += 1) {
    const entries = await loadDirectoryListing(currentCid);
    if (!entries) {
      return null;
    }

    const entry = entries.find((candidate) => candidate.name === parts[i]);
    if (!entry?.cid) {
      return null;
    }

    if (i === parts.length - 1) {
      return { cid: entry.cid, size: entry.size, path: parts.slice(0, i + 1).join('/') };
    }

    currentCid = entry.cid;
  }

  return null;
}

function hasImageBlobSignature(blob: Uint8Array): boolean {
  if (blob.length >= 3 && blob[0] === 0xff && blob[1] === 0xd8 && blob[2] === 0xff) {
    return true;
  }
  if (
    blob.length >= 8
    && blob[0] === 0x89
    && blob[1] === 0x50
    && blob[2] === 0x4e
    && blob[3] === 0x47
    && blob[4] === 0x0d
    && blob[5] === 0x0a
    && blob[6] === 0x1a
    && blob[7] === 0x0a
  ) {
    return true;
  }
  if (
    blob.length >= 12
    && blob[0] === 0x52
    && blob[1] === 0x49
    && blob[2] === 0x46
    && blob[3] === 0x46
    && blob[8] === 0x57
    && blob[9] === 0x45
    && blob[10] === 0x42
    && blob[11] === 0x50
  ) {
    return true;
  }
  if (
    blob.length >= 6
    && blob[0] === 0x47
    && blob[1] === 0x49
    && blob[2] === 0x46
    && blob[3] === 0x38
    && (blob[4] === 0x37 || blob[4] === 0x39)
    && blob[5] === 0x61
  ) {
    return true;
  }
  return false;
}

function readAscii(blob: Uint8Array, start: number, end: number): string {
  return String.fromCharCode(...blob.slice(start, end));
}

function sniffPlayableMediaExtension(blob: Uint8Array): string | null {
  if (!blob.length) return null;

  if (blob.length >= 12 && readAscii(blob, 4, 8) === 'ftyp') {
    const brand = readAscii(blob, 8, 12).toLowerCase();
    if (brand.startsWith('m4a')) return '.m4a';
    if (brand.startsWith('qt')) return '.mov';
    return '.mp4';
  }

  if (
    blob.length >= 4
    && blob[0] === 0x1a
    && blob[1] === 0x45
    && blob[2] === 0xdf
    && blob[3] === 0xa3
  ) {
    const lowerHeader = readAscii(blob, 0, Math.min(blob.length, 64)).toLowerCase();
    return lowerHeader.includes('webm') ? '.webm' : '.mkv';
  }

  if (blob.length >= 4 && readAscii(blob, 0, 4) === 'OggS') {
    return '.ogg';
  }

  if (blob.length >= 4 && readAscii(blob, 0, 4) === 'fLaC') {
    return '.flac';
  }

  if (
    blob.length >= 12
    && readAscii(blob, 0, 4) === 'RIFF'
    && readAscii(blob, 8, 12) === 'WAVE'
  ) {
    return '.wav';
  }

  if (blob.length >= 3 && readAscii(blob, 0, 3) === 'ID3') {
    return '.mp3';
  }

  if (blob.length >= 2 && blob[0] === 0xff && (blob[1] & 0xf6) === 0xf0) {
    return '.aac';
  }

  if (blob.length >= 2 && blob[0] === 0xff && (blob[1] & 0xe0) === 0xe0) {
    return '.mp3';
  }

  return null;
}

function guessMimeTypeFromPath(path: string | undefined): string {
  const ext = path?.split('.').pop()?.toLowerCase() ?? '';
  return MIME_TYPES[ext] || 'application/octet-stream';
}

async function canFallbackToRootBlob(
  rootCid: CID,
  resolvedPath: string,
  originalPath: string,
  expectedMimeType?: string,
): Promise<boolean> {
  if (resolvedPath !== originalPath) return false;
  if (resolvedPath.includes('/')) return false;
  if (isExactThumbnailFilenamePath(resolvedPath)) {
    if (!expectedMimeType?.startsWith('image/')) {
      return false;
    }
    if (!tree) {
      return false;
    }
    try {
      if (typeof tree.readFileRange === 'function') {
        const header = await tree.readFileRange(rootCid, 0, 64);
        return !!header && hasImageBlobSignature(header);
      }
      if (typeof tree.getBlob === 'function') {
        const blob = await tree.getBlob(rootCid.hash);
        return !!blob && hasImageBlobSignature(blob);
      }
      return false;
    } catch {
      return false;
    }
  }
  return /\.[A-Za-z0-9]{1,16}$/.test(resolvedPath);
}

function isThumbnailAliasPath(path: string): boolean {
  return path === 'thumbnail' || path.endsWith('/thumbnail');
}

function isVideoAliasPath(path: string): boolean {
  return path === 'video' || path.endsWith('/video');
}

function isExactThumbnailFilenamePath(path: string): boolean {
  const fileName = path.split('/').filter(Boolean).at(-1)?.toLowerCase() ?? '';
  return fileName.startsWith('thumbnail.');
}

function isImageFileName(fileName: string): boolean {
  const normalized = fileName.trim().toLowerCase();
  return normalized.endsWith('.jpg')
    || normalized.endsWith('.jpeg')
    || normalized.endsWith('.png')
    || normalized.endsWith('.webp');
}

function findThumbnailFileEntry(
  entries: Array<{ name: string; cid?: CID; size?: number }>
): { name: string; cid?: CID; size?: number } | null {
  for (const pattern of THUMBNAIL_PATTERNS) {
    const directMatch = entries.find((entry) => entry.name === pattern && entry.cid);
    if (directMatch?.cid) {
      return directMatch;
    }
  }

  return entries.find((entry) => isImageFileName(entry.name) && entry.cid) ?? null;
}

function resolveEmbeddedThumbnailLookup(
  value: unknown,
  entries: Array<{ name: string; cid?: CID; size?: number }>,
  dirPath: string,
): ResolvedThumbnailLookup | null {
  if (typeof value !== 'string') {
    return null;
  }

  const trimmed = value.trim();
  if (!trimmed) {
    return null;
  }

  if (trimmed.startsWith('nhash1')) {
    try {
      return {
        path: dirPath ? `${dirPath}/thumbnail` : 'thumbnail',
        cid: nhashDecode(trimmed),
      };
    } catch {
      return null;
    }
  }

  const normalized = trimmed
    .split('?')[0]
    ?.split('#')[0]
    ?.split('/')
    .filter(Boolean)
    .at(-1);
  if (!normalized) {
    return null;
  }

  const entry = entries.find((candidate) => candidate.name === normalized && candidate.cid);
  if (!entry?.cid) {
    return null;
  }

  return {
    path: dirPath ? `${dirPath}/${entry.name}` : entry.name,
    cid: entry.cid,
    size: entry.size,
  };
}

async function resolveThumbnailLookupFromMetadata(
  entries: Array<{ name: string; cid?: CID; size?: number; meta?: Record<string, unknown> }>,
  dirPath: string,
): Promise<ResolvedThumbnailLookup | null> {
  if (!tree) {
    return null;
  }

  const playableEntry = entries.find((entry) => isPlayableMediaFileName(entry.name));
  const playableThumbnail = playableEntry?.meta && typeof playableEntry.meta.thumbnail === 'string'
    ? playableEntry.meta.thumbnail
    : null;
  const embeddedPlayableThumbnail = resolveEmbeddedThumbnailLookup(playableThumbnail, entries, dirPath);
  if (embeddedPlayableThumbnail) {
    return embeddedPlayableThumbnail;
  }

  for (const metadataName of ['metadata.json', 'info.json']) {
    const metadataEntry = entries.find((entry) => entry.name === metadataName && entry.cid);
    if (!metadataEntry?.cid) {
      continue;
    }

    try {
      const metadataData = await tree.readFile(metadataEntry.cid);
      if (!metadataData) {
        continue;
      }

      const parsed = JSON.parse(new TextDecoder().decode(metadataData));
      const embeddedThumbnail = resolveEmbeddedThumbnailLookup(parsed?.thumbnail, entries, dirPath);
      if (embeddedThumbnail) {
        return embeddedThumbnail;
      }
    } catch {
      continue;
    }
  }

  return null;
}

async function findThumbnailLookupInEntries(
  entries: Array<{ name: string; cid?: CID; size?: number; meta?: Record<string, unknown> }>,
  dirPath: string,
): Promise<ResolvedThumbnailLookup | null> {
  const directThumbnail = findThumbnailFileEntry(entries);
  if (directThumbnail?.cid) {
    return {
      path: dirPath ? `${dirPath}/${directThumbnail.name}` : directThumbnail.name,
      cid: directThumbnail.cid,
      size: directThumbnail.size,
    };
  }

  return await resolveThumbnailLookupFromMetadata(entries, dirPath);
}

async function detectDirectPlayableLookup(
  cid: CID,
  dirPath: string,
): Promise<ResolvedRootEntry | null> {
  if (!tree || typeof tree.readFileRange !== 'function') {
    return null;
  }

  try {
    const header = await tree.readFileRange(cid, 0, 64);
    if (!(header instanceof Uint8Array) || header.length === 0) {
      return null;
    }

    const extension = sniffPlayableMediaExtension(header);
    if (!extension) {
      return null;
    }

    const fileName = `video${extension}`;
    return {
      cid,
      path: dirPath ? `${dirPath}/${fileName}` : fileName,
    };
  } catch {
    return null;
  }
}

function findPlayableLookupInEntries(
  entries: Array<{ name: string; cid?: CID; size?: number }>,
  dirPath: string,
): ResolvedRootEntry | null {
  const playableEntry = entries.find((entry) => isPlayableMediaFileName(entry.name) && entry.cid);
  if (!playableEntry?.cid) {
    return null;
  }

  return {
    cid: playableEntry.cid,
    size: playableEntry.size,
    path: dirPath ? `${dirPath}/${playableEntry.name}` : playableEntry.name,
  };
}

async function normalizeAliasPath(rootCid: CID, path: string): Promise<string> {
  if (!path) return '';
  const resolvedThumbnail = await resolveThumbnailAliasEntry(rootCid, path);
  if (resolvedThumbnail) {
    return resolvedThumbnail.path;
  }
  const resolvedPlayable = await resolvePlayableAliasEntry(rootCid, path);
  if (resolvedPlayable?.path) {
    return resolvedPlayable.path;
  }
  return path;
}

async function canListDirectory(rootCid: CID): Promise<boolean> {
  if (!tree) return false;
  try {
    if ('isDirectory' in tree && typeof tree.isDirectory === 'function') {
      return await tree.isDirectory(rootCid);
    }
    const entries = await listDirectoryWithProbeTimeout(rootCid);
    return Array.isArray(entries);
  } catch {
    return false;
  }
}

export const __test__ = {
  handleSwFileRequest,
  resolveCidWithinRoot,
  resolveMutableTreeEntry,
  normalizeAliasPath,
  canListDirectory,
  waitForCachedRoot,
};

async function resolveThumbnailAliasEntry(
  rootCid: CID,
  path: string
): Promise<ResolvedThumbnailLookup | null> {
  if (!isThumbnailAliasPath(path)) {
    return null;
  }

  const dirPath = path.endsWith('/thumbnail')
    ? path.slice(0, -'/thumbnail'.length)
    : '';
  return findThumbnailLookupInDir(rootCid, dirPath);
}

async function resolvePlayableAliasEntry(
  rootCid: CID,
  path: string
): Promise<ResolvedRootEntry | null> {
  if (!isVideoAliasPath(path)) {
    return null;
  }

  const dirPath = path.endsWith('/video')
    ? path.slice(0, -'/video'.length)
    : '';
  return findPlayableLookupInDir(rootCid, dirPath);
}

async function loadDirectoryListing(cid: CID): Promise<Awaited<ReturnType<HashTree['listDirectory']>> | null> {
  if (!tree) return null;
  return loadCachedLookup(
    directoryLookupCache,
    cidCacheKey(cid),
    async () => tree.listDirectory(cid),
    (value) => getLookupTtlMs(value),
  );
}

async function listDirectoryWithProbeTimeout(cid: CID): Promise<Awaited<ReturnType<HashTree['listDirectory']>> | null> {
  if (!tree) return null;
  return Promise.race([
    loadDirectoryListing(cid),
    new Promise<null>((resolve) => setTimeout(() => resolve(null), DIRECTORY_PROBE_TIMEOUT_MS)),
  ]);
}

/**
 * Send error response to SW
 */
function sendSwError(requestId: string, status: number, message: string): void {
  if (!mediaPort) return;
  logMediaDebug('sw:error', { requestId, status, message });
  mediaPort.postMessage({
    type: 'error',
    requestId,
    status,
    message,
  } as SwFileResponse);
}

/**
 * Get file size from CID (handles both chunked and single blob files)
 */
async function getFileSize(cid: CID): Promise<number | null> {
  if (!tree) return null;

  return loadCachedLookup(
    fileSizeLookupCache,
    cidCacheKey(cid),
    async () => {
      const treeNode = await tree.getTreeNode(cid);
      if (treeNode) {
        // Chunked file - sum link sizes from decrypted tree node
        return treeNode.links.reduce((sum, l) => sum + l.size, 0);
      }

      // Single blob - fetch to check existence and get size
      const blob = await tree.getBlob(cid.hash);
      if (!blob) return null;

      // For encrypted blobs, decrypted size = encrypted size - 16 (nonce overhead)
      return cid.key ? Math.max(0, blob.length - 16) : blob.length;
    },
    (value) => getLookupTtlMs(value),
  );
}

/**
 * Find actual thumbnail file in a directory
 */
async function findThumbnailLookupInDir(
  rootCid: CID,
  dirPath: string
): Promise<ResolvedThumbnailLookup | null> {
  if (!tree) return null;

  return loadCachedLookup(
    thumbnailPathLookupCache,
    `${cidCacheKey(rootCid)}|${dirPath}`,
    async () => {
      try {
        const dirEntry = dirPath
          ? await resolvePathFromDirectoryListings(rootCid, dirPath)
          : { cid: rootCid };
        if (!dirEntry) return null;

        const entries = await listDirectoryWithProbeTimeout(dirEntry.cid);
        if (!entries) return null;

        const rootThumbnail = await findThumbnailLookupInEntries(entries, dirPath);
        if (rootThumbnail) {
          return rootThumbnail;
        }

        const hasPlayableMediaFile = entries.some((entry) => isPlayableMediaFileName(entry.name));
        if (!hasPlayableMediaFile && entries.length > 0) {
          const sortedEntries = [...entries].sort((a, b) => a.name.localeCompare(b.name));
          for (const entry of sortedEntries.slice(0, 3)) {
            if (entry.name.endsWith('.json') || entry.name.endsWith('.txt')) {
              continue;
            }

            try {
              const subEntries = await listDirectoryWithProbeTimeout(entry.cid);
              if (!subEntries) {
                continue;
              }

              const prefix = dirPath ? `${dirPath}/${entry.name}` : entry.name;
              const nestedThumbnail = await findThumbnailLookupInEntries(subEntries, prefix);
              if (nestedThumbnail) {
                return nestedThumbnail;
              }
            } catch {
              continue;
            }
          }
        }

        return null;
      } catch {
        return null;
      }
    },
    (value) => getLookupTtlMs(value),
  );
}

async function findPlayableLookupInDir(
  rootCid: CID,
  dirPath: string
): Promise<ResolvedRootEntry | null> {
  if (!tree) return null;

  return loadCachedLookup(
    resolvedEntryLookupCache,
    `${cidCacheKey(rootCid)}|video|${dirPath}`,
    async () => {
      try {
        const dirEntry = dirPath
          ? await resolvePathFromDirectoryListings(rootCid, dirPath)
          : { cid: rootCid, path: '' };
        if (!dirEntry) return null;

        const entries = await listDirectoryWithProbeTimeout(dirEntry.cid);
        if (entries && entries.length > 0) {
          const directPlayable = findPlayableLookupInEntries(entries, dirPath);
          if (directPlayable) {
            return directPlayable;
          }

          const sortedEntries = [...entries].sort((a, b) => a.name.localeCompare(b.name));
          for (const entry of sortedEntries.slice(0, 3)) {
            if (entry.name.endsWith('.json') || entry.name.endsWith('.txt') || !entry.cid) {
              continue;
            }

            const prefix = dirPath ? `${dirPath}/${entry.name}` : entry.name;
            const subEntries = await listDirectoryWithProbeTimeout(entry.cid);
            if (!subEntries || subEntries.length === 0) {
              const directPlayableChild = await detectDirectPlayableLookup(entry.cid, prefix);
              if (directPlayableChild) {
                return directPlayableChild;
              }
              continue;
            }

            const nestedPlayable = findPlayableLookupInEntries(subEntries, prefix);
            if (nestedPlayable) {
              return nestedPlayable;
            }
          }
        }

        return await detectDirectPlayableLookup(dirEntry.cid, dirPath);
      } catch {
        return null;
      }
    },
    (value) => getLookupTtlMs(value),
  );
}

/**
 * Stream response to SW with proper HTTP headers
 */
async function streamSwResponse(
  requestId: string,
  cid: CID,
  totalSize: number,
  options: {
    npub?: string;
    path?: string;
    start?: number;
    end?: number;
    rangeHeader?: string | null;
    mimeType?: string;
    download?: boolean;
  }
): Promise<void> {
  if (!tree || !mediaPort) return;

  const { npub, path, start = 0, end, rangeHeader, mimeType = 'application/octet-stream', download } = options;

  let rangeStart = start;
  let rangeEnd = end !== undefined ? Math.min(end, totalSize - 1) : totalSize - 1;
  if (rangeHeader) {
    const parsedRange = parseHttpByteRange(rangeHeader, totalSize);
    if (parsedRange.kind === 'range') {
      rangeStart = parsedRange.range.start;
      rangeEnd = parsedRange.range.endInclusive;
    } else if (parsedRange.kind === 'unsatisfiable') {
      sendSwError(requestId, 416, `Range not satisfiable for ${totalSize} byte file`);
      return;
    }
  }
  const contentLength = rangeEnd - rangeStart + 1;

  // Build cache control header
  const isNpubRequest = !!npub;
  const isImage = mimeType.startsWith('image/');
  let cacheControl: string;
  if (!isNpubRequest) {
    cacheControl = 'public, max-age=31536000, immutable'; // nhash: immutable
  } else if (isImage) {
    cacheControl = 'public, max-age=60, stale-while-revalidate=86400';
  } else {
    cacheControl = 'no-cache, no-store, must-revalidate';
  }

  // Build headers
  const headers: Record<string, string> = {
    'Content-Type': mimeType,
    'Accept-Ranges': 'bytes',
    'Cache-Control': cacheControl,
    'Content-Length': String(contentLength),
  };

  if (download) {
    const filename = path || 'file';
    headers['Content-Disposition'] = `attachment; filename="${filename}"`;
  }

  // Determine status (206 for range requests)
  const isRangeRequest = !!rangeHeader || end !== undefined || start > 0;
  const status = isRangeRequest ? 206 : 200;
  if (isRangeRequest) {
    headers['Content-Range'] = `bytes ${rangeStart}-${rangeEnd}/${totalSize}`;
  }

  logMediaDebug('sw:response', {
    requestId,
    status,
    totalSize,
    rangeStart,
    rangeEnd,
  });

  // Send headers
  mediaPort.postMessage({
    type: 'headers',
    requestId,
    status,
    headers,
    totalSize,
  } as SwFileResponse);

  // Stream chunks
  let offset = rangeStart;
  while (offset <= rangeEnd) {
    const chunkEnd = Math.min(offset + MEDIA_CHUNK_SIZE - 1, rangeEnd);
    const chunk = await tree.readFileRange(cid, offset, chunkEnd + 1);

    if (!chunk) break;

    mediaPort.postMessage(
      { type: 'chunk', requestId, data: chunk } as SwFileResponse,
      [chunk.buffer]
    );

    offset = chunkEnd + 1;
  }

  // Signal done
  mediaPort.postMessage({ type: 'done', requestId } as SwFileResponse);
}

async function streamUnknownSizeSwResponse(
  requestId: string,
  cid: CID,
  options: {
    npub?: string;
    path?: string;
    mimeType?: string;
    download?: boolean;
  },
): Promise<void> {
  if (!tree || !mediaPort) return;

  const { npub, path, mimeType = 'application/octet-stream', download } = options;
  const isNpubRequest = !!npub;
  const isImage = mimeType.startsWith('image/');
  let cacheControl: string;
  if (!isNpubRequest) {
    cacheControl = 'public, max-age=31536000, immutable';
  } else if (isImage) {
    cacheControl = 'public, max-age=60, stale-while-revalidate=86400';
  } else {
    cacheControl = 'no-cache, no-store, must-revalidate';
  }

  const headers: Record<string, string> = {
    'Content-Type': mimeType,
    'Accept-Ranges': 'bytes',
    'Cache-Control': cacheControl,
  };

  if (download) {
    headers['Content-Disposition'] = `attachment; filename="${path || 'file'}"`;
  }

  mediaPort.postMessage({
    type: 'headers',
    requestId,
    status: 200,
    headers,
    totalSize: 0,
  } as SwFileResponse);

  if (typeof tree.readFileStream === 'function') {
    for await (const chunk of tree.readFileStream(cid)) {
      if (!chunk || chunk.byteLength === 0) continue;
      mediaPort.postMessage(
        { type: 'chunk', requestId, data: chunk } as SwFileResponse,
        [chunk.buffer],
      );
    }
  } else {
    let offset = 0;
    while (true) {
      const chunk = await tree.readFileRange(cid, offset, offset + MEDIA_CHUNK_SIZE);
      if (!chunk || chunk.byteLength === 0) break;
      mediaPort.postMessage(
        { type: 'chunk', requestId, data: chunk } as SwFileResponse,
        [chunk.buffer],
      );
      offset += chunk.byteLength;
      if (chunk.byteLength < MEDIA_CHUNK_SIZE) break;
    }
  }

  mediaPort.postMessage({ type: 'done', requestId } as SwFileResponse);
}

function sendBufferedSwResponse(
  requestId: string,
  data: Uint8Array,
  options: {
    npub?: string;
    path?: string;
    mimeType?: string;
    download?: boolean;
  },
): void {
  if (!mediaPort) return;

  const { npub, path, mimeType = 'application/octet-stream', download } = options;
  const isNpubRequest = !!npub;
  const isImage = mimeType.startsWith('image/');
  let cacheControl: string;
  if (!isNpubRequest) {
    cacheControl = 'public, max-age=31536000, immutable';
  } else if (isImage) {
    cacheControl = 'public, max-age=60, stale-while-revalidate=86400';
  } else {
    cacheControl = 'no-cache, no-store, must-revalidate';
  }

  const headers: Record<string, string> = {
    'Content-Type': mimeType,
    'Content-Length': String(data.length),
    'Cache-Control': cacheControl,
  };

  if (download) {
    headers['Content-Disposition'] = `attachment; filename="${path || 'file'}"`;
  }

  mediaPort.postMessage({
    type: 'headers',
    requestId,
    status: 200,
    headers,
    totalSize: data.length,
  } as SwFileResponse);

  mediaPort.postMessage(
    { type: 'chunk', requestId, data } as SwFileResponse,
    [data.buffer]
  );
  mediaPort.postMessage({ type: 'done', requestId } as SwFileResponse);
}

interface ResolvedByteRange {
  start: number;
  endInclusive: number;
}

type ParsedHttpRange =
  | { kind: 'range'; range: ResolvedByteRange }
  | { kind: 'unsatisfiable' }
  | { kind: 'unsupported' };

function parseHttpByteRange(
  rangeHeader: string | null | undefined,
  totalSize: number,
): ParsedHttpRange {
  if (!rangeHeader) return { kind: 'unsupported' };
  const bytesRange = rangeHeader.startsWith('bytes=')
    ? rangeHeader.slice('bytes='.length)
    : null;
  if (!bytesRange || bytesRange.includes(',')) return { kind: 'unsupported' };
  if (totalSize <= 0) return { kind: 'unsatisfiable' };

  const parts = bytesRange.split('-', 2);
  if (parts.length !== 2) return { kind: 'unsupported' };
  const [startPart, endPart] = parts;

  if (!startPart) {
    const suffixLength = Number.parseInt(endPart, 10);
    if (!Number.isFinite(suffixLength) || suffixLength <= 0) {
      return { kind: 'unsatisfiable' };
    }
    const clampedSuffix = Math.min(suffixLength, totalSize);
    return {
      kind: 'range',
      range: {
        start: totalSize - clampedSuffix,
        endInclusive: totalSize - 1,
      },
    };
  }

  const start = Number.parseInt(startPart, 10);
  if (!Number.isFinite(start) || start < 0 || start >= totalSize) {
    return { kind: 'unsatisfiable' };
  }

  const endInclusive = endPart ? Number.parseInt(endPart, 10) : totalSize - 1;
  if (!Number.isFinite(endInclusive) || endInclusive < start) {
    return { kind: 'unsatisfiable' };
  }

  return {
    kind: 'range',
    range: {
      start,
      endInclusive: Math.min(endInclusive, totalSize - 1),
    },
  };
}

function isPlayableMediaFileName(name: string): boolean {
  const normalized = name.trim().toLowerCase();
  if (!normalized || normalized.endsWith('/')) {
    return false;
  }
  if (normalized.startsWith('video.')) {
    return true;
  }

  const lastDot = normalized.lastIndexOf('.');
  if (lastDot === -1) {
    return false;
  }

  return PLAYABLE_MEDIA_EXTENSION_SET.has(normalized.slice(lastDot));
}
