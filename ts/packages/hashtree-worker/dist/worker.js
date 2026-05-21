/// <reference lib="webworker" />
import { HashTree, decryptChk, fromHex, nhashDecode, nhashEncode, toHex, tryDecodeTreeNode, } from '@hashtree/core';
import { IdbBlobStorage } from './capabilities/idbStorage.js';
import { BlossomTransport, DEFAULT_BLOSSOM_SERVERS } from './capabilities/blossomTransport.js';
import { probeConnectivity } from './capabilities/connectivity.js';
import { MeshRouterStore } from './capabilities/meshRouterStore.js';
import { resolveRootPathFromRelays, watchRootPathFromRelays } from './capabilities/rootResolver.js';
import { clearMemoryCache, initTreeRootCache } from './relay/treeRootCache.js';
import { assertEncryptedUploadCid, markEncryptedHashes, shouldServeHashToPeer } from './privacyGuards.js';
import { streamFileRangeChunks } from './mediaStreaming.js';
import { cloneTransferableBytes } from './transferableBytes.js';
const DEFAULT_STORE_NAME = 'hashtree-worker';
const DEFAULT_STORAGE_MAX_BYTES = 1024 * 1024 * 1024;
const DEFAULT_CONNECTIVITY_PROBE_INTERVAL_MS = 20_000;
const P2P_FETCH_SLOW_LOG_MS = 15_000;
const RAW_BLOCK_UPLOAD_CONCURRENCY = 6;
const P2P_PEER_LIST_CACHE_MS = 1_500;
// Let IndexedDB start first, but only as a soft hedge window. MeshRouterStore
// keeps the local read alive after this delay instead of treating it as a miss.
const PRIMARY_READ_TIMEOUT_MS = 300;
const REMOTE_HEDGE_INTERVAL_MS = 250;
let endpoint = null;
let endpointListener = null;
let storage = null;
let blossom = null;
let meshStore = null;
let tree = null;
let nostrRelays = [];
let probeInterval = null;
let probeIntervalMs = DEFAULT_CONNECTIVITY_PROBE_INTERVAL_MS;
let p2pFetchCounter = 0;
let p2pPeerListCounter = 0;
let rootWatchCounter = 0;
let diagnosticsEnabled = false;
let diagnosticsMirrorToConsole = false;
const pendingP2PFetches = new Map();
const pendingP2PPeerLists = new Map();
let inflightP2PPeerList = null;
let p2pPeerIds = [];
let p2pPeerIdsRefreshedAt = 0;
const peerShareableEncryptedHashes = new Set();
const peerShareablePublishedHashes = new Set();
const activeRootWatches = new Map();
let putBlobStreamCounter = 0;
const activePutBlobStreams = new Map();
let typedArraySetDebugInstalled = false;
function parseHttpByteRange(rangeHeader, totalSize) {
    if (!rangeHeader)
        return { kind: 'unsupported' };
    const bytesRange = rangeHeader.startsWith('bytes=')
        ? rangeHeader.slice('bytes='.length)
        : null;
    if (!bytesRange || bytesRange.includes(','))
        return { kind: 'unsupported' };
    if (totalSize <= 0)
        return { kind: 'unsatisfiable' };
    const parts = bytesRange.split('-', 2);
    if (parts.length !== 2)
        return { kind: 'unsupported' };
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
const MEDIA_CHUNK_SIZE = 256 * 1024;
// Keep startup-at-zero aligned with videoChunker's first chunk so the browser
// does not immediately refetch the same encrypted media block.
const STARTUP_OPEN_ENDED_RANGE_WINDOW_BYTES = 256 * 1024;
// After a seek, browsers typically ask for an open-ended range from a nonzero
// offset and expect a substantially larger contiguous window than startup.
const SEEK_OPEN_ENDED_RANGE_WINDOW_BYTES = 8 * 1024 * 1024;
const MEDIA_STREAM_PREFETCH = 4;
const MESH_READ_TIMEOUT_MS = 20_000;
const MEDIA_PATH_RESOLUTION_RETRY_DELAYS_MS = [100, 300, 900];
const STARTUP_MEDIA_RANGE_RETRY_DELAYS_MS = [250, 1_000, 2_500, 5_000];
const PEER_SHARED_READ_SOURCE_IDS = ['blossom'];
function getErrorMessage(err) {
    return err instanceof Error ? err.message : String(err);
}
function installTypedArraySetDebugHook() {
    if (typedArraySetDebugInstalled) {
        return;
    }
    const originalSet = Uint8Array.prototype.set;
    Uint8Array.prototype.set = function patchedTypedArraySet(source, offset) {
        try {
            return originalSet.call(this, source, offset);
        }
        catch (error) {
            emitDiagnostic('error', 'typed-array', 'set-failed', getErrorMessage(error), {
                targetLength: typeof this?.length === 'number' ? this.length : null,
                sourceLength: typeof source?.length === 'number' ? source.length : null,
                offset: typeof offset === 'number' ? offset : 0,
                stack: new Error().stack ?? null,
            });
            throw error;
        }
    };
    typedArraySetDebugInstalled = true;
}
function isOpenEndedHttpByteRange(rangeHeader) {
    if (!rangeHeader) {
        return false;
    }
    return /^bytes=\d+-$/i.test(rangeHeader.trim());
}
function isTransientMissingChunkError(error) {
    return getErrorMessage(error).toLowerCase().startsWith('missing chunk:');
}
function shouldSendMediaHeadersBeforeFirstChunk(mimeType) {
    const normalized = `${mimeType ?? ''}`.trim().toLowerCase();
    return normalized.startsWith('audio/') || normalized.startsWith('video/');
}
async function waitForMediaRetry(ms) {
    await new Promise((resolve) => {
        setTimeout(resolve, Math.max(0, ms));
    });
}
async function readNextNonEmptyMediaChunk(iterator) {
    while (true) {
        const next = await iterator.next();
        if (next.done) {
            return null;
        }
        if (next.value.byteLength > 0) {
            return next.value;
        }
    }
}
async function readStartupMediaRangeWithRetries(tree, cid, start, endExclusive) {
    let lastError = null;
    for (let attempt = 0; attempt <= STARTUP_MEDIA_RANGE_RETRY_DELAYS_MS.length; attempt += 1) {
        try {
            const range = await tree.readFileRange(cid, start, endExclusive);
            if (!range) {
                throw new Error('File not found');
            }
            return range;
        }
        catch (error) {
            lastError = error;
            if (!isTransientMissingChunkError(error) || attempt >= STARTUP_MEDIA_RANGE_RETRY_DELAYS_MS.length) {
                throw error;
            }
            await waitForMediaRetry(STARTUP_MEDIA_RANGE_RETRY_DELAYS_MS[attempt] ?? 0);
        }
    }
    throw lastError instanceof Error ? lastError : new Error(getErrorMessage(lastError));
}
function getOpenEndedRangeWindowBytes(start) {
    return start <= 0
        ? STARTUP_OPEN_ENDED_RANGE_WINDOW_BYTES
        : SEEK_OPEN_ENDED_RANGE_WINDOW_BYTES;
}
async function resolveMediaPathWithRetries(tree, rootCid, requestedPath, requestId) {
    let resolved = await tree.resolvePath(rootCid, requestedPath);
    if (resolved) {
        return resolved;
    }
    for (let attempt = 0; attempt < MEDIA_PATH_RESOLUTION_RETRY_DELAYS_MS.length; attempt += 1) {
        const delayMs = MEDIA_PATH_RESOLUTION_RETRY_DELAYS_MS[attempt] ?? 0;
        emitDiagnostic('debug', 'media', 'path-resolve-retry', 'Retrying media path resolution after a transient directory miss', {
            requestId,
            path: requestedPath,
            attempt: attempt + 1,
            delayMs,
        });
        await waitForMediaRetry(delayMs);
        resolved = await tree.resolvePath(rootCid, requestedPath);
        if (resolved) {
            return resolved;
        }
    }
    return null;
}
async function detectMediaDirectoryWithRetries(tree, rootCid, requestedPath, requestId) {
    let rootIsDirectory = await tree.isDirectory(rootCid);
    if (rootIsDirectory) {
        return true;
    }
    for (let attempt = 0; attempt < MEDIA_PATH_RESOLUTION_RETRY_DELAYS_MS.length; attempt += 1) {
        const delayMs = MEDIA_PATH_RESOLUTION_RETRY_DELAYS_MS[attempt] ?? 0;
        emitDiagnostic('debug', 'media', 'path-root-kind-retry', 'Retrying media root kind detection after a transient miss', {
            requestId,
            path: requestedPath,
            attempt: attempt + 1,
            delayMs,
        });
        await waitForMediaRetry(delayMs);
        rootIsDirectory = await tree.isDirectory(rootCid);
        if (rootIsDirectory) {
            return true;
        }
    }
    return false;
}
async function* readFileStreamWithRetries(tree, cid, offset, requestId, prefetch = MEDIA_STREAM_PREFETCH) {
    let nextOffset = Math.max(0, offset);
    let attempt = 0;
    while (true) {
        try {
            const stream = tree.readFileStream(cid, { offset: nextOffset, prefetch });
            const iterator = stream[Symbol.asyncIterator]();
            while (true) {
                const chunk = await readNextNonEmptyMediaChunk(iterator);
                if (!chunk) {
                    return;
                }
                nextOffset += chunk.byteLength;
                attempt = 0;
                yield chunk;
            }
        }
        catch (error) {
            if (!isTransientMissingChunkError(error) || attempt >= STARTUP_MEDIA_RANGE_RETRY_DELAYS_MS.length) {
                throw error;
            }
            const delayMs = STARTUP_MEDIA_RANGE_RETRY_DELAYS_MS[attempt] ?? 0;
            emitDiagnostic('debug', 'media', 'stream-retry', 'Retrying media stream after a transient missing chunk', {
                requestId,
                offset: nextOffset,
                attempt: attempt + 1,
                delayMs,
                error: getErrorMessage(error),
            });
            attempt += 1;
            await waitForMediaRetry(delayMs);
        }
    }
}
const EMPTY_BLOSSOM_BANDWIDTH = {
    totalBytesSent: 0,
    totalBytesReceived: 0,
    updatedAt: 0,
    servers: [],
};
let blossomBandwidth = { ...EMPTY_BLOSSOM_BANDWIDTH };
function respond(message) {
    endpoint?.postMessage(message);
}
function emitDiagnostic(level, scope, code, message, data) {
    if (!diagnosticsEnabled && !diagnosticsMirrorToConsole) {
        return;
    }
    const event = {
        scope,
        code,
        level,
        message,
        timestamp: Date.now(),
        data,
    };
    if (diagnosticsEnabled) {
        respond({ type: 'diagnostic', event });
    }
    if (diagnosticsMirrorToConsole) {
        const prefix = `[HashtreeWorker:${scope}:${code}] ${message}`;
        if (level === 'error') {
            console.error(prefix, data ?? {});
            return;
        }
        if (level === 'warn') {
            console.warn(prefix, data ?? {});
            return;
        }
        console.log(prefix, data ?? {});
    }
}
function publishBlossomBandwidth(stats) {
    blossomBandwidth = {
        totalBytesSent: stats.totalBytesSent,
        totalBytesReceived: stats.totalBytesReceived,
        updatedAt: stats.updatedAt,
        servers: stats.servers.map(server => ({
            url: server.url,
            bytesSent: server.bytesSent,
            bytesReceived: server.bytesReceived,
        })),
    };
    respond({
        type: 'blossomBandwidth',
        stats: blossomBandwidth,
    });
}
function resetState() {
    if (probeInterval) {
        clearInterval(probeInterval);
        probeInterval = null;
    }
    for (const watch of activeRootWatches.values()) {
        void Promise.resolve(watch.close()).catch(() => undefined);
    }
    activeRootWatches.clear();
    storage?.close();
    storage = null;
    blossom = null;
    meshStore = null;
    tree = null;
    for (const pending of pendingP2PFetches.values()) {
        if (pending.slowLogId) {
            clearTimeout(pending.slowLogId);
        }
    }
    pendingP2PFetches.clear();
    pendingP2PPeerLists.clear();
    inflightP2PPeerList = null;
    p2pPeerIds = [];
    peerShareableEncryptedHashes.clear();
    peerShareablePublishedHashes.clear();
    activePutBlobStreams.clear();
    clearMemoryCache();
    blossomBandwidth = { ...EMPTY_BLOSSOM_BANDWIDTH };
    nostrRelays = [];
    diagnosticsEnabled = false;
    diagnosticsMirrorToConsole = false;
}
async function markEncryptedTreeHashesAsPeerShareable(id) {
    if (!tree)
        return;
    const hashes = [];
    for await (const block of tree.walkBlocks(id)) {
        hashes.push(toHex(block.hash));
    }
    markEncryptedHashes(hashes, peerShareableEncryptedHashes);
}
async function emitConnectivityUpdate() {
    if (!blossom)
        return;
    const state = await probeConnectivity(blossom.getServers());
    respond({ type: 'connectivityUpdate', state });
}
function startConnectivityProbeLoop() {
    if (probeInterval) {
        clearInterval(probeInterval);
        probeInterval = null;
    }
    probeInterval = setInterval(() => {
        void emitConnectivityUpdate();
    }, probeIntervalMs);
}
function nextP2PFetchRequestId() {
    p2pFetchCounter += 1;
    return `p2p_${Date.now()}_${p2pFetchCounter}`;
}
function nextRootWatchId() {
    rootWatchCounter += 1;
    return `root_${Date.now()}_${rootWatchCounter}`;
}
function nextP2PPeerListRequestId() {
    p2pPeerListCounter += 1;
    return `p2p_peers_${Date.now()}_${p2pPeerListCounter}`;
}
async function requestP2PBlob(hashHex, peerId) {
    const requestId = nextP2PFetchRequestId();
    const startedAt = Date.now();
    emitDiagnostic('debug', 'mesh', 'p2p-fetch-start', 'Requesting blob over P2P', {
        requestId,
        hashHex: hashHex.slice(0, 16),
        peerId: peerId ?? null,
    });
    const data = await new Promise((resolve) => {
        const slowLogId = setTimeout(() => {
            if (!pendingP2PFetches.has(requestId)) {
                return;
            }
            emitDiagnostic('warn', 'mesh', 'p2p-fetch-slow', 'P2P blob request is still in flight', {
                requestId,
                hashHex: hashHex.slice(0, 16),
                elapsedMs: Date.now() - startedAt,
            });
        }, P2P_FETCH_SLOW_LOG_MS);
        pendingP2PFetches.set(requestId, { resolve, slowLogId, startedAt });
        respond({ type: 'p2pFetch', requestId, hashHex, peerId });
    });
    return data;
}
async function requestP2PPeerIds() {
    if (inflightP2PPeerList) {
        return inflightP2PPeerList;
    }
    const requestId = nextP2PPeerListRequestId();
    let pending;
    pending = new Promise((resolve) => {
        pendingP2PPeerLists.set(requestId, { resolve });
        respond({ type: 'p2pPeerList', requestId });
    }).finally(() => {
        if (inflightP2PPeerList === pending) {
            inflightP2PPeerList = null;
        }
    });
    inflightP2PPeerList = pending;
    return pending;
}
async function refreshP2PPeerIds() {
    if (Date.now() - p2pPeerIdsRefreshedAt < P2P_PEER_LIST_CACHE_MS) {
        return;
    }
    try {
        const peerIds = await requestP2PPeerIds();
        p2pPeerIds = Array.from(new Set(peerIds.filter((peerId) => `${peerId}`.length > 0))).sort();
        p2pPeerIdsRefreshedAt = Date.now();
    }
    catch {
        p2pPeerIds = [];
        p2pPeerIdsRefreshedAt = Date.now();
    }
}
function resolveP2PFetch(requestId, data, error) {
    const pending = pendingP2PFetches.get(requestId);
    if (!pending)
        return;
    if (pending.slowLogId) {
        clearTimeout(pending.slowLogId);
    }
    pendingP2PFetches.delete(requestId);
    if (error || !data) {
        emitDiagnostic('debug', 'mesh', 'p2p-fetch-miss', 'P2P blob request completed without data', {
            requestId,
            error: error ?? null,
            elapsedMs: Math.max(0, Date.now() - pending.startedAt),
        });
        pending.resolve(null);
        return;
    }
    emitDiagnostic('debug', 'mesh', 'p2p-fetch-hit', 'Received blob data over P2P', {
        requestId,
        bytes: data.byteLength,
        elapsedMs: Math.max(0, Date.now() - pending.startedAt),
    });
    pending.resolve(data);
}
function resolveP2PPeerList(requestId, peerIds, error) {
    const pending = pendingP2PPeerLists.get(requestId);
    if (!pending)
        return;
    pendingP2PPeerLists.delete(requestId);
    if (error) {
        pending.resolve([]);
        return;
    }
    pending.resolve(Array.isArray(peerIds) ? peerIds : []);
}
function toBlobSource(sourceId) {
    return sourceId === 'idb'
        ? 'idb'
        : sourceId === 'blossom' || sourceId.startsWith('blossom:')
            ? 'blossom'
            : 'p2p';
}
async function loadBlobData(hashHex, options = {}) {
    if (!meshStore)
        return null;
    await refreshP2PPeerIds();
    const result = await meshStore.getDetailed(fromHex(hashHex), options);
    if (!result) {
        emitDiagnostic('debug', 'mesh', 'blob-load-miss', 'Blob was not available from any source', {
            hashHex: hashHex.slice(0, 16),
            skipPrimary: options.skipPrimary === true,
        });
        return null;
    }
    const source = toBlobSource(result.sourceId);
    emitDiagnostic('debug', 'mesh', 'blob-load-hit', 'Loaded blob from mesh store', {
        hashHex: hashHex.slice(0, 16),
        source,
        bytes: result.data.byteLength,
        skipPrimary: options.skipPrimary === true,
    });
    return { data: result.data, source, sourceId: result.sourceId };
}
async function loadPeerBlobData(hashHex) {
    const trustedEncryptedHash = shouldServeHashToPeer(hashHex, peerShareableEncryptedHashes);
    const trustedPublishedHash = shouldServeHashToPeer(hashHex, peerShareablePublishedHashes);
    const loaded = await loadBlobData(hashHex, trustedEncryptedHash || trustedPublishedHash ? {} : { sourceIds: PEER_SHARED_READ_SOURCE_IDS });
    if (!loaded) {
        return null;
    }
    if (trustedEncryptedHash || trustedPublishedHash || loaded.sourceId !== 'idb') {
        if (!trustedEncryptedHash) {
            markEncryptedHashes([hashHex], trustedPublishedHash ? peerShareablePublishedHashes : peerShareableEncryptedHashes);
        }
        return loaded;
    }
    const readSourceResult = await loadBlobData(hashHex, {
        skipPrimary: true,
        sourceIds: PEER_SHARED_READ_SOURCE_IDS,
    });
    if (readSourceResult) {
        markEncryptedHashes([hashHex], peerShareableEncryptedHashes);
        emitDiagnostic('debug', 'mesh', 'peer-blob-share-enabled', 'Allowing peer blob after verifying it is reachable from a read source', {
            hashHex: hashHex.slice(0, 16),
            source: readSourceResult.source,
        });
        return {
            ...loaded,
            source: readSourceResult.source,
            sourceId: readSourceResult.sourceId,
        };
    }
    emitDiagnostic('warn', 'mesh', 'peer-blob-share-denied', 'Refusing peer blob that is only available from local encrypted cache', {
        hashHex: hashHex.slice(0, 16),
    });
    return null;
}
function createStorageStore() {
    return {
        put: async (hash, data) => {
            if (!storage)
                throw new Error('Worker storage not initialized');
            await storage.putByHashTrusted(toHex(hash), data);
            return true;
        },
        get: async (hash) => {
            if (!storage) {
                return null;
            }
            return storage.get(toHex(hash));
        },
        has: async (hash) => {
            if (!storage)
                return false;
            return storage.has(toHex(hash));
        },
        delete: async (hash) => {
            if (!storage)
                return false;
            return storage.delete(toHex(hash));
        },
    };
}
function createMeshStore() {
    return new MeshRouterStore({
        primary: createStorageStore(),
        primarySourceId: 'idb',
        requestTimeoutMs: MESH_READ_TIMEOUT_MS,
        primaryReadTimeoutMs: PRIMARY_READ_TIMEOUT_MS,
        dispatch: {
            initialFanout: 1,
            hedgeFanout: 1,
            maxFanout: 2,
            hedgeIntervalMs: REMOTE_HEDGE_INTERVAL_MS,
        },
        sources: [
            {
                id: 'p2p',
                groupId: 'p2p',
                isAvailable: () => p2pPeerIds.length === 0,
                get: async (hash) => requestP2PBlob(toHex(hash)),
            },
        ],
        sourceProviders: [
            () => p2pPeerIds.map((peerId) => ({
                id: `peer:${peerId}`,
                groupId: 'p2p',
                get: async (hash) => requestP2PBlob(toHex(hash), peerId),
            })),
            () => blossom
                ? blossom.getReadServers().map((server) => ({
                    id: `blossom:${server.url}`,
                    groupId: 'blossom',
                    canWrite: !!server.write,
                    get: async (hash) => blossom ? blossom.fetchFromServer(toHex(hash), server.url) : null,
                }))
                : [],
        ],
    });
}
async function getPlaintextFileSize(fileCid) {
    if (!tree)
        return null;
    if (!fileCid.key) {
        return tree.getSize(fileCid.hash);
    }
    const loaded = await loadBlobData(toHex(fileCid.hash));
    if (!loaded)
        return null;
    const decryptedRoot = await decryptChk(loaded.data, fileCid.key);
    const rootNode = tryDecodeTreeNode(decryptedRoot);
    if (!rootNode) {
        return decryptedRoot.byteLength;
    }
    const summedSize = rootNode.links.reduce((sum, link) => sum + (link.size ?? 0), 0);
    if (summedSize > 0) {
        return summedSize;
    }
    const fullData = await tree.readFile(fileCid);
    return fullData?.byteLength ?? 0;
}
function decodeDownloadName(path) {
    try {
        return decodeURIComponent(path.split('/').pop() || 'file');
    }
    catch {
        return path.split('/').pop() || 'file';
    }
}
function postMediaError(port, requestId, message) {
    emitDiagnostic('warn', 'media', 'media-request-error', message, { requestId });
    const response = { type: 'error', requestId, message };
    port.postMessage(response);
}
async function handleMediaFileRequest(port, request) {
    if (!tree) {
        emitDiagnostic('error', 'media', 'worker-not-initialized', 'Worker not initialized for media request', {
            requestId: request.requestId,
        });
        postMediaError(port, request.requestId, 'Worker not initialized');
        return;
    }
    let rootCid;
    try {
        rootCid = nhashDecode(request.nhash);
    }
    catch {
        emitDiagnostic('warn', 'media', 'invalid-nhash', 'Invalid nhash for media request', {
            requestId: request.requestId,
        });
        postMediaError(port, request.requestId, 'Invalid nhash');
        return;
    }
    emitDiagnostic('debug', 'media', 'request-start', 'Handling media request', {
        requestId: request.requestId,
        start: request.start,
        end: typeof request.end === 'number' ? request.end : null,
        rangeHeader: request.rangeHeader ?? null,
        head: request.head === true,
    });
    let cid = rootCid;
    const requestedPath = request.path.trim().replace(/^\/+/, '');
    if (requestedPath) {
        emitDiagnostic('debug', 'media', 'path-resolve-start', 'Resolving media path', {
            requestId: request.requestId,
            path: requestedPath,
        });
        const rootIsDirectory = await detectMediaDirectoryWithRetries(tree, rootCid, requestedPath, request.requestId);
        emitDiagnostic('debug', 'media', 'path-resolve-root-kind', 'Resolved root directory status for media request', {
            requestId: request.requestId,
            path: requestedPath,
            rootIsDirectory,
        });
        if (rootIsDirectory) {
            emitDiagnostic('debug', 'media', 'path-resolve-lookup', 'Looking up media path inside a directory root', {
                requestId: request.requestId,
                path: requestedPath,
            });
            const resolved = await resolveMediaPathWithRetries(tree, rootCid, requestedPath, request.requestId);
            if (resolved) {
                cid = resolved.cid;
                emitDiagnostic('debug', 'media', 'path-resolved', 'Resolved media path to a CID', {
                    requestId: request.requestId,
                    path: requestedPath,
                    type: resolved.type,
                });
            }
            else {
                emitDiagnostic('warn', 'media', 'file-not-found', 'Media file path not found', {
                    requestId: request.requestId,
                });
                postMediaError(port, request.requestId, 'File not found');
                return;
            }
        }
        else {
            emitDiagnostic('debug', 'media', 'root-is-file', 'Using the root CID directly for a file media request', {
                requestId: request.requestId,
                path: requestedPath,
            });
        }
    }
    const sizeHint = typeof request.sizeHint === 'number' && Number.isFinite(request.sizeHint) && request.sizeHint > 0
        ? Math.floor(request.sizeHint)
        : undefined;
    const isStreamingStartupRequestWithoutKnownSize = !request.head
        && !sizeHint
        && !request.rangeHeader
        && (!Number.isFinite(request.start) || request.start <= 0)
        && typeof request.end !== 'number'
        && (!request.rangeHeader || shouldSendMediaHeadersBeforeFirstChunk(request.mimeType));
    if (isStreamingStartupRequestWithoutKnownSize) {
        const responseHeaders = {
            'content-type': request.mimeType || 'application/octet-stream',
            'accept-ranges': 'bytes',
        };
        if (request.download) {
            const fileName = decodeDownloadName(request.path).replace(/["\\]/g, '_');
            responseHeaders['content-disposition'] = `attachment; filename="${fileName}"`;
        }
        const headersMessage = {
            type: 'headers',
            requestId: request.requestId,
            status: 200,
            totalSize: 0,
            headers: responseHeaders,
        };
        const sendHeadersBeforeStartupChunk = shouldSendMediaHeadersBeforeFirstChunk(request.mimeType);
        if (sendHeadersBeforeStartupChunk) {
            port.postMessage(headersMessage);
            emitDiagnostic('debug', 'media', 'headers-sent', 'Sent media response headers', {
                requestId: request.requestId,
                totalSize: 0,
                status: 200,
                start: 0,
                end: null,
            });
        }
        const startupChunk = await readStartupMediaRangeWithRetries(tree, cid, 0, STARTUP_OPEN_ENDED_RANGE_WINDOW_BYTES);
        let startupBytesSent = 0;
        if (!sendHeadersBeforeStartupChunk) {
            port.postMessage(headersMessage);
            emitDiagnostic('debug', 'media', 'headers-sent', 'Sent media response headers', {
                requestId: request.requestId,
                totalSize: 0,
                status: 200,
                start: 0,
                end: null,
            });
        }
        let emittedChunks = 0;
        for (let offset = 0; offset < startupChunk.byteLength; offset += MEDIA_CHUNK_SIZE) {
            const chunk = startupChunk.slice(offset, offset + MEDIA_CHUNK_SIZE);
            if (chunk.byteLength === 0) {
                continue;
            }
            startupBytesSent += chunk.byteLength;
            emittedChunks += 1;
            if (offset === 0) {
                emitDiagnostic('debug', 'media', 'first-chunk', 'Emitting first unbounded media chunk', {
                    requestId: request.requestId,
                    bytes: chunk.byteLength,
                });
            }
            const transferableChunk = cloneTransferableBytes(chunk);
            const chunkMessage = {
                type: 'chunk',
                requestId: request.requestId,
                data: transferableChunk,
            };
            port.postMessage(chunkMessage, [transferableChunk.buffer]);
        }
        const stream = readFileStreamWithRetries(tree, cid, startupBytesSent, request.requestId, MEDIA_STREAM_PREFETCH);
        for await (const chunk of stream) {
            emittedChunks += 1;
            const transferableChunk = cloneTransferableBytes(chunk);
            const chunkMessage = {
                type: 'chunk',
                requestId: request.requestId,
                data: transferableChunk,
            };
            port.postMessage(chunkMessage, [transferableChunk.buffer]);
        }
        emitDiagnostic('debug', 'media', 'request-complete', 'Completed media request without a precomputed size', {
            requestId: request.requestId,
            status: 200,
        });
        const doneMessage = { type: 'done', requestId: request.requestId };
        port.postMessage(doneMessage);
        return;
    }
    const totalSizeValue = sizeHint ?? await getPlaintextFileSize(cid);
    if (totalSizeValue === null) {
        emitDiagnostic('warn', 'media', 'size-not-found', 'Media file size unavailable', {
            requestId: request.requestId,
        });
        postMediaError(port, request.requestId, 'File not found');
        return;
    }
    const totalSize = totalSizeValue;
    if (totalSize === 0) {
        const headersMessage = {
            type: 'headers',
            requestId: request.requestId,
            status: 200,
            totalSize,
            headers: {
                'content-type': request.mimeType || 'application/octet-stream',
                'accept-ranges': 'bytes',
                'content-length': '0',
            },
        };
        port.postMessage(headersMessage);
        const doneMessage = { type: 'done', requestId: request.requestId };
        port.postMessage(doneMessage);
        return;
    }
    const parsedRange = parseHttpByteRange(request.rangeHeader, totalSize);
    if (parsedRange.kind === 'unsatisfiable') {
        const headers = {
            type: 'headers',
            requestId: request.requestId,
            status: 416,
            totalSize,
            headers: {
                'content-type': request.mimeType || 'application/octet-stream',
                'content-range': `bytes */${totalSize}`,
            },
        };
        port.postMessage(headers);
        const done = { type: 'done', requestId: request.requestId };
        port.postMessage(done);
        return;
    }
    const defaultStart = Number.isFinite(request.start) ? Math.max(0, Math.floor(request.start)) : 0;
    const start = parsedRange.kind === 'range' ? parsedRange.range.start : defaultStart;
    if (start >= totalSize) {
        const headers = {
            type: 'headers',
            requestId: request.requestId,
            status: 416,
            totalSize,
            headers: {
                'content-type': request.mimeType || 'application/octet-stream',
                'content-range': `bytes */${totalSize}`,
            },
        };
        port.postMessage(headers);
        const done = { type: 'done', requestId: request.requestId };
        port.postMessage(done);
        return;
    }
    const requestedEnd = parsedRange.kind === 'range'
        ? parsedRange.range.endInclusive
        : Number.isFinite(request.end) && typeof request.end === 'number'
            ? Math.floor(request.end)
            : totalSize - 1;
    const openEndedRangeWindowBytes = getOpenEndedRangeWindowBytes(start);
    const cappedRequestedEnd = parsedRange.kind === 'range' && isOpenEndedHttpByteRange(request.rangeHeader)
        ? Math.min(requestedEnd, start + openEndedRangeWindowBytes - 1)
        : requestedEnd;
    const end = Math.min(totalSize - 1, Math.max(start, cappedRequestedEnd));
    const isPartial = parsedRange.kind === 'range' || start !== 0 || end !== totalSize - 1;
    const expectedLength = end - start + 1;
    const responseHeaders = {
        'content-type': request.mimeType || 'application/octet-stream',
        'accept-ranges': 'bytes',
        'content-length': String(expectedLength),
    };
    if (isPartial) {
        responseHeaders['content-range'] = `bytes ${start}-${end}/${totalSize}`;
    }
    if (request.download) {
        const fileName = decodeDownloadName(request.path).replace(/["\\]/g, '_');
        responseHeaders['content-disposition'] = `attachment; filename="${fileName}"`;
    }
    const headersMessage = {
        type: 'headers',
        requestId: request.requestId,
        status: isPartial ? 206 : 200,
        totalSize,
        headers: responseHeaders,
    };
    const sendHeadersBeforeFirstChunk = !request.head
        && shouldSendMediaHeadersBeforeFirstChunk(request.mimeType);
    const shouldBufferStartupRange = !request.head
        && parsedRange.kind === 'range'
        && isOpenEndedHttpByteRange(request.rangeHeader)
        && start === 0
        && expectedLength <= STARTUP_OPEN_ENDED_RANGE_WINDOW_BYTES;
    if (shouldBufferStartupRange) {
        if (sendHeadersBeforeFirstChunk) {
            port.postMessage(headersMessage);
            emitDiagnostic('debug', 'media', 'headers-sent', 'Sent media response headers', {
                requestId: request.requestId,
                totalSize,
                status: isPartial ? 206 : 200,
                start,
                end,
            });
        }
        const buffered = await readStartupMediaRangeWithRetries(tree, cid, start, end + 1);
        if (!sendHeadersBeforeFirstChunk) {
            port.postMessage(headersMessage);
            emitDiagnostic('debug', 'media', 'headers-sent', 'Sent media response headers', {
                requestId: request.requestId,
                totalSize,
                status: isPartial ? 206 : 200,
                start,
                end,
            });
        }
        for (let offset = 0; offset < buffered.byteLength; offset += MEDIA_CHUNK_SIZE) {
            const chunk = buffered.slice(offset, offset + MEDIA_CHUNK_SIZE);
            if (offset === 0) {
                emitDiagnostic('debug', 'media', 'first-chunk', 'Emitting first ranged media chunk', {
                    requestId: request.requestId,
                    bytes: chunk.byteLength,
                });
            }
            const transferableChunk = cloneTransferableBytes(chunk);
            const chunkMessage = {
                type: 'chunk',
                requestId: request.requestId,
                data: transferableChunk,
            };
            port.postMessage(chunkMessage, [transferableChunk.buffer]);
        }
        emitDiagnostic('debug', 'media', 'request-complete', 'Completed media request', {
            requestId: request.requestId,
            totalSize,
            status: isPartial ? 206 : 200,
        });
        const doneMessage = { type: 'done', requestId: request.requestId };
        port.postMessage(doneMessage);
        return;
    }
    if (!request.head) {
        const stream = streamFileRangeChunks(tree, cid, start, end, MEDIA_CHUNK_SIZE, MEDIA_STREAM_PREFETCH);
        const iterator = stream[Symbol.asyncIterator]();
        let firstChunk = null;
        if (sendHeadersBeforeFirstChunk) {
            port.postMessage(headersMessage);
            emitDiagnostic('debug', 'media', 'headers-sent', 'Sent media response headers', {
                requestId: request.requestId,
                totalSize,
                status: isPartial ? 206 : 200,
                start,
                end,
            });
            firstChunk = await readNextNonEmptyMediaChunk(iterator);
        }
        else {
            firstChunk = await readNextNonEmptyMediaChunk(iterator);
            port.postMessage(headersMessage);
            emitDiagnostic('debug', 'media', 'headers-sent', 'Sent media response headers', {
                requestId: request.requestId,
                totalSize,
                status: isPartial ? 206 : 200,
                start,
                end,
            });
        }
        let emittedChunks = 0;
        if (firstChunk) {
            emittedChunks += 1;
            emitDiagnostic('debug', 'media', 'first-chunk', 'Emitting first ranged media chunk', {
                requestId: request.requestId,
                bytes: firstChunk.byteLength,
            });
            const transferableChunk = cloneTransferableBytes(firstChunk);
            const chunkMessage = {
                type: 'chunk',
                requestId: request.requestId,
                data: transferableChunk,
            };
            port.postMessage(chunkMessage, [transferableChunk.buffer]);
        }
        while (true) {
            const chunk = await readNextNonEmptyMediaChunk(iterator);
            if (!chunk) {
                break;
            }
            emittedChunks += 1;
            const transferableChunk = cloneTransferableBytes(chunk);
            const chunkMessage = {
                type: 'chunk',
                requestId: request.requestId,
                data: transferableChunk,
            };
            port.postMessage(chunkMessage, [transferableChunk.buffer]);
        }
    }
    else {
        port.postMessage(headersMessage);
        emitDiagnostic('debug', 'media', 'headers-sent', 'Sent media response headers', {
            requestId: request.requestId,
            totalSize,
            status: isPartial ? 206 : 200,
            start,
            end,
        });
    }
    emitDiagnostic('debug', 'media', 'request-complete', 'Completed media request', {
        requestId: request.requestId,
        totalSize,
        status: isPartial ? 206 : 200,
    });
    const doneMessage = { type: 'done', requestId: request.requestId };
    port.postMessage(doneMessage);
}
function registerMediaPort(port) {
    emitDiagnostic('info', 'media', 'port-registered', 'Registered media MessagePort');
    port.onmessage = (event) => {
        const data = event.data;
        if (!data || data.type !== 'hashtree-file' || typeof data.requestId !== 'string') {
            return;
        }
        if (typeof data.nhash !== 'string' || typeof data.path !== 'string') {
            emitDiagnostic('warn', 'media', 'invalid-request', 'Received invalid media request payload', {
                requestId: data.requestId,
            });
            postMediaError(port, data.requestId, 'Invalid media request');
            return;
        }
        const request = {
            type: 'hashtree-file',
            requestId: data.requestId,
            nhash: data.nhash,
            path: data.path,
            start: typeof data.start === 'number' ? data.start : 0,
            end: typeof data.end === 'number' ? data.end : undefined,
            rangeHeader: typeof data.rangeHeader === 'string' ? data.rangeHeader : null,
            sizeHint: typeof data.sizeHint === 'number' && Number.isFinite(data.sizeHint) && data.sizeHint > 0
                ? Math.floor(data.sizeHint)
                : undefined,
            mimeType: typeof data.mimeType === 'string' ? data.mimeType : undefined,
            download: !!data.download,
            head: !!data.head,
        };
        void handleMediaFileRequest(port, request).catch((err) => {
            postMediaError(port, request.requestId, getErrorMessage(err));
        });
    };
}
function init(config) {
    resetState();
    const storeName = config.storeName || DEFAULT_STORE_NAME;
    const maxBytes = config.storageMaxBytes || DEFAULT_STORAGE_MAX_BYTES;
    probeIntervalMs = config.connectivityProbeIntervalMs || DEFAULT_CONNECTIVITY_PROBE_INTERVAL_MS;
    nostrRelays = config.relays ?? [];
    diagnosticsEnabled = config.diagnosticsEnabled === true;
    diagnosticsMirrorToConsole = config.diagnosticsMirrorToConsole === true;
    if (diagnosticsEnabled || diagnosticsMirrorToConsole) {
        installTypedArraySetDebugHook();
    }
    storage = new IdbBlobStorage(storeName, maxBytes);
    initTreeRootCache(createStorageStore());
    blossom = new BlossomTransport(config.blossomServers || DEFAULT_BLOSSOM_SERVERS, (stats) => {
        publishBlossomBandwidth(stats);
    });
    meshStore = createMeshStore();
    tree = new HashTree({ store: meshStore });
    publishBlossomBandwidth(blossom.getBandwidthStats());
    emitDiagnostic('info', 'worker', 'initialized', 'Hashtree worker initialized', {
        storeName,
        relayCount: nostrRelays.length,
        diagnosticsMirrorToConsole,
    });
    startConnectivityProbeLoop();
    void emitConnectivityUpdate();
}
function nextPutBlobStreamId() {
    putBlobStreamCounter += 1;
    return `pbs_${Date.now()}_${putBlobStreamCounter}`;
}
function normalizeHashHex(value) {
    const normalized = `${value ?? ''}`.trim().toLowerCase();
    if (!normalized) {
        return undefined;
    }
    if (!/^[0-9a-f]{64}$/.test(normalized)) {
        throw new Error('Invalid raw block hash');
    }
    return normalized;
}
async function storeRawBlock(block) {
    if (!storage) {
        throw new Error('Worker storage not initialized');
    }
    const hashHex = normalizeHashHex(block.hashHex);
    const data = block.data.slice();
    const storedHashHex = hashHex
        ? (await storage.putByHash(hashHex, data), hashHex)
        : await storage.put(data);
    return {
        hashHex: storedHashHex,
        nhash: nhashEncode({ hash: fromHex(storedHashHex) }),
        data,
        mimeType: block.mimeType,
    };
}
async function uploadRawBlocks(blocks) {
    if (!blossom || blocks.length === 0 || blossom.getWriteServers().length === 0) {
        return;
    }
    const failures = [];
    let nextIndex = 0;
    const workerCount = Math.min(RAW_BLOCK_UPLOAD_CONCURRENCY, blocks.length);
    const uploadStores = Array.from({ length: workerCount }, () => blossom.createUploadStore());
    await Promise.all(uploadStores.map(async (store) => {
        while (nextIndex < blocks.length) {
            const block = blocks[nextIndex];
            nextIndex += 1;
            if (!block) {
                continue;
            }
            try {
                await store.put(fromHex(block.hashHex), block.data, block.mimeType || 'application/octet-stream');
            }
            catch (error) {
                failures.push({
                    hashHex: block.hashHex,
                    error: error instanceof Error ? error : new Error(String(error)),
                });
            }
        }
    }));
    if (failures.length > 0) {
        const detail = failures
            .slice(0, 3)
            .map(({ hashHex, error }) => `${hashHex}: ${error.message}`)
            .join('; ');
        throw new Error(detail ? `Raw block upload failed: ${detail}` : 'Raw block upload failed');
    }
    markEncryptedHashes(blocks.map(({ hashHex }) => hashHex), peerShareablePublishedHashes);
}
async function storeAndMaybeUploadRawBlocks(blocks, upload) {
    const storedBlocks = await Promise.all(blocks.map((block) => storeRawBlock(block)));
    if (upload) {
        await uploadRawBlocks(storedBlocks);
    }
    return storedBlocks;
}
function startBlossomUploadProgress(hashHex, nhash, fileCid) {
    if (!blossom || !tree)
        return;
    const writeServers = blossom.getWriteServers();
    if (writeServers.length === 0)
        return;
    const chunkProgressEmitIntervalMs = 100;
    const progress = {
        hashHex,
        nhash,
        totalServers: writeServers.length,
        processedServers: 0,
        uploadedServers: 0,
        skippedServers: 0,
        failedServers: 0,
        totalChunks: 0,
        processedChunks: 0,
        progressRatio: 0,
        complete: false,
    };
    const serverStats = new Map();
    for (const server of writeServers) {
        serverStats.set(server.url, { url: server.url, uploaded: 0, skipped: 0, failed: 0 });
    }
    let lastChunkProgressEmit = 0;
    const syncServerStatuses = () => {
        progress.serverStatuses = Array.from(serverStats.values())
            .map((status) => ({ ...status }))
            .sort((a, b) => a.url.localeCompare(b.url));
    };
    const emitProgress = () => {
        syncServerStatuses();
        respond({ type: 'uploadProgress', progress: { ...progress } });
    };
    emitProgress();
    const onUploadProgress = (serverUrl, status) => {
        const stats = serverStats.get(serverUrl);
        if (!stats)
            return;
        stats[status]++;
    };
    void (async () => {
        const uploadStore = blossom.createUploadStore(onUploadProgress);
        const result = await tree.push(fileCid, uploadStore, {
            onProgress: (current, total) => {
                if (total <= 0 || progress.complete)
                    return;
                const fraction = current / total;
                progress.totalChunks = total;
                progress.processedChunks = current;
                progress.progressRatio = Math.max(0, Math.min(1, fraction));
                const processedEstimate = Math.min(progress.totalServers, Math.max(0, Math.floor(fraction * progress.totalServers)));
                const serverEstimateChanged = processedEstimate !== progress.processedServers;
                if (serverEstimateChanged) {
                    progress.processedServers = processedEstimate;
                }
                const now = Date.now();
                const shouldEmitChunkProgress = now - lastChunkProgressEmit >= chunkProgressEmitIntervalMs || current >= total;
                if (serverEstimateChanged || shouldEmitChunkProgress) {
                    lastChunkProgressEmit = now;
                    emitProgress();
                }
            },
        });
        let uploadedServers = 0;
        let skippedServers = 0;
        let failedServers = 0;
        for (const [, stats] of serverStats) {
            if (stats.failed > 0) {
                failedServers++;
            }
            else if (stats.uploaded > 0) {
                uploadedServers++;
            }
            else {
                skippedServers++;
            }
        }
        progress.uploadedServers = uploadedServers;
        progress.skippedServers = skippedServers;
        progress.failedServers = failedServers;
        progress.processedServers = progress.totalServers;
        if (typeof progress.totalChunks === 'number' && progress.totalChunks > 0) {
            progress.processedChunks = progress.totalChunks;
        }
        progress.progressRatio = 1;
        progress.complete = true;
        if (result.failed > 0 && result.errors.length > 0) {
            progress.error = result.errors[0].error.message;
        }
        emitProgress();
    })().catch((err) => {
        if (progress.complete)
            return;
        progress.failedServers = progress.totalServers;
        progress.processedServers = progress.totalServers;
        if (typeof progress.totalChunks === 'number' && progress.totalChunks > 0) {
            progress.processedChunks = progress.totalChunks;
        }
        progress.progressRatio = 1;
        progress.complete = true;
        progress.error = getErrorMessage(err);
        emitProgress();
    });
}
function respondBlobStored(id, fileCid, upload) {
    const hashHex = toHex(fileCid.hash);
    const nhash = nhashEncode(fileCid);
    if (upload) {
        startBlossomUploadProgress(hashHex, nhash, fileCid);
    }
    respond({
        type: 'blobStored',
        id,
        hashHex,
        nhash,
    });
}
async function handleRequest(req) {
    switch (req.type) {
        case 'init': {
            init(req.config);
            respond({ type: 'ready', id: req.id });
            return;
        }
        case 'close': {
            resetState();
            respond({ type: 'void', id: req.id });
            return;
        }
        case 'putBlob': {
            if (!storage || !blossom || !tree) {
                respond({ type: 'error', id: req.id, error: 'Worker not initialized' });
                return;
            }
            let fileCid;
            if (req.upload === false) {
                const hash = await tree.putBlob(req.data);
                fileCid = { hash };
            }
            else {
                const fileResult = await tree.putFile(req.data);
                fileCid = fileResult.cid;
                assertEncryptedUploadCid(fileCid);
                await markEncryptedTreeHashesAsPeerShareable(fileCid);
            }
            respondBlobStored(req.id, fileCid, req.upload !== false);
            return;
        }
        case 'putBlock': {
            if (!storage) {
                respond({ type: 'error', id: req.id, error: 'Worker not initialized' });
                return;
            }
            const [storedBlock] = await storeAndMaybeUploadRawBlocks([{
                    data: req.data,
                    hashHex: req.hashHex,
                    mimeType: req.mimeType,
                }], req.upload === true);
            respond({
                type: 'blockStored',
                id: req.id,
                block: {
                    hashHex: storedBlock.hashHex,
                    nhash: storedBlock.nhash,
                },
            });
            return;
        }
        case 'putBlocks': {
            if (!storage) {
                respond({ type: 'error', id: req.id, error: 'Worker not initialized' });
                return;
            }
            const storedBlocks = await storeAndMaybeUploadRawBlocks(req.blocks, req.upload === true);
            respond({
                type: 'blocksStored',
                id: req.id,
                blocks: storedBlocks.map(({ hashHex, nhash }) => ({ hashHex, nhash })),
            });
            return;
        }
        case 'beginPutBlobStream': {
            if (!tree) {
                respond({ type: 'error', id: req.id, error: 'Worker not initialized' });
                return;
            }
            const upload = req.upload !== false;
            const streamId = nextPutBlobStreamId();
            const writer = tree.createStream({ unencrypted: !upload });
            activePutBlobStreams.set(streamId, { upload, writer });
            respond({ type: 'blobStreamStarted', id: req.id, streamId });
            return;
        }
        case 'appendPutBlobStream': {
            const stream = activePutBlobStreams.get(req.streamId);
            if (!stream) {
                respond({ type: 'void', id: req.id, error: 'Upload stream not found' });
                return;
            }
            await stream.writer.append(req.chunk);
            respond({ type: 'void', id: req.id });
            return;
        }
        case 'finishPutBlobStream': {
            const stream = activePutBlobStreams.get(req.streamId);
            if (!stream) {
                respond({ type: 'error', id: req.id, error: 'Upload stream not found' });
                return;
            }
            activePutBlobStreams.delete(req.streamId);
            const finalized = await stream.writer.finalize();
            const fileCid = finalized.key
                ? { hash: finalized.hash, key: finalized.key }
                : { hash: finalized.hash };
            if (stream.upload) {
                assertEncryptedUploadCid(fileCid);
                await markEncryptedTreeHashesAsPeerShareable(fileCid);
            }
            respondBlobStored(req.id, fileCid, stream.upload);
            return;
        }
        case 'cancelPutBlobStream': {
            activePutBlobStreams.delete(req.streamId);
            respond({ type: 'void', id: req.id });
            return;
        }
        case 'p2pFetchResult': {
            resolveP2PFetch(req.requestId, req.data, req.error);
            return;
        }
        case 'p2pPeerListResult': {
            resolveP2PPeerList(req.requestId, req.peerIds, req.error);
            return;
        }
        case 'getBlob': {
            if (!storage) {
                respond({ type: 'blob', id: req.id, error: 'Worker not initialized' });
                return;
            }
            const loaded = req.forPeer
                ? await loadPeerBlobData(req.hashHex)
                : await loadBlobData(req.hashHex);
            if (!loaded) {
                respond({
                    type: 'blob',
                    id: req.id,
                    error: req.forPeer
                        ? 'Refusing to serve blob to peer because it is not reachable from a shared read source'
                        : 'Blob not found',
                });
                return;
            }
            respond({ type: 'blob', id: req.id, data: loaded.data, source: loaded.source });
            return;
        }
        case 'registerMediaPort': {
            if (!storage) {
                respond({ type: 'void', id: req.id, error: 'Worker not initialized' });
                return;
            }
            registerMediaPort(req.port);
            respond({ type: 'void', id: req.id });
            return;
        }
        case 'setBlossomServers': {
            if (!blossom) {
                respond({ type: 'void', id: req.id, error: 'Worker not initialized' });
                return;
            }
            blossom.setServers(req.servers);
            respond({ type: 'void', id: req.id });
            void emitConnectivityUpdate();
            return;
        }
        case 'setStorageMaxBytes': {
            if (!storage) {
                respond({ type: 'void', id: req.id, error: 'Worker not initialized' });
                return;
            }
            storage.setMaxBytes(req.maxBytes);
            respond({ type: 'void', id: req.id });
            return;
        }
        case 'getStorageStats': {
            if (!storage) {
                respond({
                    type: 'storageStats',
                    id: req.id,
                    items: 0,
                    bytes: 0,
                    maxBytes: 0,
                    error: 'Worker not initialized',
                });
                return;
            }
            const stats = await storage.getStats();
            respond({ type: 'storageStats', id: req.id, ...stats });
            return;
        }
        case 'probeConnectivity': {
            if (!blossom) {
                respond({ type: 'connectivity', id: req.id, error: 'Worker not initialized' });
                return;
            }
            const state = await probeConnectivity(blossom.getServers());
            respond({ type: 'connectivity', id: req.id, state });
            return;
        }
        case 'resolveRoot': {
            if (!tree) {
                respond({ type: 'cid', id: req.id, error: 'Worker not initialized' });
                return;
            }
            try {
                const cid = await resolveRootPathFromRelays(tree, nostrRelays, req.npub, req.path, req.timeoutMs, req.settleMs);
                respond({ type: 'cid', id: req.id, cid: cid ?? undefined });
            }
            catch (err) {
                respond({ type: 'cid', id: req.id, error: getErrorMessage(err) });
            }
            return;
        }
        case 'watchRoot': {
            if (!tree) {
                respond({ type: 'rootWatchStarted', id: req.id, watchId: '', error: 'Worker not initialized' });
                return;
            }
            const watchId = nextRootWatchId();
            try {
                const watch = await watchRootPathFromRelays(tree, nostrRelays, req.npub, req.path, (cid) => {
                    respond({ type: 'rootUpdate', watchId, cid: cid ?? undefined });
                }, req.timeoutMs, req.settleMs);
                activeRootWatches.set(watchId, { close: watch.close });
                respond({
                    type: 'rootWatchStarted',
                    id: req.id,
                    watchId,
                    ...(watch.initialCid ? { cid: watch.initialCid } : {}),
                });
            }
            catch (err) {
                respond({ type: 'rootWatchStarted', id: req.id, watchId: '', error: getErrorMessage(err) });
            }
            return;
        }
        case 'unwatchRoot': {
            const watch = activeRootWatches.get(req.watchId);
            activeRootWatches.delete(req.watchId);
            if (watch) {
                await Promise.resolve(watch.close()).catch(() => undefined);
            }
            respond({ type: 'void', id: req.id });
            return;
        }
    }
}
function isWorkerRequestMessage(value) {
    return Boolean(value
        && typeof value === 'object'
        && typeof value.type === 'string');
}
export function attachHashtreeWorker(target = self) {
    if (endpoint && endpointListener) {
        endpoint.removeEventListener('message', endpointListener);
    }
    endpoint = target;
    endpointListener = ((event) => {
        const req = event.data;
        if (!isWorkerRequestMessage(req)) {
            return;
        }
        void handleRequest(req).catch((err) => {
            respond({ type: 'error', id: req.id, error: getErrorMessage(err) });
        });
    });
    endpoint.addEventListener('message', endpointListener);
    endpoint.start?.();
    return () => {
        target.removeEventListener('message', endpointListener);
        if (endpoint === target) {
            endpoint = null;
            endpointListener = null;
        }
    };
}
//# sourceMappingURL=worker.js.map