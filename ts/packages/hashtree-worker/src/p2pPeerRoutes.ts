import type { BlobRequest, BlobRoute, BlobRouteContext } from '@hashtree/core';
import type { P2PBridge } from './p2pBridge.js';

const DEFAULT_PEER_LIST_CACHE_MS = 1_500;

/** One composite route whose provider owns authenticated peer selection. */
export class P2PPeerRoutes implements BlobRoute {
  readonly id = 'p2p';
  private peerIds: string[] = [];
  private refreshedAt = 0;
  private inflight: Promise<string[]> | null = null;
  private generation = 0;

  constructor(
    private readonly bridge: P2PBridge,
    private readonly cacheMs = DEFAULT_PEER_LIST_CACHE_MS,
  ) {}

  isAvailable = (): boolean => this.bridge.isEnabled();

  setEnabled(enabled: boolean): void {
    this.generation += 1;
    this.peerIds = [];
    this.refreshedAt = 0;
    this.inflight = null;
    this.bridge.setEnabled(enabled);
  }

  read(request: BlobRequest, context?: BlobRouteContext) {
    return this.bridge.fetch(request, undefined, context?.signal);
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
      this.peerIds = [...new Set(peerIds.filter(Boolean))].sort();
      this.refreshedAt = Date.now();
      return [...this.peerIds];
    } finally {
      if (this.inflight === pending) this.inflight = null;
    }
  }
}
