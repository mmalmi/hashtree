import { generateRequestId } from '@hashtree/core';
const REQUEST_TIMEOUT_MS = 30_000;
const PUT_BLOB_TIMEOUT_MS = 15 * 60_000;
const STREAM_APPEND_TIMEOUT_MS = 60_000;
export class HashtreeWorkerClient {
    workerFactory;
    config;
    worker = null;
    initPromise = null;
    pending = new Map();
    connectivityListeners = new Set();
    uploadProgressListeners = new Set();
    blossomBandwidthListeners = new Set();
    diagnosticListeners = new Set();
    rootWatchListeners = new Map();
    pendingRootWatchUpdates = new Map();
    p2pFetchHandler = null;
    p2pPeerListHandler = null;
    constructor(workerFactory, config = {}) {
        this.workerFactory = workerFactory;
        this.config = config;
    }
    async init() {
        if (this.initPromise)
            return this.initPromise;
        try {
            this.spawnWorker();
        }
        catch (err) {
            throw err instanceof Error ? err : new Error(String(err));
        }
        this.initPromise = new Promise((resolve, reject) => {
            if (!this.worker) {
                reject(new Error('Failed to create worker'));
                return;
            }
            const id = generateRequestId();
            const timeoutId = setTimeout(() => {
                this.pending.delete(id);
                reject(new Error('Worker init timed out'));
            }, REQUEST_TIMEOUT_MS);
            this.pending.set(id, {
                resolve: (message) => {
                    if (message.type === 'ready') {
                        resolve();
                        return;
                    }
                    reject(new Error('Unexpected init response'));
                },
                reject: (error) => reject(error),
                timeoutId,
            });
            this.worker.postMessage({
                type: 'init',
                id,
                config: this.config,
            });
        });
        return this.initPromise;
    }
    spawnWorker() {
        if (this.workerFactory instanceof URL) {
            this.worker = new Worker(this.workerFactory, { type: 'module' });
        }
        else if (typeof this.workerFactory === 'string') {
            this.worker = new Worker(this.workerFactory, { type: 'module' });
        }
        else {
            this.worker = new this.workerFactory();
        }
        this.worker.onmessage = (event) => {
            const message = event.data;
            if (message.type === 'connectivityUpdate') {
                this.connectivityListeners.forEach(listener => listener(message.state));
                return;
            }
            if (message.type === 'uploadProgress') {
                this.uploadProgressListeners.forEach(listener => listener(message.progress));
                return;
            }
            if (message.type === 'blossomBandwidth') {
                this.blossomBandwidthListeners.forEach(listener => listener(message.stats));
                return;
            }
            if (message.type === 'diagnostic') {
                this.diagnosticListeners.forEach(listener => listener(message.event));
                return;
            }
            if (message.type === 'rootUpdate') {
                const listener = this.rootWatchListeners.get(message.watchId);
                if (listener) {
                    listener(message.cid ?? null);
                }
                else {
                    this.pendingRootWatchUpdates.set(message.watchId, message.cid ?? null);
                }
                return;
            }
            if (message.type === 'p2pFetch') {
                void this.handleP2PFetch(message.requestId, message.hashHex, message.peerId);
                return;
            }
            if (message.type === 'p2pPeerList') {
                void this.handleP2PPeerList(message.requestId);
                return;
            }
            if (message.type === 'error' && message.id) {
                this.rejectPending(message.id, new Error(message.error));
                return;
            }
            if ('id' in message && typeof message.id === 'string') {
                this.resolvePending(message.id, message);
            }
        };
        this.worker.onerror = (event) => {
            const errorMessage = event instanceof ErrorEvent ? event.message : 'Worker error';
            this.rejectAllPending(new Error(errorMessage));
        };
    }
    rejectAllPending(error) {
        for (const [id, pending] of this.pending.entries()) {
            clearTimeout(pending.timeoutId);
            pending.reject(error);
            this.pending.delete(id);
        }
    }
    resolvePending(id, message) {
        const pending = this.pending.get(id);
        if (!pending)
            return;
        clearTimeout(pending.timeoutId);
        pending.resolve(message);
        this.pending.delete(id);
    }
    rejectPending(id, error) {
        const pending = this.pending.get(id);
        if (!pending)
            return;
        clearTimeout(pending.timeoutId);
        pending.reject(error);
        this.pending.delete(id);
    }
    async handleP2PFetch(requestId, hashHex, peerId) {
        if (!this.worker)
            return;
        const id = generateRequestId();
        if (!this.p2pFetchHandler) {
            this.worker.postMessage({
                type: 'p2pFetchResult',
                id,
                requestId,
            });
            return;
        }
        try {
            const data = await this.p2pFetchHandler(hashHex, peerId);
            if (data && data.byteLength > 0) {
                const transferableData = data.slice();
                this.worker.postMessage({
                    type: 'p2pFetchResult',
                    id,
                    requestId,
                    data: transferableData,
                }, [transferableData.buffer]);
                return;
            }
            this.worker.postMessage({
                type: 'p2pFetchResult',
                id,
                requestId,
            });
        }
        catch (err) {
            const error = err instanceof Error ? err.message : String(err);
            this.worker.postMessage({
                type: 'p2pFetchResult',
                id,
                requestId,
                error,
            });
        }
    }
    async handleP2PPeerList(requestId) {
        if (!this.worker)
            return;
        const id = generateRequestId();
        if (!this.p2pPeerListHandler) {
            this.worker.postMessage({
                type: 'p2pPeerListResult',
                id,
                requestId,
                peerIds: [],
            });
            return;
        }
        try {
            const peerIds = await this.p2pPeerListHandler();
            this.worker.postMessage({
                type: 'p2pPeerListResult',
                id,
                requestId,
                peerIds: Array.isArray(peerIds) ? peerIds : [],
            });
        }
        catch (err) {
            const error = err instanceof Error ? err.message : String(err);
            this.worker.postMessage({
                type: 'p2pPeerListResult',
                id,
                requestId,
                error,
            });
        }
    }
    async request(payload, timeoutMs = REQUEST_TIMEOUT_MS, transfer = []) {
        await this.initIfNeeded();
        if (!this.worker) {
            throw new Error('Worker not initialized');
        }
        const id = generateRequestId();
        const message = { ...payload, id };
        return new Promise((resolve, reject) => {
            const timeoutId = setTimeout(() => {
                this.pending.delete(id);
                reject(new Error(`Worker request timed out: ${payload.type}`));
            }, timeoutMs);
            this.pending.set(id, { resolve, reject, timeoutId });
            this.worker?.postMessage(message, transfer);
        });
    }
    async initIfNeeded() {
        if (!this.initPromise) {
            await this.init();
            return;
        }
        await this.initPromise;
    }
    async putBlob(data, mimeType, upload = true) {
        const res = await this.request({ type: 'putBlob', data, mimeType, upload }, PUT_BLOB_TIMEOUT_MS);
        if (res.type !== 'blobStored') {
            throw new Error('Unexpected response for putBlob');
        }
        if (!res.hashHex || !res.nhash) {
            throw new Error('Failed to store blob');
        }
        return { hashHex: res.hashHex, nhash: res.nhash };
    }
    async putBlock(data, options = {}) {
        const res = await this.request({
            type: 'putBlock',
            data,
            hashHex: options.hashHex,
            mimeType: options.mimeType,
            upload: options.upload,
        }, PUT_BLOB_TIMEOUT_MS);
        if (res.type !== 'blockStored') {
            throw new Error('Unexpected response for putBlock');
        }
        return res.block;
    }
    async putBlocks(blocks, options = {}) {
        const res = await this.request({
            type: 'putBlocks',
            blocks,
            upload: options.upload,
        }, PUT_BLOB_TIMEOUT_MS);
        if (res.type !== 'blocksStored') {
            throw new Error('Unexpected response for putBlocks');
        }
        return res.blocks;
    }
    async beginPutBlobStream(mimeType, upload = true) {
        const res = await this.request({ type: 'beginPutBlobStream', mimeType, upload });
        if (res.type !== 'blobStreamStarted') {
            throw new Error('Unexpected response for beginPutBlobStream');
        }
        if (!res.streamId) {
            throw new Error('Failed to start blob stream');
        }
        return res.streamId;
    }
    async appendPutBlobStream(streamId, chunk) {
        const res = await this.request({ type: 'appendPutBlobStream', streamId, chunk }, STREAM_APPEND_TIMEOUT_MS, [chunk.buffer]);
        if (res.type !== 'void') {
            throw new Error('Unexpected response for appendPutBlobStream');
        }
        if (res.error) {
            throw new Error(res.error);
        }
    }
    async finishPutBlobStream(streamId) {
        const res = await this.request({ type: 'finishPutBlobStream', streamId }, PUT_BLOB_TIMEOUT_MS);
        if (res.type !== 'blobStored') {
            throw new Error('Unexpected response for finishPutBlobStream');
        }
        if (!res.hashHex || !res.nhash) {
            throw new Error('Failed to finalize blob stream');
        }
        return { hashHex: res.hashHex, nhash: res.nhash };
    }
    async cancelPutBlobStream(streamId) {
        const res = await this.request({ type: 'cancelPutBlobStream', streamId });
        if (res.type !== 'void') {
            throw new Error('Unexpected response for cancelPutBlobStream');
        }
        if (res.error) {
            throw new Error(res.error);
        }
    }
    async getBlob(hashHex, options = {}) {
        const res = await this.request({
            type: 'getBlob',
            hashHex,
            sourceIds: options.sourceIds ? [...options.sourceIds] : undefined,
            skipPrimary: options.skipPrimary,
        });
        if (res.type !== 'blob') {
            throw new Error('Unexpected response for getBlob');
        }
        if (res.error || !res.data || !res.source) {
            throw new Error(res.error || 'Blob not found');
        }
        return { data: res.data, source: res.source };
    }
    async hasBlob(hashHex, options = {}) {
        const res = await this.request({
            type: 'hasBlob',
            hashHex,
            sourceIds: options.sourceIds ? [...options.sourceIds] : undefined,
            skipPrimary: options.skipPrimary,
        });
        if (res.type !== 'availability') {
            throw new Error('Unexpected response for hasBlob');
        }
        if (res.error) {
            throw new Error(res.error);
        }
        return { available: res.available, size: res.size, source: res.source };
    }
    async getBlobForPeer(hashHex) {
        const res = await this.request({ type: 'getBlob', hashHex, forPeer: true });
        if (res.type !== 'blob') {
            throw new Error('Unexpected response for getBlobForPeer');
        }
        if (res.error || !res.data) {
            return null;
        }
        return res.data;
    }
    async setBlossomServers(servers) {
        const res = await this.request({ type: 'setBlossomServers', servers });
        if (res.type !== 'void') {
            throw new Error('Unexpected response for setBlossomServers');
        }
        if (res.error) {
            throw new Error(res.error);
        }
    }
    async registerMediaPort(port) {
        const res = await this.request({ type: 'registerMediaPort', port }, REQUEST_TIMEOUT_MS, [port]);
        if (res.type !== 'void') {
            throw new Error('Unexpected response for registerMediaPort');
        }
        if (res.error) {
            throw new Error(res.error);
        }
    }
    async setStorageMaxBytes(maxBytes) {
        const res = await this.request({ type: 'setStorageMaxBytes', maxBytes });
        if (res.type !== 'void') {
            throw new Error('Unexpected response for setStorageMaxBytes');
        }
        if (res.error) {
            throw new Error(res.error);
        }
    }
    async getStorageStats() {
        const res = await this.request({ type: 'getStorageStats' });
        if (res.type !== 'storageStats') {
            throw new Error('Unexpected response for getStorageStats');
        }
        if (res.error) {
            throw new Error(res.error);
        }
        return {
            items: res.items,
            bytes: res.bytes,
            maxBytes: res.maxBytes,
        };
    }
    async probeConnectivity() {
        const res = await this.request({ type: 'probeConnectivity' });
        if (res.type !== 'connectivity') {
            throw new Error('Unexpected response for probeConnectivity');
        }
        if (res.error || !res.state) {
            throw new Error(res.error || 'Connectivity probe failed');
        }
        return res.state;
    }
    async resolveRoot(npub, path, options = {}) {
        const res = await this.request({
            type: 'resolveRoot',
            npub,
            path,
            timeoutMs: options.timeoutMs,
            settleMs: options.settleMs,
        });
        if (res.type !== 'cid') {
            throw new Error('Unexpected response for resolveRoot');
        }
        if (res.error) {
            throw new Error(res.error);
        }
        return res.cid ?? null;
    }
    async watchRoot(npub, path, listener, options = {}) {
        const res = await this.request({
            type: 'watchRoot',
            npub,
            path,
            timeoutMs: options.timeoutMs,
            settleMs: options.settleMs,
        });
        if (res.type !== 'rootWatchStarted') {
            throw new Error('Unexpected response for watchRoot');
        }
        if (res.error) {
            throw new Error(res.error);
        }
        this.rootWatchListeners.set(res.watchId, listener);
        if ('cid' in res) {
            listener(res.cid ?? null);
        }
        if (this.pendingRootWatchUpdates.has(res.watchId)) {
            const pendingCid = this.pendingRootWatchUpdates.get(res.watchId) ?? null;
            this.pendingRootWatchUpdates.delete(res.watchId);
            listener(pendingCid);
        }
        return async () => {
            this.rootWatchListeners.delete(res.watchId);
            this.pendingRootWatchUpdates.delete(res.watchId);
            try {
                const stopRes = await this.request({ type: 'unwatchRoot', watchId: res.watchId });
                if (stopRes.type !== 'void') {
                    throw new Error('Unexpected response for unwatchRoot');
                }
                if (stopRes.error) {
                    throw new Error(stopRes.error);
                }
            }
            catch {
                // Ignore cleanup failures after local listener removal.
            }
        };
    }
    onConnectivityUpdate(listener) {
        this.connectivityListeners.add(listener);
        return () => {
            this.connectivityListeners.delete(listener);
        };
    }
    onUploadProgress(listener) {
        this.uploadProgressListeners.add(listener);
        return () => {
            this.uploadProgressListeners.delete(listener);
        };
    }
    onBlossomBandwidth(listener) {
        this.blossomBandwidthListeners.add(listener);
        return () => {
            this.blossomBandwidthListeners.delete(listener);
        };
    }
    onDiagnostic(listener) {
        this.diagnosticListeners.add(listener);
        return () => {
            this.diagnosticListeners.delete(listener);
        };
    }
    setP2PFetchHandler(handler) {
        this.p2pFetchHandler = handler;
    }
    setP2PPeerListHandler(handler) {
        this.p2pPeerListHandler = handler;
    }
    setP2PProvider(provider) {
        this.p2pFetchHandler = provider
            ? (hashHex, peerId) => provider.fetch(hashHex, peerId)
            : null;
        this.p2pPeerListHandler = provider
            ? () => provider.listPeerIds()
            : null;
    }
    async close() {
        try {
            await this.request({ type: 'close' });
        }
        catch {
            // Ignore close errors.
        }
        this.rootWatchListeners.clear();
        this.pendingRootWatchUpdates.clear();
        this.worker?.terminate();
        this.worker = null;
        this.initPromise = null;
        this.rejectAllPending(new Error('Worker closed'));
    }
}
//# sourceMappingURL=client.js.map