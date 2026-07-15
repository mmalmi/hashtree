import { fromHex, type Hash, type Store } from '@hashtree/core';
import type { FipsDatagramEndpoint } from '@fips/tcp';
import { TcpBlobTransport } from './tcpBlobTransport.js';

export interface HashtreeWorkerP2PProvider {
  fetch(hashHex: string, peerId?: string): Promise<Uint8Array | null>;
  listPeerIds(): string[] | Promise<string[]>;
}

export interface FipsWorkerNode extends FipsDatagramEndpoint {
  on(event: 'peer', handler: (value: unknown) => void): () => void;
}

export interface FipsWorkerP2PProviderOptions {
  node: FipsWorkerNode;
  localStore: Store;
  requestTimeoutMs?: number;
}

/**
 * Bridges a running FIPS node into HashtreeWorkerClient.setP2PProvider().
 * Peer selection remains dynamic: the provider reads authenticated FIPS links
 * for every request instead of maintaining a second discovery mesh.
 */
export class FipsWorkerP2PProvider implements HashtreeWorkerP2PProvider {
  readonly transport: TcpBlobTransport;
  private readonly node: FipsWorkerNode;
  private readonly peers = new Set<string>();
  private readonly unsubscribePeer: () => void;
  private closed = false;

  constructor(options: FipsWorkerP2PProviderOptions) {
    this.node = options.node;
    this.unsubscribePeer = this.node.on('peer', (value) => {
      const event = value as { remotePubkey: string; state: 'connected' | 'disconnected' };
      if (event.state === 'connected') this.peers.add(event.remotePubkey);
      else this.peers.delete(event.remotePubkey);
    });
    this.transport = new TcpBlobTransport({
      endpoint: options.node,
      localStore: options.localStore,
      timeoutMs: options.requestTimeoutMs,
    });
  }

  fetch(hashHex: string, peerId?: string): Promise<Uint8Array | null> {
    if (this.closed) return Promise.resolve(null);
    const hash = parseHash(hashHex);
    return this.transport.get(hash, peerId ? [peerId] : [...this.peers]);
  }

  async listPeerIds(): Promise<string[]> {
    if (this.closed) return [];
    return [...this.peers];
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.unsubscribePeer();
    void this.transport.close();
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
