import { MemoryStore, sha256, toHex, } from '@hashtree/core';
import { createRequest, createResponse, encodeRequest, encodeResponse, hashToKey, MAX_HTL, MSG_TYPE_REQUEST, MSG_TYPE_RESPONSE, parseMessage, verifyHash, } from '@hashtree/mesh';
export const DEFAULT_FIPS_DISCOVERY_APP = 'hashtree-v1';
export const DEFAULT_FIPS_REQUEST_TIMEOUT_MS = 5_500;
const DYNAMIC_PEER_POLL_INTERVAL_MS = 100;
function copyBytes(data) {
    return data.slice();
}
function bytesToHex(data) {
    let out = '';
    for (const byte of data) {
        out += byte.toString(16).padStart(2, '0');
    }
    return out;
}
function bytesEqual(a, b) {
    if (a.length !== b.length)
        return false;
    for (let i = 0; i < a.length; i += 1) {
        if (a[i] !== b[i])
            return false;
    }
    return true;
}
function sleep(ms) {
    return new Promise((resolve) => setTimeout(resolve, Math.max(0, ms)));
}
function normalizePeers(peers, localPeerId) {
    const seen = new Set();
    const out = [];
    for (const peer of peers) {
        const id = `${peer}`.trim();
        if (!id || id === localPeerId || seen.has(id))
            continue;
        seen.add(id);
        out.push(id);
    }
    return out;
}
async function resolvePeerSource(endpoint, peers) {
    const resolved = typeof peers === 'function'
        ? await peers()
        : peers ?? await endpoint.listPeerIds?.() ?? [];
    return normalizePeers(resolved, endpoint.localPeerId?.());
}
async function verifiedLocalGet(store, hash) {
    const data = await store.get(hash);
    if (!data)
        return null;
    return await verifyHash(data, hash) ? data : null;
}
export function createFipsNodeEndpoint(node, options = {}) {
    const peers = new Set();
    const dataUnsubs = new Set();
    for (const peer of options.initialPeers ?? []) {
        const id = `${peer}`.trim();
        if (id)
            peers.add(id);
    }
    const localPeerId = node.identity?.publicKey ? bytesToHex(node.identity.publicKey) : undefined;
    const peerUnsub = node.on('peer', (event) => {
        if (event.state === 'connected') {
            peers.add(event.remotePubkey);
        }
        else {
            peers.delete(event.remotePubkey);
        }
    });
    return {
        localPeerId: () => localPeerId ?? '',
        listPeerIds: () => normalizePeers(Array.from(peers), localPeerId),
        send: (peerId, data) => node.sendEndpointData({
            dst: peerId,
            payload: copyBytes(data),
        }),
        onMessage: (handler) => {
            const dataUnsub = node.on('endpointData', (event) => {
                peers.add(event.src);
                void Promise.resolve(handler({
                    peerId: event.src,
                    data: copyBytes(event.payload),
                })).catch(() => undefined);
            });
            dataUnsubs.add(dataUnsub);
            return () => {
                dataUnsub();
                dataUnsubs.delete(dataUnsub);
            };
        },
        close: () => {
            peerUnsub();
            for (const unsubscribe of dataUnsubs)
                unsubscribe();
            dataUnsubs.clear();
        },
    };
}
export class HashtreeFipsTransport {
    endpoint;
    localStore;
    peers;
    requestTimeoutMs;
    requestHtl;
    cacheResponses;
    pending = new Map();
    unsubscribe = null;
    constructor(options) {
        this.endpoint = options.endpoint;
        this.localStore = options.localStore ?? new MemoryStore();
        this.peers = options.peers;
        this.requestTimeoutMs = options.requestTimeoutMs ?? DEFAULT_FIPS_REQUEST_TIMEOUT_MS;
        this.requestHtl = options.requestHtl ?? MAX_HTL;
        this.cacheResponses = options.cacheResponses ?? true;
        this.unsubscribe = this.endpoint.onMessage((message) => {
            void this.handleMessage(message).catch(() => undefined);
        });
    }
    close() {
        this.unsubscribe?.();
        this.unsubscribe = null;
        this.endpoint.close?.();
        for (const pending of this.pending.values()) {
            for (const request of pending) {
                clearTimeout(request.timer);
                request.resolve(null);
            }
        }
        this.pending.clear();
    }
    setPeers(peers) {
        this.peers = peers;
    }
    async get(hash, peers) {
        const local = await verifiedLocalGet(this.localStore, hash);
        if (local)
            return local;
        const peerSource = peers ?? this.peers;
        if (typeof peerSource === 'function') {
            return this.requestFromDynamicPeers(hash, peerSource);
        }
        const peerIds = await resolvePeerSource(this.endpoint, peerSource);
        if (peerIds.length === 0)
            return null;
        return this.requestFromPeers(hash, peerIds);
    }
    async put(hash, data) {
        const computed = await sha256(data);
        if (!bytesEqual(computed, hash)) {
            throw new Error(`hashtree fips transport put hash mismatch: ${toHex(hash)}`);
        }
        return this.localStore.put(hash, copyBytes(data));
    }
    async handleMessage(message) {
        const parsed = parseMessage(message.data);
        if (!parsed)
            return;
        if (parsed.type === MSG_TYPE_REQUEST) {
            await this.handleRequest(message.peerId, parsed.body);
            return;
        }
        if (parsed.type === MSG_TYPE_RESPONSE) {
            await this.handleResponse(parsed.body);
        }
    }
    createReadSource(id = 'fips') {
        return {
            id,
            get: (hash) => this.get(hash),
            isAvailable: () => true,
        };
    }
    async handleRequest(peerId, req) {
        const data = await verifiedLocalGet(this.localStore, req.h);
        if (!data)
            return;
        await this.endpoint.send(peerId, new Uint8Array(encodeResponse(createResponse(req.h, data))));
    }
    async handleResponse(resp) {
        if (!await verifyHash(resp.d, resp.h))
            return;
        const hashKey = hashToKey(resp.h);
        const pending = this.pending.get(hashKey);
        if (!pending)
            return;
        this.pending.delete(hashKey);
        if (this.cacheResponses) {
            await this.localStore.put(resp.h, copyBytes(resp.d)).catch(() => false);
        }
        for (const request of pending) {
            clearTimeout(request.timer);
            request.resolve(copyBytes(resp.d));
        }
    }
    async requestFromPeers(hash, peers) {
        const hashKey = hashToKey(hash);
        const pending = this.createPendingRequest(hashKey, hash);
        const payload = new Uint8Array(encodeRequest(createRequest(hash, this.requestHtl)));
        const sent = await this.sendRequestToPeers(payload, peers);
        if (sent === 0) {
            this.resolvePendingMiss(hashKey);
        }
        return pending.promise;
    }
    async requestFromDynamicPeers(hash, peers) {
        const hashKey = hashToKey(hash);
        const pending = this.createPendingRequest(hashKey, hash);
        const payload = new Uint8Array(encodeRequest(createRequest(hash, this.requestHtl)));
        const attempted = new Set();
        const deadline = Date.now() + this.requestTimeoutMs;
        while (!pending.isSettled()) {
            const peerIds = await resolvePeerSource(this.endpoint, peers).catch(() => []);
            const newPeers = peerIds.filter((peerId) => !attempted.has(peerId));
            for (const peerId of newPeers) {
                attempted.add(peerId);
            }
            if (newPeers.length > 0) {
                void this.sendRequestToPeers(payload, newPeers);
            }
            const remainingMs = deadline - Date.now();
            if (remainingMs <= 0) {
                break;
            }
            await Promise.race([
                pending.promise.then(() => undefined),
                sleep(Math.min(DYNAMIC_PEER_POLL_INTERVAL_MS, remainingMs)),
            ]);
        }
        return pending.promise;
    }
    createPendingRequest(hashKey, hash) {
        let settled = false;
        const promise = new Promise((resolve) => {
            const finish = (data) => {
                if (settled)
                    return;
                settled = true;
                resolve(data);
            };
            const timer = setTimeout(() => {
                this.removePendingRequest(hashKey, finish);
                finish(null);
            }, this.requestTimeoutMs);
            const pending = this.pending.get(hashKey) ?? [];
            pending.push({ hash, resolve: finish, timer });
            this.pending.set(hashKey, pending);
        });
        return {
            promise,
            isSettled: () => settled,
        };
    }
    removePendingRequest(hashKey, resolve) {
        const pending = this.pending.get(hashKey) ?? [];
        const remaining = pending.filter((request) => request.resolve !== resolve);
        if (remaining.length > 0) {
            this.pending.set(hashKey, remaining);
        }
        else {
            this.pending.delete(hashKey);
        }
    }
    async sendRequestToPeers(payload, peers) {
        const sends = await Promise.allSettled(peers.map((peerId) => this.endpoint.send(peerId, copyBytes(payload))));
        return sends.filter((result) => result.status === 'fulfilled').length;
    }
    resolvePendingMiss(hashKey) {
        const pending = this.pending.get(hashKey);
        if (!pending)
            return;
        this.pending.delete(hashKey);
        for (const request of pending) {
            clearTimeout(request.timer);
            request.resolve(null);
        }
    }
}
export class FipsTransportStore {
    transport;
    localStore;
    constructor(options) {
        this.localStore = options.localStore;
        this.transport = new HashtreeFipsTransport(options);
    }
    close() {
        this.transport.close();
    }
    put(hash, data) {
        return this.transport.put(hash, data);
    }
    async get(hash) {
        const local = await verifiedLocalGet(this.localStore, hash);
        if (local)
            return local;
        return this.transport.get(hash);
    }
    has(hash) {
        return this.localStore.has(hash);
    }
    delete(hash) {
        return this.localStore.delete(hash);
    }
    watch(hash, callback) {
        return this.localStore.watch?.(hash, callback) ?? (() => { });
    }
}
//# sourceMappingURL=index.js.map