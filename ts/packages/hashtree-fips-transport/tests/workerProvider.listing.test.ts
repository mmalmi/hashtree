import { describe, expect, it, vi } from 'vitest';
import { MemoryStore, sha256, toHex, type Hash } from '@hashtree/core';
import { FipsNode, identityFromSecretKey, toHex as fipsToHex } from '@fips/core';
import { createFipsWorkerP2PProvider, type FipsBlobRoute } from '../src/workerProvider.js';
import { MemoryHub, MemoryTransport } from './support/memoryTransport.js';

function secret(value: number): Uint8Array {
  const key = new Uint8Array(32);
  key[31] = value;
  return key;
}

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>(done => { resolve = done; });
  return { promise, resolve };
}

describe('known FIPS provider listing', () => {
  it('lists a usable explicit route while another real capability probe is held', async () => {
    const hub = new MemoryHub();
    const nodes = await Promise.all([31, 32, 33].map(async (seed) => new FipsNode({
      identity: await identityFromSecretKey(secret(seed)),
      transports: [new MemoryTransport(hub)], routingMode: 'reply_learned',
    })));
    const [source, candidate, reader] = nodes;
    const [sourceId, candidateId] = nodes.map(node => fipsToHex(node.identity.publicKey));
    const store = new MemoryStore();
    const sourceProvider = createFipsWorkerP2PProvider({ node: source!, localStore: store });
    const candidateProvider = createFipsWorkerP2PProvider({ node: candidate!, localStore: new MemoryStore() });
    const held = deferred();
    const entered = deferred();
    const candidates: string[] = [];
    let blocked = true;
    const provider = createFipsWorkerP2PProvider({
      node: {
        registerService: reader!.registerService.bind(reader),
        sendDatagram: async args => {
          if (args.dst === candidateId && blocked) {
            entered.resolve();
            await held.promise;
          }
          await reader!.sendDatagram(args);
        },
      },
      localStore: new MemoryStore(), requestTimeoutMs: 2_000,
      providerRoutes: [{ peerId: sourceId!, htl: 10, priority: 1 }],
      candidatePeerIds: () => candidates,
    });
    let discovery: Promise<void> | undefined;
    let listing: Promise<void> | undefined;
    try {
      await Promise.all(nodes.map(node => node.start()));
      await reader!.connect({ transport: 'memory', addr: sourceId! });
      await reader!.connect({ transport: 'memory', addr: candidateId! });
      const bytes = new TextEncoder().encode('known route is already usable');
      const hash = await sha256(bytes) as Hash;
      await store.put(hash, bytes);
      await expect(provider.fetch(toHex(hash))).resolves.toEqual(bytes);

      candidates.push(candidateId!);
      discovery = provider.discoverProviders();
      await entered.promise;
      let listed: string[] | undefined;
      listing = provider.listPeerIds().then(ids => { listed = ids; });
      await expect.poll(() => listed, {
        timeout: 200, interval: 5,
        message: 'known explicit route must list while the candidate probe is held',
      }).toEqual([sourceId]);

      blocked = false;
      held.resolve();
      await discovery;
      await expect(provider.listPeerIds()).resolves.toEqual([sourceId, candidateId]);
    } finally {
      blocked = false;
      held.resolve();
      let cleanupTimer: ReturnType<typeof setTimeout> | undefined;
      try {
        await Promise.race([
          Promise.allSettled([discovery, listing]),
          new Promise<never>((_, reject) => {
            cleanupTimer = setTimeout(() => reject(new Error('provider probe cleanup did not settle')), 1_000);
          }),
        ]);
      } finally {
        clearTimeout(cleanupTimer);
        sourceProvider.close();
        candidateProvider.close();
        provider.close();
        await Promise.all(nodes.map(node => node.stop()));
      }
    }
  });

  it('keeps normalization, dynamic route sources and closed behavior', async () => {
    const node = new FipsNode({ identity: await identityFromSecretKey(secret(34)), transports: [] });
    let routes: FipsBlobRoute[] = [
      { peerId: ' lower ', htl: 10 },
      { peerId: 'preferred', htl: 0, priority: 5 },
      { peerId: 'preferred', htl: 10 },
    ];
    const provider = createFipsWorkerP2PProvider({
      node, localStore: new MemoryStore(), providerRoutes: () => routes,
      candidatePeerIds: () => ['unqualified'],
    });
    const probe = vi.spyOn(provider.transport, 'probe').mockResolvedValue(false);
    try {
      await expect(provider.listPeerIds()).resolves.toEqual(['preferred', 'lower']);
      expect(probe).not.toHaveBeenCalled();
      routes = [{ peerId: 'replacement', htl: 0 }];
      await expect(provider.listPeerIds()).resolves.toEqual(['replacement']);
      provider.close();
      await expect(provider.listPeerIds()).resolves.toEqual([]);
    } finally { provider.close(); }
  });

  it('preserves route-source and validation errors before listing', async () => {
    const node = new FipsNode({ identity: await identityFromSecretKey(secret(35)), transports: [] });
    const failure = new Error('route source failed');
    let reject = true;
    const provider = createFipsWorkerP2PProvider({
      node, localStore: new MemoryStore(),
      providerRoutes: async () => {
        if (reject) throw failure;
        return [{ peerId: '', htl: 0 }];
      },
    });
    try {
      await expect(provider.listPeerIds()).rejects.toBe(failure);
      reject = false;
      await expect(provider.listPeerIds()).rejects.toThrow('identity is empty');
    } finally { provider.close(); }
  });
});
