import {
  BLOB_NO_RESULT,
  type BlobRequest,
  type BlobRoute,
  type BlobRouteContext,
} from '@hashtree/core';
import { BlobRouter } from '@hashtree/mesh';
import type { P2PBridge } from './p2pBridge.js';

const DEFAULT_PEER_LIST_CACHE_MS = 1_500;
const MAX_P2P_PEERS = 32;
const P2P_ROUTE_TIMEOUT_MS = 20_000;

/** One composite route over the exact identities advertised by its configured provider. */
export class P2PPeerRoutes implements BlobRoute {
  readonly id = 'p2p';
  private peerIds: string[] = [];
  private peerRoutes = new Map<string, BlobRoute>();
  private readonly router: BlobRouter;
  private refreshedAt = 0;
  private inflight: Promise<string[]> | null = null;
  private generation = 0;

  constructor(
    private readonly bridge: P2PBridge,
    private readonly cacheMs = DEFAULT_PEER_LIST_CACHE_MS,
  ) {
    this.router = new BlobRouter([], {
      id: 'p2p-peers',
      requestTimeoutMs: P2P_ROUTE_TIMEOUT_MS,
      maxRoutes: MAX_P2P_PEERS,
      maxRouteAttempts: MAX_P2P_PEERS,
    });
  }

  isAvailable = (): boolean => this.bridge.isEnabled();

  setEnabled(enabled: boolean): void {
    this.generation += 1;
    this.peerIds = [];
    this.peerRoutes.clear();
    this.router.setRoutes([]);
    this.refreshedAt = 0;
    this.inflight = null;
    this.bridge.setEnabled(enabled);
  }

  async read(request: BlobRequest, context?: BlobRouteContext) {
    if (context?.signal?.aborted) throw new Error('P2P blob request was cancelled');
    const peerIds = await this.peerList();
    if (context?.signal?.aborted) throw new Error('P2P blob request was cancelled');
    if (peerIds.length === 0) return BLOB_NO_RESULT;
    return this.router.read(request, context);
  }

  async peerList(): Promise<string[]> {
    if (!this.bridge.isEnabled()) return [];
    if (Date.now() - this.refreshedAt < this.cacheMs) return [...this.peerIds];

    const generation = this.generation;
    const pending = this.inflight ?? this.bridge.listPeers();
    this.inflight = pending;
    try {
      const peerIds = await pending;
      if (generation !== this.generation || !this.bridge.isEnabled()) return [];
      this.peerIds = [...new Set(peerIds.filter(Boolean))].sort().slice(0, MAX_P2P_PEERS);
      this.syncPeerRoutes();
      this.refreshedAt = Date.now();
      return [...this.peerIds];
    } finally {
      if (this.inflight === pending) this.inflight = null;
    }
  }

  private syncPeerRoutes(): void {
    const next = new Map<string, BlobRoute>();
    for (const peerId of this.peerIds) {
      const route = this.peerRoutes.get(peerId) ?? {
        id: peerId,
        groupId: this.id,
        isAvailable: () => this.bridge.isEnabled() && this.peerIds.includes(peerId),
        read: (request: BlobRequest, context?: BlobRouteContext) => (
          this.bridge.fetch(request, peerId, context?.signal)
        ),
      };
      next.set(peerId, route);
    }
    this.peerRoutes = next;
    this.router.setRoutes([...next.values()]);
  }
}
