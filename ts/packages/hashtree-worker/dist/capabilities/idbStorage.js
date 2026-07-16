import { fromHex, sha256, toHex } from '@hashtree/core';
import { DexieStore } from '@hashtree/dexie';
import Dexie from 'dexie';
const HASH_HEX_PATTERN = /^[0-9a-f]{64}$/;
function normalizeHashHex(hashHex) {
    const normalized = hashHex.toLowerCase();
    if (!HASH_HEX_PATTERN.test(normalized)) {
        throw new Error('Invalid blob hash');
    }
    return normalized;
}
function isCanonicalHashHex(value) {
    return typeof value === 'string' && HASH_HEX_PATTERN.test(value);
}
class PeerShareAuthorizationDB extends Dexie {
    authorizations;
    constructor(dbName) {
        super(`${dbName}-peer-share`);
        this.version(1).stores({ authorizations: '&hashHex' });
    }
}
export class IdbBlobStorage {
    store;
    peerShare;
    onPeerShareRevoked;
    maxBytes;
    writesSinceEviction = 0;
    evictionPromise = null;
    static EVICTION_WRITE_INTERVAL = 32;
    constructor(dbName, maxBytes, onPeerShareRevoked = () => { }) {
        this.store = new DexieStore(dbName);
        this.peerShare = new PeerShareAuthorizationDB(dbName);
        this.onPeerShareRevoked = onPeerShareRevoked;
        this.maxBytes = maxBytes;
    }
    setMaxBytes(maxBytes) {
        this.maxBytes = maxBytes;
    }
    getMaxBytes() {
        return this.maxBytes;
    }
    async put(data) {
        const hashHex = toHex(await sha256(data));
        await this.store.put(fromHex(hashHex), data);
        void this.scheduleEviction();
        return hashHex;
    }
    async putByHash(hashHex, data) {
        const computed = toHex(await sha256(data));
        if (computed !== hashHex) {
            throw new Error('Hash mismatch while caching fetched blob');
        }
        await this.store.put(fromHex(hashHex), data);
        void this.scheduleEviction();
    }
    async putByHashTrusted(hashHex, data) {
        await this.store.put(fromHex(hashHex), data);
        void this.scheduleEviction();
    }
    async get(hashHex) {
        return this.store.get(fromHex(hashHex));
    }
    async has(hashHex) {
        return this.store.has(fromHex(hashHex));
    }
    async delete(hashHex) {
        const normalized = normalizeHashHex(hashHex);
        const deleted = await this.store.delete(fromHex(normalized));
        await this.revokePeerSharing([normalized]);
        return deleted;
    }
    async authorizePeerSharing(hashHexes) {
        const authorizations = [...new Set(Array.from(hashHexes, normalizeHashHex))].map((hashHex) => ({ hashHex }));
        if (authorizations.length > 0) {
            await this.peerShare.authorizations.bulkPut(authorizations);
        }
    }
    async loadPeerShareAuthorizations() {
        const storedKeys = await this.peerShare.authorizations.toCollection().primaryKeys();
        const hashHexes = storedKeys.filter(isCanonicalHashHex);
        const malformedKeys = storedKeys.filter((hashHex) => !isCanonicalHashHex(hashHex));
        if (malformedKeys.length > 0) {
            await this.peerShare.authorizations.bulkDelete(malformedKeys);
        }
        const presence = await Promise.all(hashHexes.map(async (hashHex) => ({
            hashHex,
            present: await this.store.has(fromHex(hashHex)),
        })));
        const stale = presence.filter(({ present }) => !present).map(({ hashHex }) => hashHex);
        await this.revokePeerSharing(stale);
        return presence.filter(({ present }) => present).map(({ hashHex }) => hashHex);
    }
    async getStats() {
        const [items, bytes] = await Promise.all([
            this.store.count(),
            this.store.totalBytes(),
        ]);
        return { items, bytes, maxBytes: this.maxBytes };
    }
    close() {
        this.store.close();
        this.peerShare.close();
    }
    async revokePeerSharing(hashHexes) {
        if (hashHexes.length === 0)
            return;
        await this.peerShare.authorizations.bulkDelete([...hashHexes]);
        this.onPeerShareRevoked(hashHexes);
    }
    scheduleEviction() {
        this.writesSinceEviction += 1;
        if (this.writesSinceEviction < IdbBlobStorage.EVICTION_WRITE_INTERVAL) {
            return;
        }
        this.writesSinceEviction = 0;
        if (this.evictionPromise) {
            return;
        }
        this.evictionPromise = this.store
            .evict(this.maxBytes)
            .then(async () => {
            await this.loadPeerShareAuthorizations();
        })
            .finally(() => {
            this.evictionPromise = null;
        });
    }
}
//# sourceMappingURL=idbStorage.js.map