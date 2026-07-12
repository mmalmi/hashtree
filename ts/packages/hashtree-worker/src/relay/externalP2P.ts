import type { WorkerResponse } from './protocol.js';

type ExternalP2PRequest = Extract<WorkerResponse, { type: 'p2pFetch' | 'p2pPeerList' }>;
type PendingFetch = {
  resolve: (data: Uint8Array | null) => void;
  timeout: ReturnType<typeof setTimeout>;
};
type PendingPeerList = {
  resolve: (peerIds: string[]) => void;
  timeout: ReturnType<typeof setTimeout>;
};

export class ExternalP2PBridge {
  private readonly respond: (request: ExternalP2PRequest) => void;
  private readonly fetchTimeoutMs: number;
  private readonly peerListTimeoutMs: number;
  private readonly pendingFetches = new Map<string, PendingFetch>();
  private readonly pendingPeerLists = new Map<string, PendingPeerList>();
  private requestCounter = 0;
  private enabled = false;

  constructor(options: {
    respond: (request: ExternalP2PRequest) => void;
    fetchTimeoutMs: number;
    peerListTimeoutMs: number;
  }) {
    this.respond = options.respond;
    this.fetchTimeoutMs = options.fetchTimeoutMs;
    this.peerListTimeoutMs = options.peerListTimeoutMs;
  }

  setEnabled(enabled: boolean): void {
    this.enabled = enabled;
    if (!enabled) this.clear();
  }

  isEnabled(): boolean {
    return this.enabled;
  }

  fetch(hashHex: string, peerId?: string): Promise<Uint8Array | null> {
    if (!this.enabled) return Promise.resolve(null);
    const requestId = this.nextRequestId('p2p');
    return new Promise((resolve) => {
      const timeout = setTimeout(() => {
        this.pendingFetches.delete(requestId);
        resolve(null);
      }, this.fetchTimeoutMs);
      this.pendingFetches.set(requestId, { resolve, timeout });
      this.respond({ type: 'p2pFetch', requestId, hashHex, peerId });
    });
  }

  listPeers(): Promise<string[]> {
    if (!this.enabled) return Promise.resolve([]);
    const requestId = this.nextRequestId('p2p_peers');
    return new Promise((resolve) => {
      const timeout = setTimeout(() => {
        this.pendingPeerLists.delete(requestId);
        resolve([]);
      }, this.peerListTimeoutMs);
      this.pendingPeerLists.set(requestId, { resolve, timeout });
      this.respond({ type: 'p2pPeerList', requestId });
    });
  }

  resolveFetch(requestId: string, data?: Uint8Array, error?: string): void {
    const pending = this.pendingFetches.get(requestId);
    if (!pending) return;
    this.pendingFetches.delete(requestId);
    clearTimeout(pending.timeout);
    pending.resolve(error ? null : data ?? null);
  }

  resolvePeerList(requestId: string, peerIds?: string[], error?: string): void {
    const pending = this.pendingPeerLists.get(requestId);
    if (!pending) return;
    this.pendingPeerLists.delete(requestId);
    clearTimeout(pending.timeout);
    pending.resolve(error ? [] : [...new Set(peerIds ?? [])]);
  }

  clear(): void {
    for (const pending of this.pendingFetches.values()) {
      clearTimeout(pending.timeout);
      pending.resolve(null);
    }
    this.pendingFetches.clear();
    for (const pending of this.pendingPeerLists.values()) {
      clearTimeout(pending.timeout);
      pending.resolve([]);
    }
    this.pendingPeerLists.clear();
  }

  private nextRequestId(prefix: string): string {
    this.requestCounter += 1;
    return `${prefix}_${Date.now()}_${this.requestCounter}`;
  }
}
