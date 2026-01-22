/**
 * Service Worker with File Streaming Support
 *
 * Intercepts file requests and streams data from main thread:
 * - /htree/{npub}/{treeName}/{path} - Npub-based file access
 * - /htree/{nhash}/{filename} - Direct nhash access (content-addressed)
 *
 * Uses WebTorrent-style per-request MessageChannel pattern:
 * - SW creates MessageChannel for each request
 * - Posts request to all clients (windows)
 * - First client to respond wins
 * - Client streams chunks back through the port
 *
 * Routes are namespaced under /htree/ for reusability across apps.
 */

/// <reference lib="webworker" />
import { precacheAndRoute } from 'workbox-precaching';

declare let self: ServiceWorkerGlobalScope;

const isTestMode = !!import.meta.env.VITE_TEST_MODE;

// Precache static assets (injected by VitePWA)
if (!isTestMode) {
  precacheAndRoute(self.__WB_MANIFEST);
}

if (isTestMode) {
  self.addEventListener('install', (event) => {
    event.waitUntil(self.skipWaiting());
  });

  self.addEventListener('activate', (event) => {
    event.waitUntil((async () => {
      const keys = await caches.keys();
      await Promise.all(keys.map(key => caches.delete(key)));
      await self.clients.claim();
    })());
  });
}

// Request counter for unique IDs
let requestId = 0;

// Worker port for direct communication (set by main thread via REGISTER_WORKER_PORT)
let workerPort: MessagePort | null = null;

// Pending requests waiting for worker responses
const pendingRequests = new Map<string, {
  resolve: (response: Response) => void;
  reject: (error: Error) => void;
  controller?: ReadableStreamDefaultController<Uint8Array>;
  stream?: ReadableStream<Uint8Array>;
  totalSize?: number;
  headers?: Record<string, string>;
  status?: number;
}>();

/**
 * Handle messages from main thread (port registration)
 */
self.addEventListener('message', (event: ExtendableMessageEvent) => {
  if (event.data?.type === 'REGISTER_WORKER_PORT' && event.data.port) {
    workerPort = event.data.port;
    workerPort.onmessage = handleWorkerMessage;
    console.log('[SW] Worker port registered');
  }
});

/**
 * Handle messages from worker via MessagePort
 */
function handleWorkerMessage(event: MessageEvent): void {
  const msg = event.data;
  if (!msg?.requestId) return;

  const pending = pendingRequests.get(msg.requestId);
  if (!pending) return;

  switch (msg.type) {
    case 'headers': {
      // Got headers - create streaming response
      pending.totalSize = msg.totalSize;
      pending.status = msg.status || 200;
      pending.headers = msg.headers || {};

      // Create the stream for this response
      const { readable, writable } = new TransformStream<Uint8Array>();
      const writer = writable.getWriter();

      // Store writer for chunk handling
      (pending as { writer?: WritableStreamDefaultWriter<Uint8Array> }).writer = writer;

      // Build response headers
      const headers = new Headers(pending.headers);
      headers.set('Cross-Origin-Resource-Policy', 'cross-origin');
      headers.set('Cross-Origin-Embedder-Policy', 'credentialless');
      headers.set('Access-Control-Allow-Origin', '*');

      pending.resolve(new Response(readable, {
        status: pending.status,
        headers,
      }));
      break;
    }

    case 'chunk': {
      const writer = (pending as { writer?: WritableStreamDefaultWriter<Uint8Array> }).writer;
      if (writer && msg.data) {
        writer.write(new Uint8Array(msg.data)).catch(() => {
          // Stream closed, ignore
        });
      }
      break;
    }

    case 'done': {
      const writer = (pending as { writer?: WritableStreamDefaultWriter<Uint8Array> }).writer;
      if (writer) {
        writer.close().catch(() => {});
      }
      pendingRequests.delete(msg.requestId);
      break;
    }

    case 'error': {
      pending.reject(new Error(msg.message || 'Worker error'));
      pendingRequests.delete(msg.requestId);
      break;
    }
  }
}

/**
 * Serve file via direct worker port (preferred path)
 */
function serveFileViaWorker(request: FileRequest): Promise<Response> {
  return new Promise((resolve, reject) => {
    if (!workerPort) {
      reject(new Error('Worker port not available'));
      return;
    }

    // Register pending request
    pendingRequests.set(request.requestId, { resolve, reject });

    // Set timeout
    const timeout = setTimeout(() => {
      const pending = pendingRequests.get(request.requestId);
      if (pending) {
        pendingRequests.delete(request.requestId);
        reject(new Error('Timeout waiting for worker response'));
      }
    }, PORT_TIMEOUT);

    // Clean up timeout on resolution
    const originalResolve = resolve;
    const originalReject = reject;
    const wrappedResolve = (response: Response) => {
      clearTimeout(timeout);
      originalResolve(response);
    };
    const wrappedReject = (error: Error) => {
      clearTimeout(timeout);
      originalReject(error);
    };
    pendingRequests.set(request.requestId, { resolve: wrappedResolve, reject: wrappedReject });

    // Send request to worker
    workerPort.postMessage(request);
  });
}

// npub pattern: npub1 followed by 58 bech32 characters
const NPUB_PATTERN = /^npub1[a-z0-9]{58}$/;

// Timeout for port responses
// Must be long enough for: tree resolution + WebRTC peer attempts + Blossom fallback
// 60s is more realistic for slow networks and peer discovery
const PORT_TIMEOUT = 60000;

/**
 * Guess MIME type from file path/extension
 */
function guessMimeType(path: string): string {
  const ext = path.split('.').pop()?.toLowerCase() || '';
  const mimeTypes: Record<string, string> = {
    // Video
    'mp4': 'video/mp4',
    'm4v': 'video/mp4',
    'webm': 'video/webm',
    'ogg': 'video/ogg',
    'ogv': 'video/ogg',
    'mov': 'video/quicktime',
    'avi': 'video/x-msvideo',
    'mkv': 'video/x-matroska',
    // Audio
    'mp3': 'audio/mpeg',
    'wav': 'audio/wav',
    'flac': 'audio/flac',
    'm4a': 'audio/mp4',
    'aac': 'audio/aac',
    'oga': 'audio/ogg',
    // Images
    'jpg': 'image/jpeg',
    'jpeg': 'image/jpeg',
    'png': 'image/png',
    'gif': 'image/gif',
    'webp': 'image/webp',
    'svg': 'image/svg+xml',
    'ico': 'image/x-icon',
    // Documents
    'pdf': 'application/pdf',
    'txt': 'text/plain',
    'md': 'text/markdown',
    'html': 'text/html',
    'htm': 'text/html',
    'css': 'text/css',
    'js': 'application/javascript',
    'json': 'application/json',
    'xml': 'application/xml',
    // Archives
    'zip': 'application/zip',
    'tar': 'application/x-tar',
    'gz': 'application/gzip',
    // Code
    'ts': 'text/typescript',
    'tsx': 'text/typescript',
    'jsx': 'text/javascript',
    'py': 'text/x-python',
    'rs': 'text/x-rust',
    'go': 'text/x-go',
  };
  return mimeTypes[ext] || 'application/octet-stream';
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

/**
 * Serve file - tries worker port first, falls back to client broadcast
 */
async function serveFile(request: FileRequest): Promise<Response> {
  // Try direct worker path first (faster, no main thread involvement)
  if (workerPort) {
    try {
      return await serveFileViaWorker(request);
    } catch (error) {
      console.warn('[SW] Worker path failed, falling back to clients:', error);
    }
  }

  // Fall back to client broadcast (legacy path)
  return serveFileViaClients(request);
}

/**
 * Request file from main thread via per-request MessageChannel
 * Based on WebTorrent's worker-server.js pattern (legacy fallback)
 */
async function serveFileViaClients(request: FileRequest): Promise<Response> {
  const clientList = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });

  if (clientList.length === 0) {
    return new Response('No clients available', { status: 503 });
  }

  // Create MessageChannel and broadcast to all clients - first to respond wins
  const [data, port] = await new Promise<[FileResponseHeaders, MessagePort]>((resolve, reject) => {
    let resolved = false;
    const timeout = setTimeout(() => {
      if (!resolved) {
        resolved = true;
        reject(new Error('Timeout waiting for client response'));
      }
    }, PORT_TIMEOUT);

    for (const client of clientList) {
      const messageChannel = new MessageChannel();
      const { port1, port2 } = messageChannel;

      port1.onmessage = ({ data }) => {
        if (!resolved) {
          resolved = true;
          clearTimeout(timeout);
          resolve([data, port1]);
        }
      };

      client.postMessage(request, [port2]);
    }
  });

  const cleanup = () => {
    port.postMessage(false); // Signal cancel
    port.onmessage = null;
  };

  // Non-streaming response
  if (data.body !== 'STREAM') {
    cleanup();
    // Add cross-origin headers for embedding in iframes (required when main page has COEP)
    const headers = new Headers(data.headers);
    headers.set('Cross-Origin-Resource-Policy', 'cross-origin');
    headers.set('Cross-Origin-Embedder-Policy', 'credentialless');
    headers.set('Access-Control-Allow-Origin', '*');
    return new Response(data.body, {
      status: data.status,
      headers,
    });
  }

  // Streaming response
  let timeoutHandle: ReturnType<typeof setTimeout> | null = null;

  let streamClosed = false;

  const stream = new ReadableStream({
    pull(controller) {
      return new Promise<void>((resolve) => {
        if (streamClosed) {
          resolve();
          return;
        }

        port.onmessage = ({ data: chunk }) => {
          if (timeoutHandle) {
            clearTimeout(timeoutHandle);
            timeoutHandle = null;
          }
          if (chunk) {
            controller.enqueue(new Uint8Array(chunk));
          } else {
            streamClosed = true;
            cleanup();
            controller.close();
          }
          resolve();
        };

        // Clear any previous timeout
        if (timeoutHandle) {
          clearTimeout(timeoutHandle);
        }

        // Timeout for inactive streams (Firefox doesn't support cancel)
        // When timeout fires, close the stream properly so video element knows to stop
        timeoutHandle = setTimeout(() => {
          if (!streamClosed) {
            streamClosed = true;
            cleanup();
            controller.close();
          }
          resolve();
        }, PORT_TIMEOUT);

        // Request next chunk
        port.postMessage(true);
      });
    },
    cancel() {
      streamClosed = true;
      if (timeoutHandle) clearTimeout(timeoutHandle);
      cleanup();
    },
  });

  // Add cross-origin headers for embedding in iframes (required when main page has COEP)
  const headers = new Headers(data.headers);
  // Allow sandboxed iframe origins ("null") to load scripts/styles from /htree.
  headers.set('Cross-Origin-Resource-Policy', 'cross-origin');
  headers.set('Cross-Origin-Embedder-Policy', 'credentialless');
  headers.set('Access-Control-Allow-Origin', '*');
  return new Response(stream, {
    status: data.status,
    headers,
  });
}

/**
 * Create file request for npub-based paths
 */
function createNpubFileResponse(
  npub: string,
  treeName: string,
  filePath: string,
  rangeHeader: string | null
): Promise<Response> {
  const id = `file_${++requestId}`;
  const mimeType = guessMimeType(filePath || treeName);

  let start = 0;
  let end: number | undefined;

  if (rangeHeader) {
    const match = rangeHeader.match(/bytes=(\d*)-(\d*)/);
    if (match) {
      start = match[1] ? parseInt(match[1], 10) : 0;
      end = match[2] ? parseInt(match[2], 10) : undefined;
    }
  }

  const request: FileRequest = {
    type: 'hashtree-file',
    requestId: id,
    npub,
    treeName,
    path: filePath,
    start,
    end,
    mimeType,
  };

  return serveFile(request).catch((error) => {
    console.error('[SW] File request failed:', error);
    return new Response(`File request failed: ${error.message}`, { status: 500 });
  });
}

/**
 * Create file request for nhash-based paths (content-addressed)
 */
function createNhashFileResponse(
  nhash: string,
  filename: string,
  rangeHeader: string | null,
  forceDownload: boolean
): Promise<Response> {
  const id = `file_${++requestId}`;
  const mimeType = guessMimeType(filename);

  let start = 0;
  let end: number | undefined;

  if (rangeHeader) {
    const match = rangeHeader.match(/bytes=(\d*)-(\d*)/);
    if (match) {
      start = match[1] ? parseInt(match[1], 10) : 0;
      end = match[2] ? parseInt(match[2], 10) : undefined;
    }
  }

  const request: FileRequest = {
    type: 'hashtree-file',
    requestId: id,
    nhash,
    path: filename,
    start,
    end,
    mimeType,
    download: forceDownload,
  };

  return serveFile(request).catch((error) => {
    console.error('[SW] File request failed:', error);
    return new Response(`File request failed: ${error.message}`, { status: 500 });
  });
}

/**
 * Add COOP/COEP headers to enable SharedArrayBuffer for FFmpeg WASM
 * Uses 'credentialless' COEP mode to allow cross-origin images without CORP headers
 */
function addCrossOriginHeaders(response: Response): Response {
  // Don't modify opaque responses or redirects
  if (response.type === 'opaque' || response.type === 'opaqueredirect') {
    return response;
  }

  const newHeaders = new Headers(response.headers);
  newHeaders.set('Cross-Origin-Embedder-Policy', 'credentialless');
  newHeaders.set('Cross-Origin-Opener-Policy', 'same-origin');

  return new Response(response.body, {
    status: response.status,
    statusText: response.statusText,
    headers: newHeaders,
  });
}

/**
 * Add CORP header for same-origin resources
 * Required for scripts (including worker scripts) when COEP: credentialless is active
 */
function addCORPHeader(response: Response): Response {
  if (response.type === 'opaque' || response.type === 'opaqueredirect') {
    return response;
  }

  const newHeaders = new Headers(response.headers);
  newHeaders.set('Cross-Origin-Resource-Policy', 'same-origin');

  return new Response(response.body, {
    status: response.status,
    statusText: response.statusText,
    headers: newHeaders,
  });
}

/**
 * Add CORS headers for /htree/ responses
 * Required for sandboxed iframes (opaque origin) to access resources
 */
function addCORSHeaders(response: Response): Response {
  const newHeaders = new Headers(response.headers);
  newHeaders.set('Access-Control-Allow-Origin', '*');
  newHeaders.set('Cross-Origin-Resource-Policy', 'cross-origin');

  return new Response(response.body, {
    status: response.status,
    statusText: response.statusText,
    headers: newHeaders,
  });
}

/**
 * Intercept fetch requests
 */
self.addEventListener('fetch', (event: FetchEvent) => {
  const url = new URL(event.request.url);
  // Use href to preserve encoded characters (pathname auto-decodes %2F)
  // Extract path from URL without decoding: /htree/npub/videos%2FName/file.mp4
  const pathMatch = url.href.match(/^[^:]+:\/\/[^/]+(.*)$/);
  const rawPath = pathMatch ? pathMatch[1].split('?')[0] : url.pathname;
  const pathParts = rawPath.slice(1).split('/'); // Remove leading /
  const rangeHeader = event.request.headers.get('Range');

  // Skip non-GET requests
  if (event.request.method !== 'GET') return;

  // All hashtree routes start with /htree/ - check this FIRST before navigation handling
  // Otherwise navigation requests to /htree/... get redirected to index.html
  if (pathParts[0] === 'htree') {
    // /htree/{nhash}/{filename} - Direct nhash access (content-addressed)
    if (pathParts.length >= 2 && pathParts[1].startsWith('nhash1')) {
      const nhash = pathParts[1];
      const filename = pathParts.slice(2).join('/') || 'file';
      const forceDownload = url.searchParams.get('download') === '1';
      event.respondWith(createNhashFileResponse(nhash, filename, rangeHeader, forceDownload).then(addCORSHeaders));
      return;
    }

    // /htree/{npub}/{treeName}/{path...} - Npub-based file access
    // treeName is URL-encoded (may contain %2F for slashes)
    if (pathParts.length >= 3 && NPUB_PATTERN.test(pathParts[1])) {
      const npub = pathParts[1];
      const treeName = decodeURIComponent(pathParts[2]);
      const filePath = pathParts.slice(3).map(decodeURIComponent).join('/');
      event.respondWith(createNpubFileResponse(npub, treeName, filePath, rangeHeader).then(addCORSHeaders));
      return;
    }
  }

  // For same-origin requests, add cross-origin isolation headers
  // This enables SharedArrayBuffer for FFmpeg WASM transcoding
  if (url.origin === self.location.origin) {
    // Navigation requests need COOP/COEP headers
    if (event.request.mode === 'navigate') {
      event.respondWith(
        fetch(event.request).then(addCrossOriginHeaders)
      );
      return;
    }

    // In cross-origin isolated context, ALL same-origin resources need CORP headers
    // This includes worker scripts and all their module imports
    event.respondWith(
      fetch(event.request).then(addCORPHeader)
    );
    return;
  }

  // Let workbox handle everything else (static assets, app routes)
});

// Handle service worker installation
self.addEventListener('install', () => {
  console.log('[SW] Installing...');
  self.skipWaiting();
});

// Handle service worker activation
self.addEventListener('activate', (event: ExtendableEvent) => {
  console.log('[SW] Activating...');
  event.waitUntil(self.clients.claim());
});
