import { fromHex, sha256, toHex } from '@hashtree/core';
import { DexieStore } from '@hashtree/dexie';
export class IdbBlobStorage {
    store;
    maxBytes;
    writesSinceEviction = 0;
    evictionPromise = null;
    static EVICTION_WRITE_INTERVAL = 32;
    constructor(dbName, maxBytes) {
        this.store = new DexieStore(dbName);
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
        return this.store.delete(fromHex(hashHex));
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
            .then(() => { })
            .finally(() => {
            this.evictionPromise = null;
        });
    }
}
//# sourceMappingURL=idbStorage.js.map