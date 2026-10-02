import { serveNostrSource } from './nostrSourcePort.js';
const REQUEST_TIMEOUT_MS = 30_000;
export class RelayWorkerClient {
    workerFactory;
    config;
    worker = null;
    workerReady = false;
    p2pProvider = null;
    p2pProviderEnabledAtInit = false;
    p2pPeerListSupportedAtInit = false;
    initPromise = null;
    initPending = null;
    pendingRequests = new Map();
    eventListeners = new Map();
    sourceBridge = null;
    sourceRevision = 0;
    treeRootListeners = new Set();
    blossomBandwidthListeners = new Set();
    constructor(workerFactory, config) {
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
        this.p2pProviderEnabledAtInit = this.p2pProvider !== null;
        this.p2pPeerListSupportedAtInit = this.p2pProvider?.listPeerIds !== undefined;
        this.initPromise = new Promise((resolve, reject) => {
            if (!this.worker) {
                reject(new Error('Failed to create worker'));
                return;
            }
            const timeoutId = setTimeout(() => {
                this.initPending = null;
                this.initPromise = null;
                reject(new Error('Worker init timed out'));
            }, REQUEST_TIMEOUT_MS);
            this.initPending = {
                resolve,
                reject,
                timeoutId,
            };
            this.worker.postMessage({
                type: 'init',
                id: this.nextRequestId('worker_init'),
                config: this.config,
                p2pProviderEnabled: this.p2pProviderEnabledAtInit,
                p2pPeerListSupported: this.p2pPeerListSupportedAtInit,
            });
        });
        return this.initPromise;
    }
    spawnWorker() {
        this.workerReady = false;
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
            if (message.type === 'ready') {
                this.workerReady = true;
                if ((this.p2pProvider !== null) !== this.p2pProviderEnabledAtInit
                    || (this.p2pProvider?.listPeerIds !== undefined) !== this.p2pPeerListSupportedAtInit) {
                    this.notifyP2PProviderState();
                }
                if (this.initPending) {
                    clearTimeout(this.initPending.timeoutId);
                    this.initPending.resolve();
                    this.initPending = null;
                }
                return;
            }
            if (message.type === 'void' && message.error && message.id && this.eventListeners.has(message.id)) {
                this.eventListeners.get(message.id)?.onError?.(new Error(message.error));
                return;
            }
            if (message.type === 'event') {
                this.eventListeners.get(message.subId)?.onEvent(message.event, message.info ?? { source: 'worker', cached: false });
                return;
            }
            if (message.type === 'nostrStatus') {
                this.eventListeners.get(message.subId)?.onEose?.(message.status);
                return;
            }
            if (message.type === 'blossomBandwidth') {
                for (const listener of this.blossomBandwidthListeners) {
                    listener(message.stats);
                }
                return;
            }
            if (message.type === 'treeRootUpdate') {
                for (const listener of this.treeRootListeners) {
                    const { type, ...update } = message;
                    void type;
                    listener(update);
                }
                return;
            }
            if (message.type === 'p2pFetch') {
                void this.handleP2PFetch(message.requestId, message.hashHex, message.htl, message.peerId);
                return;
            }
            if (message.type === 'p2pPeerList') {
                void this.handleP2PPeerList(message.requestId);
                return;
            }
            if (message.type === 'signEvent') {
                void this.handleSignRequest(message);
                return;
            }
            if (message.type === 'nip44Encrypt') {
                void this.handleEncryptRequest(message);
                return;
            }
            if (message.type === 'nip44Decrypt') {
                void this.handleDecryptRequest(message);
                return;
            }
            if (message.type === 'error' && message.id) {
                const errorMessage = typeof message.error === 'string' ? message.error : 'Worker error';
                this.rejectPending(message.id, new Error(errorMessage));
                return;
            }
            if ('id' in message && typeof message.id === 'string') {
                this.resolvePending(message.id, message);
            }
        };
        this.worker.onerror = (event) => {
            this.workerReady = false;
            const errorMessage = event instanceof ErrorEvent ? event.message : 'Worker error';
            this.rejectAllPending(new Error(errorMessage));
        };
    }
    async handleP2PFetch(requestId, hashHex, htl, peerId) {
        if (!this.worker)
            return;
        const id = this.nextRequestId('p2p_fetch_result');
        try {
            if (!this.p2pProvider) {
                throw new Error('P2P provider is not configured');
            }
            const data = await this.p2pProvider.fetch(hashHex, peerId, htl);
            if (data !== null) {
                const transferableData = data.slice();
                this.worker.postMessage({
                    type: 'p2pFetchResult',
                    id,
                    requestId,
                    data: transferableData,
                }, [transferableData.buffer]);
                return;
            }
            this.worker.postMessage({ type: 'p2pFetchResult', id, requestId });
        }
        catch (error) {
            this.worker.postMessage({
                type: 'p2pFetchResult',
                id,
                requestId,
                error: error instanceof Error ? error.message : String(error),
            });
        }
    }
    async handleP2PPeerList(requestId) {
        if (!this.worker)
            return;
        const id = this.nextRequestId('p2p_peer_list_result');
        try {
            const peerIds = await this.p2pProvider?.listPeerIds?.() ?? [];
            this.worker.postMessage({
                type: 'p2pPeerListResult',
                id,
                requestId,
                peerIds: [...new Set(peerIds)],
            });
        }
        catch (error) {
            this.worker.postMessage({
                type: 'p2pPeerListResult',
                id,
                requestId,
                error: error instanceof Error ? error.message : String(error),
            });
        }
    }
    getNostrExtension() {
        if (typeof window === 'undefined') {
            return null;
        }
        return window.nostr ?? null;
    }
    async handleSignRequest(message) {
        try {
            const nostr = this.getNostrExtension();
            if (!nostr?.signEvent) {
                throw new Error('NIP-07 extension not available');
            }
            const signed = await nostr.signEvent(message.event);
            this.worker?.postMessage({
                type: 'signed',
                id: message.id,
                event: signed,
            });
        }
        catch (error) {
            this.worker?.postMessage({
                type: 'signed',
                id: message.id,
                error: error instanceof Error ? error.message : String(error),
            });
        }
    }
    async handleEncryptRequest(message) {
        try {
            const nostr = this.getNostrExtension();
            if (!nostr?.nip44?.encrypt) {
                throw new Error('NIP-44 encryption not available');
            }
            const ciphertext = await nostr.nip44.encrypt(message.pubkey, message.plaintext);
            this.worker?.postMessage({
                type: 'encrypted',
                id: message.id,
                ciphertext,
            });
        }
        catch (error) {
            this.worker?.postMessage({
                type: 'encrypted',
                id: message.id,
                error: error instanceof Error ? error.message : String(error),
            });
        }
    }
    async handleDecryptRequest(message) {
        try {
            const nostr = this.getNostrExtension();
            if (!nostr?.nip44?.decrypt) {
                throw new Error('NIP-44 decryption not available');
            }
            const plaintext = await nostr.nip44.decrypt(message.pubkey, message.ciphertext);
            this.worker?.postMessage({
                type: 'decrypted',
                id: message.id,
                plaintext,
            });
        }
        catch (error) {
            this.worker?.postMessage({
                type: 'decrypted',
                id: message.id,
                error: error instanceof Error ? error.message : String(error),
            });
        }
    }
    nextRequestId(prefix) {
        if (typeof crypto !== 'undefined' && 'randomUUID' in crypto) {
            return `${prefix}_${crypto.randomUUID()}`;
        }
        return `${prefix}_${Date.now().toString(36)}_${Math.random().toString(36).slice(2)}`;
    }
    resolvePending(id, message) {
        const pending = this.pendingRequests.get(id);
        if (!pending)
            return;
        clearTimeout(pending.timeoutId);
        pending.resolve(message);
        this.pendingRequests.delete(id);
    }
    rejectPending(id, error) {
        const pending = this.pendingRequests.get(id);
        if (!pending)
            return;
        clearTimeout(pending.timeoutId);
        pending.reject(error);
        this.pendingRequests.delete(id);
    }
    rejectAllPending(error) {
        for (const [id, pending] of this.pendingRequests.entries()) {
            clearTimeout(pending.timeoutId);
            pending.reject(error);
            this.pendingRequests.delete(id);
        }
        if (this.initPending) {
            clearTimeout(this.initPending.timeoutId);
            this.initPending.reject(error);
            this.initPending = null;
        }
        this.initPromise = null;
    }
    async request(payload, timeoutMs = REQUEST_TIMEOUT_MS, transfer = [], signal) {
        await this.init();
        if (!this.worker) {
            throw new Error('Worker not initialized');
        }
        if (signal?.aborted)
            throw new DOMException('Worker request cancelled', 'AbortError');
        const id = this.nextRequestId(payload.type);
        const message = { ...payload, id };
        return new Promise((resolve, reject) => {
            const cleanup = () => { clearTimeout(timeoutId); signal?.removeEventListener('abort', abort); };
            const cancelRemote = () => {
                if (payload.type === 'query')
                    this.worker?.postMessage({ type: 'cancelNostrQuery', id: this.nextRequestId('cancel_query'), requestId: id });
            };
            const abort = () => {
                cleanup();
                cancelRemote();
                this.pendingRequests.delete(id);
                reject(new DOMException('Worker request cancelled', 'AbortError'));
            };
            const timeoutId = setTimeout(() => {
                cleanup();
                cancelRemote();
                this.pendingRequests.delete(id);
                reject(new Error(`Worker request timed out: ${payload.type}`));
            }, timeoutMs);
            this.pendingRequests.set(id, {
                resolve: (value) => { cleanup(); resolve(value); },
                reject: (error) => { cleanup(); reject(error); },
                timeoutId,
            });
            signal?.addEventListener('abort', abort, { once: true });
            this.worker?.postMessage(message, transfer);
        });
    }
    async registerMediaPort(port, debug) {
        await this.init();
        if (!this.worker) {
            throw new Error('Worker not initialized');
        }
        this.worker.postMessage({ type: 'registerMediaPort', port, debug }, [port]);
    }
    async getTreeRootInfo(npub, treeName) {
        const res = await this.request({ type: 'getTreeRootInfo', npub, treeName });
        if (res.type !== 'treeRootInfo') {
            throw new Error('Unexpected tree root response');
        }
        if (res.error) {
            throw new Error(res.error);
        }
        return res.record ?? null;
    }
    async getPeerStats() {
        const res = await this.request({ type: 'getPeerStats' });
        if (res.type !== 'peerStats') {
            throw new Error('Unexpected peer stats response');
        }
        return res.stats ?? [];
    }
    async getRelayStats() {
        const res = await this.request({ type: 'getRelayStats' });
        if (res.type !== 'relayStats') {
            throw new Error('Unexpected relay stats response');
        }
        return res.stats ?? [];
    }
    async setIdentity(pubkey, nsecHex) {
        const res = await this.request({ type: 'setIdentity', pubkey, nsec: nsecHex });
        if (res.type !== 'void') {
            throw new Error('Unexpected setIdentity response');
        }
        if (res.error) {
            throw new Error(res.error);
        }
    }
    subscribeEvents(filters, handlers) {
        const id = this.nextRequestId('nostr_sub');
        this.eventListeners.set(id, handlers);
        void this.init().then(() => {
            if (this.eventListeners.has(id))
                this.worker?.postMessage({ type: 'subscribe', id, filters });
        }).catch((error) => handlers.onError?.(error instanceof Error ? error : new Error(String(error))));
        return { close: () => {
                if (!this.eventListeners.delete(id))
                    return;
                if (this.workerReady)
                    this.worker?.postMessage({ type: 'unsubscribe', id: this.nextRequestId('nostr_close'), subId: id });
            } };
    }
    async queryEvents(filters, options = {}) {
        if (options.signal?.aborted)
            throw new DOMException('Query cancelled', 'AbortError');
        const { signal, ...wireOptions } = options;
        const response = await this.request({ type: 'query', filters, options: wireOptions }, REQUEST_TIMEOUT_MS, [], signal);
        if (signal?.aborted)
            throw new DOMException('Query cancelled', 'AbortError');
        if (response.type !== 'nostrQuery' || !response.result)
            throw new Error('error' in response && response.error || 'Invalid event query response');
        return response.result;
    }
    async publishEvent(event) {
        const response = await this.request({ type: 'publish', event });
        if (response.type !== 'void' || response.error || !response.receipt)
            throw new Error('error' in response && response.error || 'Invalid event publication receipt');
        return response.receipt;
    }
    /** Attach pubsub on the same FIPS node already used by the blob provider. */
    async setNostrSource(source) {
        const revision = ++this.sourceRevision;
        await this.init();
        if (revision !== this.sourceRevision)
            return;
        const previous = this.sourceBridge;
        this.sourceBridge = null;
        if (previous) {
            previous.close();
            await this.request({ type: 'detachNostrSource', sourceId: previous.id });
        }
        if (!source || revision !== this.sourceRevision)
            return;
        const channel = new MessageChannel();
        const close = serveNostrSource(channel.port1, source);
        this.sourceBridge = { id: source.id, close };
        try {
            await this.request({ type: 'attachNostrSource', sourceId: source.id, port: channel.port2, publishAcceptance: source.publishAcceptance }, REQUEST_TIMEOUT_MS, [channel.port2]);
        }
        catch (error) {
            close();
            if (this.sourceBridge?.close === close)
                this.sourceBridge = null;
            throw error;
        }
    }
    setP2PProvider(provider) {
        this.p2pProvider = provider;
        this.notifyP2PProviderState();
    }
    notifyP2PProviderState() {
        if (!this.workerReady)
            return;
        this.worker?.postMessage({
            type: 'setP2PProviderState',
            id: this.nextRequestId('p2p_provider_state'),
            enabled: this.p2pProvider !== null,
            peerListSupported: this.p2pProvider?.listPeerIds !== undefined,
        });
    }
    async setBlossomServers(servers) {
        const res = await this.request({ type: 'setBlossomServers', servers });
        if (res.type !== 'void') {
            throw new Error('Unexpected setBlossomServers response');
        }
        if (res.error) {
            throw new Error(res.error);
        }
    }
    async setStorageMaxBytes(maxBytes) {
        const res = await this.request({ type: 'setStorageMaxBytes', maxBytes });
        if (res.type !== 'void') {
            throw new Error('Unexpected setStorageMaxBytes response');
        }
        if (res.error) {
            throw new Error(res.error);
        }
    }
    async setRelays(relays) {
        const res = await this.request({ type: 'setRelays', relays });
        if (res.type !== 'void') {
            throw new Error('Unexpected setRelays response');
        }
        if (res.error) {
            throw new Error(res.error);
        }
    }
    async subscribeTreeRoots(pubkey) {
        const res = await this.request({ type: 'subscribeTreeRoots', pubkey });
        if (res.type !== 'void') {
            throw new Error('Unexpected tree root subscribe response');
        }
        if (res.error) {
            throw new Error(res.error);
        }
    }
    async unsubscribeTreeRoots(pubkey) {
        const res = await this.request({ type: 'unsubscribeTreeRoots', pubkey });
        if (res.type !== 'void') {
            throw new Error('Unexpected tree root unsubscribe response');
        }
        if (res.error) {
            throw new Error(res.error);
        }
    }
    onTreeRootUpdate(listener) {
        this.treeRootListeners.add(listener);
        return () => {
            this.treeRootListeners.delete(listener);
        };
    }
    onBlossomBandwidth(listener) {
        this.blossomBandwidthListeners.add(listener);
        return () => {
            this.blossomBandwidthListeners.delete(listener);
        };
    }
    async close() {
        ++this.sourceRevision;
        this.sourceBridge?.close();
        this.sourceBridge = null;
        this.eventListeners.clear();
        try {
            const res = await this.request({ type: 'close' });
            if (res.type !== 'void' && res.type !== 'error') {
                throw new Error('Unexpected response for close');
            }
        }
        catch {
            // Ignore close errors and always terminate locally.
        }
        this.blossomBandwidthListeners.clear();
        this.treeRootListeners.clear();
        this.p2pProvider = null;
        this.worker?.terminate();
        this.worker = null;
        this.workerReady = false;
        this.initPromise = null;
        this.initPending = null;
        this.rejectAllPending(new Error('Worker closed'));
    }
}
//# sourceMappingURL=relay-client.js.map