import { afterEach, describe, expect, it, vi } from 'vitest';
import { MemoryStore, sha256 } from '@hashtree/core';
import { createRequest, hashToKey, type PeerHTLConfig } from '@hashtree/nostr';
import { MeshQueryRouter } from '../src/p2p/meshQueryRouter.js';

type RequestMessage = {
  type: 'request';
  from: string;
  to: string;
  hash: Uint8Array;
  htl: number;
};

type ResponseMessage = {
  type: 'response';
  from: string;
  to: string;
  hash: Uint8Array;
  data: Uint8Array;
};

type SimMessage = RequestMessage | ResponseMessage;

interface NodeStats {
  requestsSent: number;
  requestsReceived: number;
  responsesSent: number;
  responsesReceived: number;
  suppressed: number;
  resolved: number;
  upstreamFetches: number;
}

interface SimNode {
  id: string;
  neighbors: string[];
  localStore: MemoryStore;
  router: MeshQueryRouter;
  stats: NodeStats;
}

const DETERMINISTIC_HTL: PeerHTLConfig = {
  atMaxSample: 0,
  atMinSample: 0,
};

const CLIENT_PREFIX = 'client:';

function clientPeerId(nodeId: string): string {
  return `${CLIENT_PREFIX}${nodeId}`;
}

class MeshQuerySimulation {
  private readonly nodes = new Map<string, SimNode>();
  private readonly queue: SimMessage[] = [];
  private readonly resolvedClients = new Set<string>();

  async addNode(
    id: string,
    neighbors: string[],
    options: {
      localPayloads?: Uint8Array[];
      upstreamPayloads?: Uint8Array[];
      upstreamDelayMs?: number;
      maxForwardsPerPeerWindow?: number;
      forwardRateLimitWindowMs?: number;
    } = {},
  ): Promise<void> {
    const localStore = new MemoryStore();
    for (const payload of options.localPayloads ?? []) {
      const hash = await sha256(payload);
      await localStore.put(hash, payload);
    }

    const stats: NodeStats = {
      requestsSent: 0,
      requestsReceived: 0,
      responsesSent: 0,
      responsesReceived: 0,
      suppressed: 0,
      resolved: 0,
      upstreamFetches: 0,
    };
    const upstreamByHash = new Map<string, Uint8Array>();
    for (const payload of options.upstreamPayloads ?? []) {
      const hash = await sha256(payload);
      upstreamByHash.set(hashToKey(hash), payload);
    }

    const router = new MeshQueryRouter({
      localStore,
      requestTimeoutMs: 200,
      upstreamFetch: upstreamByHash.size > 0
        ? async (hash) => {
            stats.upstreamFetches += 1;
            return new Promise<Uint8Array | null>((resolve) => {
              setTimeout(() => {
                resolve(upstreamByHash.get(hashToKey(hash)) ?? null);
              }, options.upstreamDelayMs ?? 0);
            });
          }
        : undefined,
      maxForwardsPerPeerWindow: options.maxForwardsPerPeerWindow ?? 1000,
      forwardRateLimitWindowMs: options.forwardRateLimitWindowMs ?? 1000,
    });

    this.nodes.set(id, {
      id,
      neighbors: [...neighbors],
      localStore,
      router,
      stats,
    });
  }

  connect(): void {
    for (const node of this.nodes.values()) {
      node.router.registerPeer({
        peerId: clientPeerId(node.id),
        canSend: () => false,
        getHtlConfig: () => DETERMINISTIC_HTL,
        sendRequest: () => false,
        sendResponse: async (hash) => {
          node.stats.resolved += 1;
          this.resolvedClients.add(`${node.id}:${hashToKey(hash)}`);
        },
        onForwardedSuppressed: () => {
          node.stats.suppressed += 1;
        },
      });

      for (const neighborId of node.neighbors) {
        node.router.registerPeer({
          peerId: neighborId,
          canSend: () => true,
          getHtlConfig: () => DETERMINISTIC_HTL,
          sendRequest: (hash, htl) => {
            node.stats.requestsSent += 1;
            this.queue.push({ type: 'request', from: node.id, to: neighborId, hash, htl });
            return true;
          },
          sendResponse: async (hash, data) => {
            node.stats.responsesSent += 1;
            this.queue.push({ type: 'response', from: node.id, to: neighborId, hash, data });
          },
          onForwardedSuppressed: () => {
            node.stats.suppressed += 1;
          },
        });
      }
    }
  }

  async startLookup(originId: string, hash: Uint8Array, htl: number): Promise<void> {
    const origin = this.nodes.get(originId);
    if (!origin) {
      throw new Error(`Unknown origin node ${originId}`);
    }
    await origin.router.handleRequest(clientPeerId(originId), createRequest(hash, htl));
  }

  async runUntilIdle(maxSteps = 400): Promise<{ requests: number; responses: number }> {
    let steps = 0;
    let idleTicks = 0;

    while (steps < maxSteps) {
      const next = this.queue.shift();
      if (next) {
        idleTicks = 0;
        if (next.type === 'request') {
          const node = this.nodes.get(next.to);
          if (!node) {
            throw new Error(`Unknown request target ${next.to}`);
          }
          node.stats.requestsReceived += 1;
          await node.router.handleRequest(next.from, createRequest(next.hash, next.htl));
        } else {
          const node = this.nodes.get(next.to);
          if (!node) {
            throw new Error(`Unknown response target ${next.to}`);
          }
          node.stats.responsesReceived += 1;
          await node.router.resolve(next.hash, next.data);
        }
        steps += 1;
        continue;
      }

      await vi.advanceTimersByTimeAsync(5);
      await Promise.resolve();
      idleTicks += 1;
      steps += 1;
      if (idleTicks >= 10 && this.queue.length === 0) {
        break;
      }
    }

    if (steps >= maxSteps) {
      throw new Error(`Simulation exceeded ${maxSteps} steps`);
    }

    let requests = 0;
    let responses = 0;
    for (const node of this.nodes.values()) {
      requests += node.stats.requestsSent;
      responses += node.stats.responsesSent;
    }
    return { requests, responses };
  }

  wasResolved(originId: string, hash: Uint8Array): boolean {
    return this.resolvedClients.has(`${originId}:${hashToKey(hash)}`);
  }

  statsFor(nodeId: string): NodeStats {
    const node = this.nodes.get(nodeId);
    if (!node) {
      throw new Error(`Unknown node ${nodeId}`);
    }
    return node.stats;
  }

  totalSuppressed(): number {
    let total = 0;
    for (const node of this.nodes.values()) {
      total += node.stats.suppressed;
    }
    return total;
  }

  stop(): void {
    for (const node of this.nodes.values()) {
      node.router.stop();
    }
  }
}

describe('MeshQueryRouter multi-peer simulation', () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it('deduplicates a delayed upstream fetch across a larger mesh and resolves multiple clients', async () => {
    vi.useFakeTimers();
    const payload = new TextEncoder().encode('mesh-upstream-payload');
    const hash = await sha256(payload);

    const simulation = new MeshQuerySimulation();
    await simulation.addNode('A', ['B']);
    await simulation.addNode('B', ['A', 'C']);
    await simulation.addNode('C', ['B', 'D']);
    await simulation.addNode('D', ['C', 'E']);
    await simulation.addNode('E', ['D', 'F']);
    await simulation.addNode('F', ['E', 'G']);
    await simulation.addNode('G', ['F'], {
      upstreamPayloads: [payload],
      upstreamDelayMs: 25,
    });
    simulation.connect();

    await simulation.startLookup('A', hash, 5);
    await simulation.startLookup('C', hash, 5);
    const summary = await simulation.runUntilIdle();
    simulation.stop();

    expect(simulation.wasResolved('A', hash)).toBe(true);
    expect(simulation.wasResolved('C', hash)).toBe(true);
    expect(simulation.statsFor('G').upstreamFetches).toBe(1);
    expect(summary.requests).toBeLessThanOrEqual(12);
    expect(summary.responses).toBeGreaterThanOrEqual(7);
    expect(simulation.totalSuppressed()).toBeGreaterThan(0);
  });

  it('keeps a miss bounded by HTL across a denser topology', async () => {
    vi.useFakeTimers();
    const missingHash = await sha256(new TextEncoder().encode('mesh-miss'));

    const simulation = new MeshQuerySimulation();
    await simulation.addNode('A', ['B', 'C']);
    await simulation.addNode('B', ['A', 'D']);
    await simulation.addNode('C', ['A', 'D', 'E']);
    await simulation.addNode('D', ['B', 'C', 'F']);
    await simulation.addNode('E', ['C', 'F']);
    await simulation.addNode('F', ['D', 'E']);
    simulation.connect();

    await simulation.startLookup('A', missingHash, 2);
    const summary = await simulation.runUntilIdle();
    simulation.stop();

    expect(simulation.wasResolved('A', missingHash)).toBe(false);
    expect(summary.responses).toBe(0);
    expect(summary.requests).toBeLessThanOrEqual(6);
    expect(simulation.statsFor('F').requestsReceived).toBe(0);
  });

  it('does not let the forward limiter block upstream-backed misses from a provider peer', async () => {
    vi.useFakeTimers();
    const payloadA = new TextEncoder().encode('upstream-a');
    const payloadB = new TextEncoder().encode('upstream-b');
    const hashA = await sha256(payloadA);
    const hashB = await sha256(payloadB);

    const simulation = new MeshQuerySimulation();
    await simulation.addNode('A', ['B']);
    await simulation.addNode('B', ['A'], {
      upstreamPayloads: [payloadA, payloadB],
      upstreamDelayMs: 10,
      maxForwardsPerPeerWindow: 1,
      forwardRateLimitWindowMs: 10_000,
    });
    simulation.connect();

    await simulation.startLookup('A', hashA, 3);
    await simulation.startLookup('A', hashB, 3);
    const summary = await simulation.runUntilIdle();
    simulation.stop();

    expect(simulation.wasResolved('A', hashA)).toBe(true);
    expect(simulation.wasResolved('A', hashB)).toBe(true);
    expect(simulation.statsFor('B').upstreamFetches).toBe(2);
    expect(summary.responses).toBeGreaterThanOrEqual(2);
  });
});
