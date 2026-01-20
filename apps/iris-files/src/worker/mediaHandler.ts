/**
 * Media Streaming Handler for Hashtree Worker
 *
 * Handles media requests from the service worker via MessagePort.
 * Supports both direct CID-based requests and path-based requests with live streaming.
 */

import type { HashTree } from '../../../../ts/packages/hashtree/src/hashtree';
import type { CID } from '../types';
import type { MediaRequestByCid, MediaRequestByPath, MediaResponse } from './protocol';
import { getCachedRoot } from './treeRootCache';
import { subscribeToTreeRoots } from './treeRootSubscription';
import { getErrorMessage } from '../utils/errorMessage';
import { nhashDecode } from '../../../../ts/packages/hashtree/src/nhash';
import { nip19 } from 'nostr-tools';

// Thumbnail filename patterns to look for (in priority order)
const THUMBNAIL_PATTERNS = ['thumbnail.jpg', 'thumbnail.webp', 'thumbnail.png', 'thumbnail.jpeg'];

/**
 * SW FileRequest format (from service worker)
 */
interface SwFileRequest {
  type: 'hashtree-file';
  requestId: string;
  npub?: string;
  nhash?: string;
  treeName?: string;
  path: string;
  start: number;
  end?: number;
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

// Timeout for considering a stream "done" (no updates)
const LIVE_STREAM_TIMEOUT = 10000; // 10 seconds
const ROOT_WAIT_TIMEOUT_MS = 15000;
const ROOT_WAIT_INTERVAL_MS = 200;

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

let mediaPort: MessagePort | null = null;
let tree: HashTree | null = null;

/**
 * Initialize the media handler with the HashTree instance
 */
export function initMediaHandler(hashTree: HashTree): void {
  tree = hashTree;
}

/**
 * Register a MessagePort from the service worker for media streaming
 */
export function registerMediaPort(port: MessagePort): void {
  mediaPort = port;

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
      const resolved = await tree.resolvePath(cid, filePath);
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
        const resolved = await tree.resolvePath(cid, filePath);
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

  const { requestId, npub, nhash, treeName, path, start, end, mimeType, download } = req;

  try {
    let cid: CID | null = null;

    if (nhash) {
      // Direct nhash request - decode to CID
      const rootCid = nhashDecode(nhash);

      // If path provided AND it contains a slash, navigate within the nhash directory
      // Single filename without slashes is just a hint for MIME type - use rootCid directly
      if (path && path.includes('/')) {
        const entry = await tree.resolvePath(rootCid, path);
        if (!entry) {
          sendSwError(requestId, 404, `File not found: ${path}`);
          return;
        }
        cid = entry.cid;
      } else if (path && !path.includes('/')) {
        // Try to resolve as file within directory first
        const entry = await tree.resolvePath(rootCid, path);
        cid = entry ? entry.cid : rootCid;
      } else {
        cid = rootCid;
      }
    } else if (npub && treeName) {
      // Npub-based request - resolve through cached root
      const rootCid = await waitForCachedRoot(npub, treeName);
      if (!rootCid) {
        sendSwError(requestId, 404, 'Tree not found');
        return;
      }

      // Handle thumbnail requests without extension
      let resolvedPath = path || '';
      if (resolvedPath.endsWith('/thumbnail') || resolvedPath === 'thumbnail') {
        const dirPath = resolvedPath.endsWith('/thumbnail')
          ? resolvedPath.slice(0, -'/thumbnail'.length)
          : '';
        const actualPath = await findThumbnailInDir(rootCid, dirPath);
        if (actualPath) {
          resolvedPath = actualPath;
        }
      }

      // Navigate to file
      if (resolvedPath) {
        const entry = await tree.resolvePath(rootCid, resolvedPath);
        if (!entry) {
          sendSwError(requestId, 404, 'File not found');
          return;
        }
        cid = entry.cid;
      } else {
        cid = rootCid;
      }
    }

    if (!cid) {
      sendSwError(requestId, 400, 'Invalid request');
      return;
    }

    // Get file size
    const totalSize = await getFileSize(cid);
    if (totalSize === null) {
      sendSwError(requestId, 404, 'File data not found');
      return;
    }

    // Stream the content
    await streamSwResponse(requestId, cid, totalSize, {
      npub,
      path,
      start,
      end,
      mimeType,
      download,
    });
  } catch (err) {
    sendSwError(requestId, 500, getErrorMessage(err));
  }
}

async function waitForCachedRoot(npub: string, treeName: string): Promise<CID | null> {
  let cached = await getCachedRoot(npub, treeName);
  if (cached) return cached;

  const pubkey = decodeNpubToPubkey(npub);
  if (pubkey) {
    subscribeToTreeRoots(pubkey);
  }

  const deadline = Date.now() + ROOT_WAIT_TIMEOUT_MS;
  while (Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, ROOT_WAIT_INTERVAL_MS));
    cached = await getCachedRoot(npub, treeName);
    if (cached) return cached;
  }

  return null;
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

/**
 * Send error response to SW
 */
function sendSwError(requestId: string, status: number, message: string): void {
  if (!mediaPort) return;
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
}

/**
 * Find actual thumbnail file in a directory
 */
async function findThumbnailInDir(rootCid: CID, dirPath: string): Promise<string | null> {
  if (!tree) return null;

  try {
    // Get directory CID
    const dirEntry = dirPath
      ? await tree.resolvePath(rootCid, dirPath)
      : { cid: rootCid };
    if (!dirEntry) return null;

    // List directory contents
    const entries = await tree.listDirectory(dirEntry.cid);
    if (!entries) return null;

    // Find first matching thumbnail pattern
    for (const pattern of THUMBNAIL_PATTERNS) {
      if (entries.some(e => e.name === pattern)) {
        return dirPath ? `${dirPath}/${pattern}` : pattern;
      }
    }

    return null;
  } catch {
    return null;
  }
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
    mimeType?: string;
    download?: boolean;
  }
): Promise<void> {
  if (!tree || !mediaPort) return;

  const { npub, path, start = 0, end, mimeType = 'application/octet-stream', download } = options;

  const rangeStart = start;
  const rangeEnd = end !== undefined ? Math.min(end, totalSize - 1) : totalSize - 1;
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
  const isRangeRequest = end !== undefined || start > 0;
  const status = isRangeRequest ? 206 : 200;
  if (isRangeRequest) {
    headers['Content-Range'] = `bytes ${rangeStart}-${rangeEnd}/${totalSize}`;
  }

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
