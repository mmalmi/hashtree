import 'fake-indexeddb/auto';

import Dexie from 'dexie';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { IdbBlobStorage } from '../src/capabilities/idbStorage.js';

const databaseNames = new Set<string>();
let databaseCounter = 0;

function nextDatabaseName(): string {
  databaseCounter += 1;
  const name = `hashtree-worker-peer-share-${databaseCounter}`;
  databaseNames.add(name);
  return name;
}

function openPeerShareMetadata(databaseName: string): Dexie {
  const database = new Dexie(`${databaseName}-peer-share`);
  database.version(1).stores({ authorizations: '&hashHex' });
  return database;
}

afterEach(async () => {
  await Promise.all(Array.from(databaseNames, async (name) => {
    await Dexie.delete(name);
    await Dexie.delete(`${name}-peer-share`);
  }));
  databaseNames.clear();
});

describe('IdbBlobStorage peer-share authorization', () => {
  it('restores only explicitly shared blobs after reopening the store', async () => {
    const databaseName = nextDatabaseName();
    let storage = new IdbBlobStorage(databaseName, 1_000_000);
    const sharedHash = await storage.put(new Uint8Array([1, 2, 3]));
    await storage.put(new Uint8Array([4, 5, 6]));
    await storage.authorizePeerSharing([sharedHash]);
    storage.close();

    storage = new IdbBlobStorage(databaseName, 1_000_000);
    expect(await storage.loadPeerShareAuthorizations()).toEqual([sharedHash]);
    storage.close();
  });

  it('revokes authorization when its blob is deleted', async () => {
    const databaseName = nextDatabaseName();
    const onRevoked = vi.fn();
    let storage = new IdbBlobStorage(databaseName, 1_000_000, onRevoked);
    const hashHex = await storage.put(new Uint8Array([7, 8, 9]));
    await storage.authorizePeerSharing([hashHex]);

    expect(await storage.delete(hashHex)).toBe(true);
    expect(onRevoked).toHaveBeenCalledWith([hashHex]);
    storage.close();

    storage = new IdbBlobStorage(databaseName, 1_000_000);
    expect(await storage.loadPeerShareAuthorizations()).toEqual([]);
    storage.close();
  });

  it('prunes malformed and stale metadata without blocking valid authorization', async () => {
    const databaseName = nextDatabaseName();
    let storage = new IdbBlobStorage(databaseName, 1_000_000);
    const sharedHash = await storage.put(new Uint8Array([10, 11, 12]));
    await storage.authorizePeerSharing([sharedHash]);
    storage.close();

    const staleHash = 'ff'.repeat(32);
    const metadata = openPeerShareMetadata(databaseName);
    await metadata.table('authorizations').bulkPut([
      { hashHex: 'not-a-hash' },
      { hashHex: staleHash },
    ]);
    metadata.close();

    const onRevoked = vi.fn();
    storage = new IdbBlobStorage(databaseName, 1_000_000, onRevoked);
    expect(await storage.loadPeerShareAuthorizations()).toEqual([sharedHash]);
    expect(onRevoked).toHaveBeenCalledWith([staleHash]);

    const inspector = openPeerShareMetadata(databaseName);
    expect(await inspector.table('authorizations').toCollection().primaryKeys()).toEqual([sharedHash]);
    inspector.close();
    storage.close();
  });

  it('revokes authorization when bounded storage evicts its blob', async () => {
    const databaseName = nextDatabaseName();
    const onRevoked = vi.fn();
    const storage = new IdbBlobStorage(databaseName, 0, onRevoked);
    const sharedHash = await storage.put(new Uint8Array([13]));
    await storage.authorizePeerSharing([sharedHash]);

    for (let value = 14; value < 45; value += 1) {
      await storage.put(new Uint8Array([value]));
    }

    await vi.waitFor(() => {
      expect(onRevoked).toHaveBeenCalledWith(expect.arrayContaining([sharedHash]));
    });
    expect(await storage.loadPeerShareAuthorizations()).not.toContain(sharedHash);
    storage.close();
  });
});
