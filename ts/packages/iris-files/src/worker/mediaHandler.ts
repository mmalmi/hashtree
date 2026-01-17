/**
 * Media Streaming Handler for Hashtree Worker
 *
 * Handles media requests from the service worker via MessagePort.
 * Supports both direct CID-based requests and path-based requests with live streaming.
 */

import type { HashTree } from '../../../hashtree/src/hashtree';
import type { CID } from '../types';
import type { MediaRequestByCid, MediaRequestByPath, MediaResponse } from './protocol';
import { getCachedRoot } from './treeRootCache';
import { getErrorMessage } from '../utils/errorMessage';

// Timeout for considering a stream "done" (no updates)
const LIVE_STREAM_TIMEOUT = 10000; // 10 seconds

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

    if (req.type === 'media') {
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
    let cid = await getCachedRoot(npub, treeName);

    if (!cid) {
      // Not in cache - try to resolve via Nostr subscription
      // For now, just return error. Full implementation would subscribe and wait.
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
