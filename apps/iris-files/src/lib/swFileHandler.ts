/**
 * Service Worker File Request Handler
 *
 * Listens for file requests from the service worker and responds with data
 * streamed from the hashtree worker.
 *
 * Based on WebTorrent's BrowserServer pattern:
 * - SW posts requests via MessageChannel
 * - Main thread responds with headers, then streams chunks
 * - Uses pull-based streaming (SW requests each chunk)
 */

import { getTree } from '../store';
import { getLocalRootCache, getLocalRootKey } from '../treeRootCache';
import { getTreeRootSync, waitForTreeRoot } from '../stores/treeRoot';
import { nhashDecode, type CID } from 'hashtree';

// Thumbnail filename patterns to look for (in priority order)
const THUMBNAIL_PATTERNS = ['thumbnail.jpg', 'thumbnail.webp', 'thumbnail.png', 'thumbnail.jpeg'];

/** Timeout wrapper for async operations */
function withTimeout<T>(promise: Promise<T>, ms: number): Promise<T | null> {
  return Promise.race([
    promise,
    new Promise<null>(resolve => setTimeout(() => resolve(null), ms))
  ]);
}

/**
 * Find actual thumbnail file in a directory
 * Returns the full path with correct extension, or null if not found
 * For playlist directories (no root thumbnail), tries first subdirectory
 * Times out after 5 seconds per operation to avoid blocking thumbnail loads
 */
async function findThumbnailInDir(rootCid: CID, dirPath: string): Promise<string | null> {
  const tree = getTree();

  try {
    // Get directory CID (with timeout for network fetches)
    const dirEntry = dirPath
      ? await withTimeout(tree.resolvePath(rootCid, dirPath), 5000)
      : { cid: rootCid };
    if (!dirEntry) {
      return null;
    }

    // List directory contents (with timeout for network fetches)
    const entries = await withTimeout(tree.listDirectory(dirEntry.cid), 5000);
    if (!entries) {
      return null;
    }

    // Find first matching thumbnail pattern
    for (const pattern of THUMBNAIL_PATTERNS) {
      if (entries.some(e => e.name === pattern)) {
        return dirPath ? `${dirPath}/${pattern}` : pattern;
      }
    }

    // No root thumbnail - check if this is a playlist (has subdirectories, no video files)
    const hasVideoFile = entries.some(e =>
      e.name.startsWith('video.') ||
      e.name.endsWith('.mp4') ||
      e.name.endsWith('.webm') ||
      e.name.endsWith('.mkv')
    );

    if (!hasVideoFile && entries.length > 0) {
      // It's a playlist - try first subdirectory's thumbnail
      const sorted = [...entries].sort((a, b) => a.name.localeCompare(b.name));
      for (const entry of sorted.slice(0, 3)) {
        // Skip metadata files
        if (entry.name.endsWith('.json') || entry.name.endsWith('.txt')) continue;

        try {
          const subEntries = await withTimeout(tree.listDirectory(entry.cid), 5000);
          if (subEntries) {
            for (const pattern of THUMBNAIL_PATTERNS) {
              if (subEntries.some(e => e.name === pattern)) {
                const subPath = dirPath ? `${dirPath}/${entry.name}` : entry.name;
                return `${subPath}/${pattern}`;
              }
            }
          }
        } catch {
          // Skip this subdirectory
        }
      }
    }

    return null;
  } catch (err) {
    console.error('[SwFileHandler] findThumbnailInDir error:', err);
    return null;
  }
}

interface FileRequest {
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

interface FileResponseHeaders {
  status: number;
  headers: Record<string, string>;
  body: 'STREAM' | string | null;
  totalSize?: number;
}

let isSetup = false;

/**
 * Setup the file request handler
 * Call this once at app startup
 */
export function setupSwFileHandler(): void {
  if (isSetup) return;
  if (!('serviceWorker' in navigator)) {
    console.warn('[SwFileHandler] Service workers not supported');
    return;
  }

  navigator.serviceWorker.addEventListener('message', handleSwMessage);
  isSetup = true;
  console.log('[SwFileHandler] Handler registered');
}

/**
 * Handle incoming messages from service worker
 */
function handleSwMessage(event: MessageEvent): void {
  const data = event.data as FileRequest;

  // Only handle hashtree-file requests
  if (data?.type !== 'hashtree-file') return;

  const [port] = event.ports;
  if (!port) {
    console.error('[SwFileHandler] No port in message');
    return;
  }

  handleFileRequest(data, port).catch((error) => {
    console.error('[SwFileHandler] Error handling request:', error);
    port.postMessage({
      status: 500,
      headers: { 'Content-Type': 'text/plain' },
      body: `Error: ${error.message}`,
    } as FileResponseHeaders);
  });
}

/**
 * Handle a file request from the service worker
 */
async function handleFileRequest(request: FileRequest, port: MessagePort): Promise<void> {
  try {
    const { npub, nhash, treeName, path } = request;
    console.log('[SwFileHandler] Request:', { npub: npub?.slice(0, 16), nhash, treeName, path });

    // Resolve the CID
    let cid: CID | null = null;

    if (nhash) {
      // Direct nhash request - decode to CID
      const rootCid = nhashDecode(nhash);
      const tree = getTree();

      // If path provided AND it contains a slash, navigate within the nhash directory
      // Single filename without slashes is just a hint for MIME type - use rootCid directly
      if (path && path.includes('/')) {
        const entry = await withTimeout(tree.resolvePath(rootCid, path), 10000);
        if (!entry) {
          port.postMessage({
            status: 404,
            headers: { 'Content-Type': 'text/plain' },
            body: `File not found: ${path}`,
          } as FileResponseHeaders);
          return;
        }
        cid = entry.cid;
      } else {
        // Path is either empty or just a filename hint - use rootCid directly
        // Check if rootCid is a directory and path is a single filename
        if (path && !path.includes('/')) {
          // Try to resolve as file within directory first
          const entry = await withTimeout(tree.resolvePath(rootCid, path), 10000);
          if (entry) {
            cid = entry.cid;
          } else {
            // Not a directory with this file - use rootCid directly as file CID
            cid = rootCid;
          }
        } else {
          cid = rootCid;
        }
      }
    } else if (npub && treeName) {
      // Npub-based request - resolve through tree root cache
      const filePath = path;

      // Try local write cache first (we're the broadcaster), then Nostr subscription cache
      let rootCid: CID | null = null;

      const localHash = getLocalRootCache(npub, treeName);
      if (localHash) {
        // This tab has the local write cache - we're the broadcaster
        const localKey = getLocalRootKey(npub, treeName);
        rootCid = { hash: localHash, key: localKey };
        console.log('[SwFileHandler] Found local cache for', npub.slice(0, 16), treeName);
      } else {
        // Try sync cache (for already-resolved trees from Nostr)
        rootCid = getTreeRootSync(npub, treeName);
        console.log('[SwFileHandler] getTreeRootSync result:', rootCid ? 'found' : 'null');

        if (!rootCid) {
          // If not in cache, wait for resolver to fetch from network
          // Use shorter timeout for thumbnails (10s) vs other files (30s)
          const isThumbnail = filePath === 'thumbnail' || filePath.endsWith('/thumbnail');
          const timeout = isThumbnail ? 10000 : 30000;
          console.log('[SwFileHandler] Waiting for tree root...');
          rootCid = await waitForTreeRoot(npub, treeName, timeout);
          if (!rootCid) {
            console.warn('[SwFileHandler] Tree root timeout:', npub.slice(0, 16), treeName);
          } else {
            console.log('[SwFileHandler] Got tree root from waitForTreeRoot');
          }
        }
      }

      if (!rootCid) {
        port.postMessage({
          status: 404,
          headers: { 'Content-Type': 'text/plain' },
          body: 'Tree not found',
        } as FileResponseHeaders);
        return;
      }

      // Navigate to file
      const tree = getTree();
      let resolvedPath = filePath || '';

      // Handle thumbnail requests without extension - find actual file
      if (resolvedPath.endsWith('/thumbnail') || resolvedPath === 'thumbnail') {
        const dirPath = resolvedPath.endsWith('/thumbnail')
          ? resolvedPath.slice(0, -'/thumbnail'.length)
          : '';
        const actualPath = await findThumbnailInDir(rootCid, dirPath);
        if (actualPath) {
          resolvedPath = actualPath;
        }
      }

      console.log('[SwFileHandler] Resolving path:', resolvedPath, 'in tree root');
      // Use timeout for path resolution to avoid hanging on slow network
      const entry = await withTimeout(tree.resolvePath(rootCid, resolvedPath), 10000);
      console.log('[SwFileHandler] resolvePath result:', entry ? 'found' : 'null');
      if (!entry) {
        port.postMessage({
          status: 404,
          headers: { 'Content-Type': 'text/plain' },
          body: 'File not found',
        } as FileResponseHeaders);
        return;
      }

      cid = entry.cid;
    }

    if (!cid) {
      port.postMessage({
        status: 400,
        headers: { 'Content-Type': 'text/plain' },
        body: 'Invalid request',
      } as FileResponseHeaders);
      return;
    }

    const tree = getTree();

    // Get file size first (needed for Content-Length and range calculations)
    // Use getTreeNode which handles encryption, then sum link sizes
    // Add timeout to avoid hanging on slow network
    let totalSize: number;
    const treeNode = await withTimeout(tree.getTreeNode(cid), 10000);

    if (treeNode) {
      // Chunked file - sum link sizes from decrypted tree node
      // Note: for encrypted files, link.size is the decrypted (plaintext) size
      totalSize = treeNode.links.reduce((sum, l) => sum + l.size, 0);
    } else {
      // Single blob - fetch just to check existence and get encrypted size
      const blob = await withTimeout(tree.getBlob(cid.hash), 10000);
      if (!blob) {
        port.postMessage({
          status: 404,
          headers: { 'Content-Type': 'text/plain' },
          body: 'File data not found',
        } as FileResponseHeaders);
        return;
      }
      // For encrypted blobs, decrypted size = encrypted size - 16 (nonce overhead)
      // This is a small file anyway (< chunk size ~2MB)
      totalSize = cid.key ? Math.max(0, blob.length - 16) : blob.length;
    }

    // Stream the content
    await streamFromCid(request, port, cid, totalSize);
  } catch (error) {
    console.error('[SwFileHandler] Error:', error);
    port.postMessage({
      status: 500,
      headers: { 'Content-Type': 'text/plain' },
      body: `Error: ${(error as Error).message}`,
    } as FileResponseHeaders);
  }
}

/**
 * Stream content from a resolved CID
 */
async function streamFromCid(
  request: FileRequest,
  port: MessagePort,
  cid: CID,
  totalSize: number
): Promise<void> {
  const { npub, path, start, end, mimeType, download } = request;
  const tree = getTree();

  const rangeStart = start || 0;
  const rangeEnd = end !== undefined ? Math.min(end, totalSize - 1) : totalSize - 1;
  const contentLength = rangeEnd - rangeStart + 1;

  // Build response headers
  // For npub-based requests (mutable content):
  //   - Images: use stale-while-revalidate for instant display on back nav
  //   - Other: no-cache to ensure fresh content
  // For nhash-based requests (content-addressed, immutable): cache forever
  const isNpubRequest = !!npub;
  const isImage = mimeType.startsWith('image/');
  let cacheControl: string;
  if (!isNpubRequest) {
    cacheControl = 'public, max-age=31536000, immutable'; // Immutable: cache forever
  } else if (isImage) {
    // Images (thumbnails): serve stale instantly, revalidate in background
    cacheControl = 'public, max-age=60, stale-while-revalidate=86400';
  } else {
    cacheControl = 'no-cache, no-store, must-revalidate'; // Other mutable: always revalidate
  }
  const headers: Record<string, string> = {
    'Content-Type': mimeType,
    'Accept-Ranges': 'bytes',
    'Cache-Control': cacheControl,
  };

  // Add Content-Disposition header for downloads
  if (download) {
    const filename = path || 'file';
    headers['Content-Disposition'] = `attachment; filename="${filename}"`;
  }

  // Safari requires 206 Partial Content for ANY range request, not just start > 0
  // If end is specified (even bytes=0-1), it's a range request
  let status = 200;
  const isRangeRequest = end !== undefined || (start !== undefined && start > 0);
  if (isRangeRequest) {
    status = 206;
    headers['Content-Range'] = `bytes ${rangeStart}-${rangeEnd}/${totalSize}`;
  }
  headers['Content-Length'] = String(contentLength);

  // Send headers first
  port.postMessage({
    status,
    headers,
    body: 'STREAM',
    totalSize,
  } as FileResponseHeaders);

  // Stream chunks via pull-based protocol using readFileRange
  // This fetches only the needed chunks from network, not the whole file
  let offset = rangeStart;
  const CHUNK_SIZE = 256 * 1024; // 256KB chunks - matches videoChunker's firstChunkSize

  port.onmessage = async (msg: MessageEvent) => {
    if (msg.data === false) {
      // Cancel signal
      port.onmessage = null;
      return;
    }

    if (msg.data === true) {
      // Pull request - fetch and send next chunk
      if (offset > rangeEnd) {
        // Done
        port.postMessage(null);
        port.onmessage = null;
        return;
      }

      try {
        const chunkEnd = Math.min(offset + CHUNK_SIZE - 1, rangeEnd);
        const chunk = await tree.readFileRange(cid, offset, chunkEnd + 1);
        if (chunk) {
          port.postMessage(chunk);
          offset = chunkEnd + 1;
        } else {
          port.postMessage(null);
          port.onmessage = null;
        }
      } catch (err) {
        console.error('[SwFileHandler] Range read error:', err);
        port.postMessage(null);
        port.onmessage = null;
      }
    }
  };
}
