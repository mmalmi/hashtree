import { fromHex, type Hash, type Store } from '@hashtree/core';
import type { FipsDatagramEndpoint } from '@fips/tcp';
import {
  TCP_BLOB_DEFAULT_HTL,
  TCP_BLOB_MAX_HTL,
  TCP_BLOB_SERVICE_PORT,
  TcpBlobTransport,
} from './tcpBlobTransport.js';

export const HASHTREE_BLOB_CAPABILITY = 'hashtree.blob/1';

export interface FipsBlobRoute {
  peerId: string;
  htl: number;
  priority?: number;
}

export interface FipsCapabilityAdvertisement {
  peerId: string;
  capabilities: readonly {
    name: string;
    fspPort?: number;
    priority?: number;
  }[];
}

export type FipsBlobRouteSource = (
  () => readonly FipsBlobRoute[] | Promise<readonly FipsBlobRoute[]>
) | readonly FipsBlobRoute[];

export interface HashtreeWorkerP2PProvider {
  fetch(hashHex: string, peerId?: string, htl?: number): Promise<Uint8Array | null>;
  listPeerIds(): string[] | Promise<string[]>;
}

export type FipsWorkerNode = FipsDatagramEndpoint;

export interface FipsWorkerP2PProviderOptions {
  node: FipsWorkerNode;
  localStore: Store;
  requestTimeoutMs?: number;
  /** Authenticated capability routes or explicitly configured Hashtree peers. */
  providerRoutes?: FipsBlobRouteSource;
  /** Authorize an authenticated FIPS identity before serving local blobs. */
  allowIncomingPeer?: (peerId: string) => boolean | Promise<boolean>;
}

/**
 * Bridges a running FIPS node into HashtreeWorkerClient.setP2PProvider().
 * Provider selection comes only from the supplied route source; connected FIPS
 * peers are never inferred to be Hashtree providers.
 */
export class FipsWorkerP2PProvider implements HashtreeWorkerP2PProvider {
  readonly transport: TcpBlobTransport;
  private closed = false;

  constructor(private readonly options: FipsWorkerP2PProviderOptions) {
    this.transport = new TcpBlobTransport({
      endpoint: options.node,
      localStore: options.localStore,
      timeoutMs: options.requestTimeoutMs,
      allowIncomingPeer: options.allowIncomingPeer,
    });
  }

  fetch(hashHex: string, peerId?: string, htl?: number): Promise<Uint8Array | null> {
    if (this.closed) return Promise.reject(new Error('FIPS worker P2P provider is closed'));
    if (htl !== undefined && (!Number.isInteger(htl) || htl < 0 || htl > TCP_BLOB_MAX_HTL)) {
      throw new Error('TCP/FIPS blob HTL is invalid');
    }
    const hash = parseHash(hashHex);
    return this.fetchHash(hash, peerId, htl);
  }

  private async fetchHash(
    hash: Hash,
    peerId?: string,
    requestedHtl?: number,
  ): Promise<Uint8Array | null> {
    const routes = await this.routes();
    if (peerId) {
      const known = routes.find((route) => route.peerId === peerId);
      return this.transport.get(hash, [peerId], effectiveHtl(known, requestedHtl));
    }
    return this.fetchRoutes(hash, routes, requestedHtl);
  }

  async listPeerIds(): Promise<string[]> {
    if (this.closed) return [];
    return (await this.routes()).map((route) => route.peerId);
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    void this.transport.close();
  }

  private async routes(): Promise<FipsBlobRoute[]> {
    const source = this.options.providerRoutes;
    if (!source) return [];
    return normalizeRoutes(typeof source === 'function' ? await source() : source);
  }

  private async fetchRoutes(
    hash: Hash,
    routes: readonly FipsBlobRoute[],
    requestedHtl?: number,
  ): Promise<Uint8Array | null> {
    const groups = new Map<number, string[]>();
    for (const route of routes) {
      const htl = effectiveHtl(route, requestedHtl);
      const peers = groups.get(htl) ?? [];
      peers.push(route.peerId);
      groups.set(htl, peers);
    }
    if (groups.size === 0) return this.transport.get(hash, []);
    const attempts = [...groups].map(async ([htl, peers]) => {
      try {
        return { kind: 'result' as const, data: await this.transport.get(hash, peers, htl) };
      } catch (error) {
        return { kind: 'failed' as const, error };
      }
    });
    const pending = new Map(attempts.map((attempt, index) => [
      index,
      attempt.then((result) => [index, result] as const),
    ]));
    const failures: unknown[] = [];
    let misses = 0;
    while (pending.size > 0) {
      const [index, result] = await Promise.race(pending.values());
      pending.delete(index);
      if (result.kind === 'result' && result.data) return result.data;
      if (result.kind === 'result') misses += 1;
      else failures.push(result.error);
    }
    if (failures.length === 0 && misses === groups.size) return null;
    throw new AggregateError(failures, 'TCP/FIPS blob availability is uncertain');
  }
}

function effectiveHtl(route?: FipsBlobRoute, requestedHtl?: number): number {
  // An authenticated same-host route is terminal and must remain local-only.
  if (route?.htl === 0) return 0;
  return requestedHtl ?? route?.htl ?? TCP_BLOB_DEFAULT_HTL;
}

export function createFipsWorkerP2PProvider(
  options: FipsWorkerP2PProviderOptions,
): FipsWorkerP2PProvider {
  return new FipsWorkerP2PProvider(options);
}

/** Convert an authenticated FSP local-instance roster into local-only blob routes. */
export function blobRoutesFromCapabilityRoster(
  advertisements: readonly FipsCapabilityAdvertisement[],
): FipsBlobRoute[] {
  return normalizeRoutes(advertisements.flatMap((advertisement) => {
    const capability = advertisement.capabilities
      .filter(({ name, fspPort }) => (
        name === HASHTREE_BLOB_CAPABILITY && fspPort === TCP_BLOB_SERVICE_PORT
      ))
      .sort((left, right) => (right.priority ?? 0) - (left.priority ?? 0))[0];
    return capability
      ? [{ peerId: advertisement.peerId, htl: 0, priority: capability.priority ?? 0 }]
      : [];
  }));
}

function normalizeRoutes(routes: readonly FipsBlobRoute[]): FipsBlobRoute[] {
  const normalized = routes.map((route) => {
    const peerId = route.peerId.trim();
    const priority = route.priority ?? 0;
    if (!peerId) throw new Error('TCP/FIPS blob provider identity is empty');
    if (!Number.isInteger(route.htl) || route.htl < 0 || route.htl > TCP_BLOB_MAX_HTL) {
      throw new Error('TCP/FIPS blob HTL is invalid');
    }
    if (!Number.isInteger(priority) || priority < -0x8000 || priority > 0x7fff) {
      throw new Error('TCP/FIPS blob provider priority is invalid');
    }
    return { peerId, htl: route.htl, priority };
  }).sort((left, right) => (
    right.priority - left.priority
    || left.peerId.localeCompare(right.peerId)
    || left.htl - right.htl
  ));
  const peers = new Set<string>();
  return normalized.filter(({ peerId }) => {
    if (peers.has(peerId)) return false;
    peers.add(peerId);
    return true;
  });
}

function parseHash(hashHex: string): Hash {
  const normalized = hashHex.trim();
  if (!/^[0-9a-f]{64}$/i.test(normalized)) {
    throw new Error('Hashtree block hash must be 32-byte hex');
  }
  return fromHex(normalized) as Hash;
}
