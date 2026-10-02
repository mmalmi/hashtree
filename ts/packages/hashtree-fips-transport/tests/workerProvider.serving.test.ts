import { describe, expect, it, vi } from 'vitest';
import { MemoryStore, sha256, toHex } from '@hashtree/core';
import { FipsNode, identityFromSecretKey } from '@fips/core';
import { createFipsWorkerP2PProvider } from '../src/workerProvider.js';
import type { TcpBlobTransportOptions } from '../src/tcpBlobTransport.js';
import { MemoryHub, MemoryTransport } from './support/memoryTransport.js';

async function pair(
  serveBlob: NonNullable<TcpBlobTransportOptions['serveBlob']>,
  allowed = true,
  getUploadLimitBytesPerSecond?: () => number | null,
  requestTimeoutMs = 1_000,
) {
  const hub = new MemoryHub();
  const nodes = await Promise.all([61, 62].map(async (seed) => new FipsNode({
    identity: await identityFromSecretKey(new Uint8Array(32).fill(seed)),
    transports: [new MemoryTransport(hub)], routingMode: 'reply_learned',
  })));
  const ids = nodes.map((node) => toHex(node.identity.publicKey));
  const stores = nodes.map(() => new MemoryStore());
  const outgoingServe = vi.fn(async () => { throw new Error('outgoing read called serving hook'); });
  const providers = [
    createFipsWorkerP2PProvider({ node: nodes[0], localStore: stores[0], serveBlob, getUploadLimitBytesPerSecond,
      allowIncomingPeer: (peer) => allowed && peer === ids[1], requestTimeoutMs }),
    createFipsWorkerP2PProvider({ node: nodes[1], localStore: stores[1], serveBlob: outgoingServe,
      providerRoutes: [{ peerId: ids[0], htl: 10 }], requestTimeoutMs }),
  ];
  const close = async () => {
    providers.forEach((provider) => provider.close());
    await Promise.all(nodes.map((node) => node.stop()));
  };
  try {
    await Promise.all(nodes.map((node) => node.start()));
    await nodes[1].connect({ transport: 'memory', addr: ids[0] });
    return { providers, stores, ids, outgoingServe, close };
  } catch (error) { await close(); throw error; }
}

describe('authenticated inbound blob serving', () => {
  it('serves uncached bytes only on the incoming path with the authenticated peer identity', async () => {
    const data = new TextEncoder().encode('upstream content behind the authenticated provider');
    const hash = await sha256(data);
    const serve = vi.fn(async () => data);
    const fixture = await pair(serve);
    try {
      await expect(fixture.stores[0].get(hash)).resolves.toBeNull();
      await expect(fixture.providers[1].fetch(toHex(hash))).resolves.toEqual(data);
      expect(serve).toHaveBeenCalledWith(hash, fixture.ids[1], expect.any(AbortSignal), 10);
      expect(fixture.outgoingServe).not.toHaveBeenCalled();
      await expect(fixture.stores[1].get(hash)).resolves.toEqual(data);
    } finally { await fixture.close(); }
  });

  it('does not call the serving callback before peer admission', async () => {
    const serve = vi.fn(async () => new Uint8Array([1]));
    const fixture = await pair(serve, false);
    try {
      await expect(fixture.providers[1].fetch(toHex(await sha256(new Uint8Array([1])))))
        .rejects.toThrow('uncertain');
      expect(serve).not.toHaveBeenCalled();
    } finally { await fixture.close(); }
  });

  it('keeps an explicit serving miss even if the raw local store has the block', async () => {
    const data = new Uint8Array([2]);
    const hash = await sha256(data);
    const serve = vi.fn(async () => null);
    const fixture = await pair(serve);
    try {
      await fixture.stores[0].put(hash, data);
      await expect(fixture.providers[1].fetch(toHex(hash))).resolves.toBeNull();
      expect(serve).toHaveBeenCalled();
    } finally { await fixture.close(); }
  });

  it.each(['failure', 'corrupt'] as const)('does not convert a serving %s into a miss or valid reply', async (kind) => {
    const data = new Uint8Array([3]);
    const hash = await sha256(data);
    const serve = vi.fn(async () => {
      if (kind === 'failure') throw new Error('upstream is incomplete');
      return new Uint8Array([4]);
    });
    const fixture = await pair(serve);
    try {
      await expect(fixture.providers[1].fetch(toHex(hash))).rejects.toThrow('uncertain');
      expect(serve).toHaveBeenCalled();
      await expect(fixture.stores[1].get(hash)).resolves.toBeNull();
    } finally { await fixture.close(); }
  });

  it('bounds a late serving callback by the original deadline and ignores its late bytes', async () => {
    const data = new Uint8Array([5]);
    const hash = await sha256(data);
    const signals: AbortSignal[] = [];
    const releases: Array<(data: Uint8Array) => void> = [];
    const serve = vi.fn((_hash, _peer, signal: AbortSignal) => {
      signals.push(signal);
      return new Promise<Uint8Array>((resolve) => releases.push(resolve));
    });
    const fixture = await pair(serve);
    try {
      await expect(fixture.providers[1].fetch(toHex(hash))).rejects.toThrow('uncertain');
      await vi.waitFor(() => expect(signals.length > 0 && signals.every((signal) => signal?.aborted)).toBe(true));
      releases.forEach((release) => release(data));
      await expect(fixture.stores[1].get(hash)).resolves.toBeNull();
    } finally {
      releases.forEach((release) => release(data));
      await fixture.close();
    }
  });

  it('aborts the serving callback when the transport closes', async () => {
    const data = new Uint8Array([6]);
    let signal: AbortSignal | undefined;
    let release: ((data: Uint8Array) => void) | undefined;
    const fixture = await pair((_hash, _peer, incomingSignal) => {
      signal = incomingSignal;
      return new Promise<Uint8Array>((resolve) => { release = resolve; });
    });
    const pending = fixture.providers[1].fetch(toHex(await sha256(data)));
    const failed = expect(pending).rejects.toThrow('uncertain');
    try {
      await vi.waitFor(() => expect(release).toBeTypeOf('function'));
      fixture.providers[0].close();
      expect(signal?.aborted).toBe(true);
    } finally {
      release?.(data);
      await failed;
      await fixture.close();
    }
  });

  it('paces a real authenticated response larger than the upload bucket', async () => {
    const data = new Uint8Array(1536).fill(7);
    const limit = vi.fn(() => 1024);
    const fixture = await pair(async () => data, true, limit, 4_000);
    try {
      const started = performance.now();
      await expect(fixture.providers[1].fetch(toHex(await sha256(data)))).resolves.toEqual(data);
      expect(performance.now() - started).toBeGreaterThanOrEqual(500);
      expect(limit).toHaveBeenCalled();
      expect(fixture.outgoingServe).not.toHaveBeenCalled();
    } finally { await fixture.close(); }
  });

  it('passes the unchanged local-only HTL to the authorized serving callback', async () => {
    const hash = await sha256(new Uint8Array([8]));
    const serve = vi.fn(async () => null);
    const fixture = await pair(serve);
    try {
      await expect(fixture.providers[1].fetch(toHex(hash), undefined, 0)).resolves.toBeNull();
      expect(serve).toHaveBeenCalledWith(hash, fixture.ids[1], expect.any(AbortSignal), 0);
    } finally { await fixture.close(); }
  });

});
