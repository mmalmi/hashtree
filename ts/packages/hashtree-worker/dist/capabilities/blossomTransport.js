import { BlossomStore, fromHex, } from '@hashtree/core';
import { finalizeEvent, generateSecretKey } from 'nostr-tools/pure';
import { BlossomBandwidthTracker, } from './blossomBandwidthTracker.js';
export const DEFAULT_BLOSSOM_SERVERS = [];
const MAX_CONCURRENT_READ_FETCHES = 32;
const DEFAULT_FETCH_TIMEOUT_MS = 6_000;
let activeReadFetches = 0;
const pendingReadFetchWaiters = [];
function normalizeServerUrl(url) {
    return url.replace(/\/+$/, '');
}
function normalizeServers(servers) {
    const source = servers && servers.length > 0 ? servers : DEFAULT_BLOSSOM_SERVERS;
    const unique = new Map();
    for (const server of source) {
        const url = normalizeServerUrl(server.url.trim());
        if (!url)
            continue;
        unique.set(url, {
            url,
            read: server.read ?? true,
            write: server.write ?? false,
        });
    }
    return Array.from(unique.values());
}
function createEphemeralSigner() {
    const secretKey = generateSecretKey();
    return async (template) => {
        const event = finalizeEvent({
            ...template,
            kind: template.kind,
            created_at: template.created_at,
            content: template.content,
            tags: template.tags,
        }, secretKey);
        return {
            kind: event.kind,
            created_at: event.created_at,
            content: event.content,
            tags: event.tags,
            pubkey: event.pubkey,
            id: event.id,
            sig: event.sig,
        };
    };
}
function releaseReadFetchSlot() {
    activeReadFetches = Math.max(0, activeReadFetches - 1);
    pendingReadFetchWaiters.shift()?.();
}
function withReadFetchSlot(loader) {
    return new Promise((resolve, reject) => {
        const start = () => {
            activeReadFetches += 1;
            let pending;
            try {
                pending = loader();
            }
            catch (error) {
                releaseReadFetchSlot();
                reject(error);
                return;
            }
            pending
                .then(resolve, reject)
                .finally(() => {
                releaseReadFetchSlot();
            });
        };
        if (activeReadFetches < MAX_CONCURRENT_READ_FETCHES) {
            start();
            return;
        }
        pendingReadFetchWaiters.push(start);
    });
}
export class BlossomTransport {
    servers;
    signer;
    bandwidthTracker;
    inflightFetches = new Map();
    fetchTimeoutMs;
    store;
    constructor(servers, onBandwidthUpdate, fetchTimeoutMs = DEFAULT_FETCH_TIMEOUT_MS) {
        this.servers = normalizeServers(servers);
        this.signer = createEphemeralSigner();
        this.bandwidthTracker = new BlossomBandwidthTracker(onBandwidthUpdate);
        this.fetchTimeoutMs = fetchTimeoutMs;
        this.store = this.createStore(this.servers);
    }
    setServers(servers) {
        this.servers = normalizeServers(servers);
        this.store = this.createStore(this.servers);
    }
    getServers() {
        return this.servers;
    }
    getReadServers() {
        return this.servers.filter((server) => server.read !== false);
    }
    getWriteServers() {
        return this.servers.filter(server => !!server.write);
    }
    getBandwidthStats() {
        return this.bandwidthTracker.getStats();
    }
    createStore(servers, onUploadProgress) {
        return new BlossomStore({
            servers,
            signer: this.signer,
            onUploadProgress,
            logger: (entry) => {
                this.bandwidthTracker.apply(entry);
            },
        });
    }
    createUploadStore(onUploadProgress) {
        return this.createStore(this.servers, onUploadProgress);
    }
    async upload(hashHex, data, _mimeType, onUploadProgress) {
        if (!this.servers.some(server => server.write))
            return;
        const uploadMimeType = 'application/octet-stream';
        if (onUploadProgress) {
            const store = this.createStore(this.servers, onUploadProgress);
            await store.put(fromHex(hashHex), data, uploadMimeType);
            return;
        }
        await this.store.put(fromHex(hashHex), data, uploadMimeType);
    }
    async fetch(hashHex) {
        const inflight = this.inflightFetches.get(hashHex);
        if (inflight) {
            return inflight;
        }
        const pending = this.fetchInternal(hashHex, () => this.store.get(fromHex(hashHex)));
        this.inflightFetches.set(hashHex, pending);
        return await pending;
    }
    async fetchFromServer(hashHex, serverUrl) {
        const normalizedServerUrl = normalizeServerUrl(serverUrl.trim());
        if (!normalizedServerUrl) {
            return null;
        }
        const key = `${normalizedServerUrl}::${hashHex}`;
        const inflight = this.inflightFetches.get(key);
        if (inflight) {
            return inflight;
        }
        const pending = this.fetchInternal(key, () => this.store.getFromServers(fromHex(hashHex), [normalizedServerUrl]));
        this.inflightFetches.set(key, pending);
        return await pending;
    }
    fetchInternal(inflightKey, loader) {
        return withReadFetchSlot(() => new Promise((resolve, reject) => {
            let settled = false;
            const timeoutId = setTimeout(() => {
                if (settled)
                    return;
                settled = true;
                resolve(null);
            }, this.fetchTimeoutMs);
            loader()
                .then((data) => {
                if (settled)
                    return;
                settled = true;
                clearTimeout(timeoutId);
                resolve(data);
            })
                .catch((error) => {
                if (settled)
                    return;
                settled = true;
                clearTimeout(timeoutId);
                reject(error);
            });
        }))
            .finally(() => {
            this.inflightFetches.delete(inflightKey);
        });
    }
}
//# sourceMappingURL=blossomTransport.js.map