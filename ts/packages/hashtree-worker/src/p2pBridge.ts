import {
  BLOB_NO_RESULT,
  blobData,
  toHex,
  type BlobReply,
  type BlobRequest,
} from '@hashtree/core';

export type P2PBridgeRequest =
  | { type: 'p2pFetch'; requestId: string; hashHex: string; htl: number; peerId?: string }
  | { type: 'p2pPeerList'; requestId: string };

type Pending<T> = {
  resolve: (value: T) => void;
  reject: (error: Error) => void;
  timeout?: ReturnType<typeof setTimeout>;
  signal?: AbortSignal;
  abort?: () => void;
};

export class P2PBridge {
  private readonly respond: (request: P2PBridgeRequest) => void;
  private readonly fetchTimeoutMs?: number;
  private readonly peerListTimeoutMs: number;
  private readonly fetches = new Map<string, Pending<BlobReply>>();
  private readonly peerLists = new Map<string, Pending<string[]>>();
  private requestCounter = 0;
  private enabled = false;

  constructor(options: {
    respond: (request: P2PBridgeRequest) => void;
    fetchTimeoutMs?: number;
    peerListTimeoutMs: number;
  }) {
    this.respond = options.respond;
    this.fetchTimeoutMs = options.fetchTimeoutMs;
    this.peerListTimeoutMs = options.peerListTimeoutMs;
  }

  setEnabled(enabled: boolean): void {
    this.enabled = enabled;
    if (!enabled) this.clear('P2P provider is not configured');
  }

  isEnabled(): boolean {
    return this.enabled;
  }

  fetch(request: BlobRequest, peerId?: string, signal?: AbortSignal): Promise<BlobReply> {
    if (!this.enabled) return Promise.reject(new Error('P2P provider is not configured'));
    const requestId = this.nextRequestId('p2p');
    const message: P2PBridgeRequest & { type: 'p2pFetch' } = {
      type: 'p2pFetch',
      requestId,
      hashHex: toHex(request.hash),
      htl: request.htl,
    };
    if (peerId) message.peerId = peerId;

    return new Promise((resolve, reject) => {
      const pending: Pending<BlobReply> = { resolve, reject, signal };
      if (signal) {
        pending.abort = () => this.rejectFetch(requestId, new Error('P2P blob request was cancelled'));
        signal.addEventListener('abort', pending.abort, { once: true });
      }
      if (this.fetchTimeoutMs && this.fetchTimeoutMs > 0) {
        pending.timeout = setTimeout(() => {
          this.rejectFetch(
            requestId,
            new Error(`P2P blob request timed out after ${this.fetchTimeoutMs}ms`),
          );
        }, this.fetchTimeoutMs);
      }
      this.fetches.set(requestId, pending);
      if (signal?.aborted) {
        pending.abort?.();
        return;
      }
      try {
        this.respond(message);
      } catch (error) {
        this.rejectFetch(requestId, error instanceof Error ? error : new Error(String(error)));
      }
    });
  }

  listPeers(): Promise<string[]> {
    if (!this.enabled) return Promise.resolve([]);
    const requestId = this.nextRequestId('p2p_peers');
    return new Promise((resolve, reject) => {
      const pending: Pending<string[]> = { resolve, reject };
      pending.timeout = setTimeout(() => {
        this.rejectPeerList(requestId, new Error('P2P peer list request timed out'));
      }, this.peerListTimeoutMs);
      this.peerLists.set(requestId, pending);
      try {
        this.respond({ type: 'p2pPeerList', requestId });
      } catch (error) {
        this.rejectPeerList(requestId, error instanceof Error ? error : new Error(String(error)));
      }
    });
  }

  resolveFetch(requestId: string, data?: Uint8Array, error?: string): void {
    const pending = this.take(this.fetches, requestId);
    if (!pending) return;
    if (error) {
      pending.reject(new Error(error));
      return;
    }
    pending.resolve(data === undefined ? BLOB_NO_RESULT : blobData(data));
  }

  resolvePeerList(requestId: string, peerIds?: string[], error?: string): void {
    const pending = this.take(this.peerLists, requestId);
    if (!pending) return;
    if (error) {
      pending.reject(new Error(error));
      return;
    }
    pending.resolve([...new Set(peerIds ?? [])]);
  }

  clear(message = 'P2P bridge was cleared'): void {
    for (const requestId of this.fetches.keys()) this.rejectFetch(requestId, new Error(message));
    for (const requestId of this.peerLists.keys()) this.take(this.peerLists, requestId)?.resolve([]);
  }

  private rejectFetch(requestId: string, error: Error): void {
    this.take(this.fetches, requestId)?.reject(error);
  }

  private rejectPeerList(requestId: string, error: Error): void {
    this.take(this.peerLists, requestId)?.reject(error);
  }

  private take<T>(pendingById: Map<string, Pending<T>>, requestId: string): Pending<T> | undefined {
    const pending = pendingById.get(requestId);
    if (!pending) return undefined;
    pendingById.delete(requestId);
    if (pending.timeout) clearTimeout(pending.timeout);
    if (pending.signal && pending.abort) pending.signal.removeEventListener('abort', pending.abort);
    return pending;
  }

  private nextRequestId(prefix: string): string {
    this.requestCounter += 1;
    return `${prefix}_${Date.now()}_${this.requestCounter}`;
  }
}
