import { fromHex, sha256, toHex } from '@hashtree/core';
import { DexieStore } from '@hashtree/dexie';
import Dexie, { type Table } from 'dexie';

export interface StorageStats {
  items: number;
  bytes: number;
  maxBytes: number;
}

interface PeerShareAuthorization {
  hashHex: string;
}

type PeerShareRevocationHandler = (hashHexes: readonly string[]) => void;

const HASH_HEX_PATTERN = /^[0-9a-f]{64}$/;

function normalizeHashHex(hashHex: string): string {
  const normalized = hashHex.toLowerCase();
  if (!HASH_HEX_PATTERN.test(normalized)) {
    throw new Error('Invalid blob hash');
  }
  return normalized;
}

function isCanonicalHashHex(value: unknown): value is string {
  return typeof value === 'string' && HASH_HEX_PATTERN.test(value);
}

class PeerShareAuthorizationDB extends Dexie {
  authorizations!: Table<PeerShareAuthorization, string>;

  constructor(dbName: string) {
    super(`${dbName}-peer-share`);
    this.version(1).stores({ authorizations: '&hashHex' });
  }
}

export class IdbBlobStorage {
  private readonly store: DexieStore;
  private readonly peerShare: PeerShareAuthorizationDB;
  private readonly onPeerShareRevoked: PeerShareRevocationHandler;
  private maxBytes: number;
  private writesSinceEviction = 0;
  private evictionPromise: Promise<void> | null = null;

  private static readonly EVICTION_WRITE_INTERVAL = 32;

  constructor(
    dbName: string,
    maxBytes: number,
    onPeerShareRevoked: PeerShareRevocationHandler = () => {},
  ) {
    this.store = new DexieStore(dbName);
    this.peerShare = new PeerShareAuthorizationDB(dbName);
    this.onPeerShareRevoked = onPeerShareRevoked;
    this.maxBytes = maxBytes;
  }

  setMaxBytes(maxBytes: number): void {
    this.maxBytes = maxBytes;
  }

  getMaxBytes(): number {
    return this.maxBytes;
  }

  async put(data: Uint8Array): Promise<string> {
    const hashHex = toHex(await sha256(data));
    await this.store.put(fromHex(hashHex), data);
    void this.scheduleEviction();
    return hashHex;
  }

  async putByHash(hashHex: string, data: Uint8Array): Promise<void> {
    const computed = toHex(await sha256(data));
    if (computed !== hashHex) {
      throw new Error('Hash mismatch while caching fetched blob');
    }
    await this.store.put(fromHex(hashHex), data);
    void this.scheduleEviction();
  }

  async putByHashTrusted(hashHex: string, data: Uint8Array): Promise<void> {
    await this.store.put(fromHex(hashHex), data);
    void this.scheduleEviction();
  }

  async get(hashHex: string): Promise<Uint8Array | null> {
    return this.store.get(fromHex(hashHex));
  }

  async has(hashHex: string): Promise<boolean> {
    return this.store.has(fromHex(hashHex));
  }

  async delete(hashHex: string): Promise<boolean> {
    const normalized = normalizeHashHex(hashHex);
    const deleted = await this.store.delete(fromHex(normalized));
    await this.revokePeerSharing([normalized]);
    return deleted;
  }

  async authorizePeerSharing(hashHexes: Iterable<string>): Promise<void> {
    const authorizations = [...new Set(
      Array.from(hashHexes, normalizeHashHex),
    )].map((hashHex) => ({ hashHex }));
    if (authorizations.length > 0) {
      await this.peerShare.authorizations.bulkPut(authorizations);
    }
  }

  async loadPeerShareAuthorizations(): Promise<string[]> {
    const storedKeys: unknown[] = await this.peerShare.authorizations.toCollection().primaryKeys();
    const hashHexes = storedKeys.filter(isCanonicalHashHex);
    const malformedKeys = storedKeys.filter((hashHex) => !isCanonicalHashHex(hashHex));
    if (malformedKeys.length > 0) {
      await this.peerShare.authorizations.bulkDelete(malformedKeys as string[]);
    }

    const presence = await Promise.all(hashHexes.map(async (hashHex) => ({
      hashHex,
      present: await this.store.has(fromHex(hashHex)),
    })));
    const stale = presence.filter(({ present }) => !present).map(({ hashHex }) => hashHex);
    await this.revokePeerSharing(stale);
    return presence.filter(({ present }) => present).map(({ hashHex }) => hashHex);
  }

  async getStats(): Promise<StorageStats> {
    const [items, bytes] = await Promise.all([
      this.store.count(),
      this.store.totalBytes(),
    ]);
    return { items, bytes, maxBytes: this.maxBytes };
  }

  close(): void {
    this.store.close();
    this.peerShare.close();
  }

  private async revokePeerSharing(hashHexes: readonly string[]): Promise<void> {
    if (hashHexes.length === 0) return;
    await this.peerShare.authorizations.bulkDelete([...hashHexes]);
    this.onPeerShareRevoked(hashHexes);
  }

  private scheduleEviction(): void {
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
