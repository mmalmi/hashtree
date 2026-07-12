import { fromHex, type Hash, type Store } from '@hashtree/core';
import {
  HashtreeFipsTransport,
  createFipsNodeEndpoint,
  type FipsEndpoint,
  type FipsNodeLike,
  type HashtreeFipsTransportOptions,
} from './index.js';

export interface HashtreeWorkerP2PProvider {
  fetch(hashHex: string, peerId?: string): Promise<Uint8Array | null>;
  listPeerIds(): string[] | Promise<string[]>;
}

export interface FipsWorkerP2PProviderOptions extends Omit<
  HashtreeFipsTransportOptions,
  'endpoint' | 'localStore' | 'peers'
> {
  node: FipsNodeLike;
  localStore: Store;
}

/**
 * Bridges a running FIPS node into HashtreeWorkerClient.setP2PProvider().
 * Peer selection remains dynamic: the provider reads authenticated FIPS links
 * for every request instead of maintaining a second discovery mesh.
 */
export class FipsWorkerP2PProvider implements HashtreeWorkerP2PProvider {
  readonly endpoint: FipsEndpoint;
  readonly transport: HashtreeFipsTransport;
  private closed = false;

  constructor(options: FipsWorkerP2PProviderOptions) {
    const { node, ...transportOptions } = options;
    this.endpoint = createFipsNodeEndpoint(node);
    this.transport = new HashtreeFipsTransport({
      ...transportOptions,
      endpoint: this.endpoint,
      peers: () => this.endpoint.listPeerIds?.() ?? [],
    });
  }

  fetch(hashHex: string, peerId?: string): Promise<Uint8Array | null> {
    if (this.closed) return Promise.resolve(null);
    const hash = parseHash(hashHex);
    return this.transport.get(hash, peerId ? [peerId] : undefined);
  }

  async listPeerIds(): Promise<string[]> {
    if (this.closed) return [];
    return [...await Promise.resolve(this.endpoint.listPeerIds?.() ?? [])];
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.transport.close();
  }
}

export function createFipsWorkerP2PProvider(
  options: FipsWorkerP2PProviderOptions,
): FipsWorkerP2PProvider {
  return new FipsWorkerP2PProvider(options);
}

function parseHash(hashHex: string): Hash {
  const normalized = hashHex.trim();
  if (!/^[0-9a-f]{64}$/i.test(normalized)) {
    throw new Error('Hashtree block hash must be 32-byte hex');
  }
  return fromHex(normalized) as Hash;
}
