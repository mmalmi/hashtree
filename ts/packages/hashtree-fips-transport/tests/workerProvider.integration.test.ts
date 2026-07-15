import { describe, expect, it } from 'vitest';
import { MemoryStore, sha256, toHex, type Hash } from '@hashtree/core';
import {
  FipsNode,
  identityFromSecretKey,
  toHex as fipsToHex,
} from '@fips/core';
import { createFipsWorkerP2PProvider } from '../src/workerProvider.js';
import { MemoryHub, MemoryTransport } from './support/memoryTransport.js';

describe('FIPS worker P2P provider integration', () => {
  it('fetches a block through two authenticated FIPS nodes', async () => {
    const hub = new MemoryHub();
    const sourceIdentity = await identityFromSecretKey(secret(1));
    const readerIdentity = await identityFromSecretKey(secret(2));
    const sourceNode = new FipsNode({
      identity: sourceIdentity,
      transports: [new MemoryTransport(hub)],
      routingMode: 'reply_learned',
    });
    const readerNode = new FipsNode({
      identity: readerIdentity,
      transports: [new MemoryTransport(hub)],
      routingMode: 'reply_learned',
    });
    const sourceStore = new MemoryStore();
    const readerStore = new MemoryStore();
    const sourceProvider = createFipsWorkerP2PProvider({
      node: sourceNode,
      localStore: sourceStore,
      requestTimeoutMs: 2_000,
    });
    const readerProvider = createFipsWorkerP2PProvider({
      node: readerNode,
      localStore: readerStore,
      requestTimeoutMs: 2_000,
    });

    try {
      await Promise.all([sourceNode.start(), readerNode.start()]);
      await readerNode.connect({
        transport: 'memory',
        addr: fipsToHex(sourceIdentity.publicKey),
      });
      const data = new Uint8Array(180_000);
      data.forEach((_, index) => { data[index] = index % 251; });
      const hash = await sha256(data) as Hash;
      await sourceStore.put(hash, data);

      await expect(readerProvider.listPeerIds()).resolves.toEqual([
        fipsToHex(sourceIdentity.publicKey),
      ]);
      await expect(readerProvider.fetch(toHex(hash))).resolves.toEqual(data);
      await expect(readerStore.get(hash)).resolves.toEqual(data);
    } finally {
      sourceProvider.close();
      readerProvider.close();
      await Promise.all([sourceNode.stop(), readerNode.stop()]);
    }
  });

  it('honors an explicitly selected connected FIPS peer', async () => {
    const hub = new MemoryHub();
    const sourceIdentity = await identityFromSecretKey(secret(3));
    const readerIdentity = await identityFromSecretKey(secret(4));
    const sourceNode = new FipsNode({
      identity: sourceIdentity,
      transports: [new MemoryTransport(hub)],
    });
    const readerNode = new FipsNode({
      identity: readerIdentity,
      transports: [new MemoryTransport(hub)],
    });
    const data = new TextEncoder().encode('selected FIPS peer');
    const hash = await sha256(data) as Hash;
    const sourceStore = new MemoryStore();
    await sourceStore.put(hash, data);
    const sourceProvider = createFipsWorkerP2PProvider({ node: sourceNode, localStore: sourceStore });
    const readerProvider = createFipsWorkerP2PProvider({
      node: readerNode,
      localStore: new MemoryStore(),
      requestTimeoutMs: 2_000,
    });

    try {
      await Promise.all([sourceNode.start(), readerNode.start()]);
      const sourcePeerId = fipsToHex(sourceIdentity.publicKey);
      await readerNode.connect({ transport: 'memory', addr: sourcePeerId });
      await expect(readerProvider.fetch(toHex(hash), sourcePeerId)).resolves.toEqual(data);
    } finally {
      sourceProvider.close();
      readerProvider.close();
      await Promise.all([sourceNode.stop(), readerNode.stop()]);
    }
  });

  it('rejects malformed hashes before sending FIPS data', async () => {
    const identity = await identityFromSecretKey(secret(5));
    const node = new FipsNode({ identity, transports: [] });
    const provider = createFipsWorkerP2PProvider({ node, localStore: new MemoryStore() });

    expect(() => provider.fetch('not-a-hash')).toThrow('32-byte hex');
    provider.close();
  });

  it('does not report a closed provider as an explicit content miss', async () => {
    const identity = await identityFromSecretKey(secret(6));
    const node = new FipsNode({ identity, transports: [] });
    const provider = createFipsWorkerP2PProvider({ node, localStore: new MemoryStore() });

    provider.close();
    await expect(provider.fetch('00'.repeat(32))).rejects.toThrow('closed');
  });
});

function secret(value: number): Uint8Array {
  const key = new Uint8Array(32);
  key[31] = value;
  return key;
}
