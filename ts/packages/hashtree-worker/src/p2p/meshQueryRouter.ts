import type { Store } from '@hashtree/core';
import {
  MAX_HTL,
  createRequest,
  decrementHTL,
  encodeRequest,
  hashToKey,
  shouldForward,
  verifyHash,
  type DataRequest,
  type PeerHTLConfig,
} from '@hashtree/mesh';

type TimeoutHandle = ReturnType<typeof setTimeout>;

export interface MeshQueryRouterPeer {
  peerId: string;
  canSend: () => boolean;
  getHtlConfig: () => PeerHTLConfig;
  sendRequest: (hash: Uint8Array, htl: number) => boolean;
  sendResponse: (hash: Uint8Array, data: Uint8Array) => Promise<void>;
  onForwardedRequest?: () => void;
  onForwardedResolved?: () => void;
  onForwardedSuppressed?: () => void;
}

export interface MeshPeerQueryOptions {
  excludePeerId?: string;
  htl: number;
}

export interface MeshQueryRouterConfig {
  localStore: Store;
  requestTimeoutMs: number;
  upstreamFetch?: (hash: Uint8Array) => Promise<Uint8Array | null>;
  queryPeers?: (hash: Uint8Array, options: MeshPeerQueryOptions) => Promise<Uint8Array | null>;
  maxForwardsPerPeerWindow?: number;
  forwardRateLimitWindowMs?: number;
}

export interface MeshForwardRateLimitConfig {
  maxForwardsPerPeerWindow?: number;
  windowMs?: number;
}

interface InFlightQuery {
  requesterIds: Set<string>;
  timeoutId: TimeoutHandle;
}

class SlidingWindowRateLimiter {
  private readonly maxEvents: number;
  private readonly windowMs: number;
  private readonly eventsByPeer = new Map<string, number[]>();

  constructor(maxEvents: number, windowMs: number) {
    this.maxEvents = maxEvents;
    this.windowMs = windowMs;
  }

  allow(peerId: string): boolean {
    const now = Date.now();
    const events = this.eventsByPeer.get(peerId) ?? [];
    let firstActiveIndex = 0;
    while (firstActiveIndex < events.length && now - events[firstActiveIndex] >= this.windowMs) {
      firstActiveIndex += 1;
    }
    if (firstActiveIndex > 0) {
      events.splice(0, firstActiveIndex);
    }

    if (events.length >= this.maxEvents) {
      this.eventsByPeer.set(peerId, events);
      return false;
    }

    events.push(now);
    this.eventsByPeer.set(peerId, events);
    return true;
  }

  resetPeer(peerId: string): void {
    this.eventsByPeer.delete(peerId);
  }

  clear(): void {
    this.eventsByPeer.clear();
  }
}

export class MeshQueryRouter {
  private readonly localStore: Store;
  private readonly requestTimeoutMs: number;
  private rateLimiter: SlidingWindowRateLimiter;
  private readonly peers = new Map<string, MeshQueryRouterPeer>();
  private readonly hashesByRequester = new Map<string, Set<string>>();
  private readonly inFlightByHash = new Map<string, InFlightQuery>();
  private readonly pendingUpstreamFetches = new Map<string, Promise<Uint8Array | null>>();
  private upstreamFetch?: (hash: Uint8Array) => Promise<Uint8Array | null>;
  private queryPeers?: (hash: Uint8Array, options: MeshPeerQueryOptions) => Promise<Uint8Array | null>;

  constructor(config: MeshQueryRouterConfig) {
    this.localStore = config.localStore;
    this.requestTimeoutMs = config.requestTimeoutMs;
    this.upstreamFetch = config.upstreamFetch;
    this.queryPeers = config.queryPeers;
    this.rateLimiter = this.createRateLimiter({
      maxForwardsPerPeerWindow: config.maxForwardsPerPeerWindow,
      windowMs: config.forwardRateLimitWindowMs,
    });
  }

  registerPeer(peer: MeshQueryRouterPeer): void {
    this.peers.set(peer.peerId, peer);
  }

  removePeer(peerId: string): void {
    const hashes = this.hashesByRequester.get(peerId);
    if (hashes) {
      for (const hashKey of Array.from(hashes)) {
        const inFlight = this.inFlightByHash.get(hashKey);
        if (!inFlight) continue;
        inFlight.requesterIds.delete(peerId);
        if (inFlight.requesterIds.size === 0) {
          this.clearQuery(hashKey);
        }
      }
    }

    this.hashesByRequester.delete(peerId);
    this.peers.delete(peerId);
    this.rateLimiter.resetPeer(peerId);
  }

  setUpstreamFetch(upstreamFetch?: (hash: Uint8Array) => Promise<Uint8Array | null>): void {
    this.upstreamFetch = upstreamFetch;
  }

  setForwardRateLimit(config?: MeshForwardRateLimitConfig): void {
    this.rateLimiter = this.createRateLimiter(config);
  }

  hasInFlight(hashKey: string): boolean {
    return this.inFlightByHash.has(hashKey);
  }

  stop(): void {
    for (const hashKey of Array.from(this.inFlightByHash.keys())) {
      this.clearQuery(hashKey);
    }
    this.hashesByRequester.clear();
    this.pendingUpstreamFetches.clear();
    this.rateLimiter.clear();
  }

  private createRateLimiter(config?: MeshForwardRateLimitConfig): SlidingWindowRateLimiter {
    return new SlidingWindowRateLimiter(
      config?.maxForwardsPerPeerWindow ?? 64,
      config?.windowMs ?? 1000,
    );
  }

  async handleRequest(requesterId: string, req: DataRequest): Promise<void> {
    const hashKey = hashToKey(req.h);
    const requester = this.peers.get(requesterId);
    if (!requester) {
      return;
    }

    const local = await this.localStore.get(req.h);
    if (local) {
      await requester.sendResponse(req.h, local);
      return;
    }

    const begin = this.beginQuery(hashKey, requesterId);
    if (begin === 'suppressed') {
      requester.onForwardedSuppressed?.();
      return;
    }

    const shouldAttemptPeerQuery = this.shouldAttemptPeerQuery(requesterId, req.htl ?? MAX_HTL);
    const peerQueryAllowed = !shouldAttemptPeerQuery || this.rateLimiter.allow(requesterId);
    const peerQueryActive = peerQueryAllowed
      ? this.startPeerQuery(hashKey, req.h, requesterId, req.htl ?? MAX_HTL)
      : false;
    const upstreamActive = this.startUpstreamFetch(hashKey, req.h);
    const forwarded = peerQueryAllowed && !peerQueryActive
      ? this.forwardRequest(requesterId, req.h, req.htl ?? MAX_HTL)
      : 0;
    if (peerQueryActive || forwarded > 0 || upstreamActive) {
      requester.onForwardedRequest?.();
      return;
    }

    this.clearQuery(hashKey);
  }

  async resolve(hash: Uint8Array, data: Uint8Array): Promise<void> {
    const hashKey = hashToKey(hash);
    const requesterIds = this.clearQuery(hashKey);
    if (requesterIds.length === 0) {
      return;
    }

    await this.localStore.put(hash, data).catch(() => false);
    for (const requesterId of requesterIds) {
      const requester = this.peers.get(requesterId);
      if (!requester) {
        continue;
      }
      requester.onForwardedResolved?.();
      await requester.sendResponse(hash, data);
    }
  }

  private beginQuery(hashKey: string, requesterId: string): 'new' | 'suppressed' {
    const existing = this.inFlightByHash.get(hashKey);
    if (existing) {
      this.trackRequester(hashKey, existing.requesterIds, requesterId);
      return 'suppressed';
    }

    const requesterIds = new Set<string>();
    this.trackRequester(hashKey, requesterIds, requesterId);
    const timeoutId = setTimeout(() => {
      this.clearQuery(hashKey);
    }, this.requestTimeoutMs);

    this.inFlightByHash.set(hashKey, { requesterIds, timeoutId });
    return 'new';
  }

  private shouldAttemptPeerQuery(requesterId: string, htl: number): boolean {
    if (!shouldForward(htl)) {
      return false;
    }
    for (const peer of this.peers.values()) {
      if (peer.peerId !== requesterId && peer.canSend()) {
        return true;
      }
    }
    return false;
  }

  private clearQuery(hashKey: string): string[] {
    const inFlight = this.inFlightByHash.get(hashKey);
    if (!inFlight) {
      return [];
    }

    clearTimeout(inFlight.timeoutId);
    this.inFlightByHash.delete(hashKey);

    const requesterIds = Array.from(inFlight.requesterIds);
    for (const requesterId of requesterIds) {
      const hashes = this.hashesByRequester.get(requesterId);
      if (!hashes) continue;
      hashes.delete(hashKey);
      if (hashes.size === 0) {
        this.hashesByRequester.delete(requesterId);
      }
    }

    return requesterIds;
  }

  private trackRequester(hashKey: string, requesterIds: Set<string>, requesterId: string): void {
    requesterIds.add(requesterId);
    let hashes = this.hashesByRequester.get(requesterId);
    if (!hashes) {
      hashes = new Set<string>();
      this.hashesByRequester.set(requesterId, hashes);
    }
    hashes.add(hashKey);
  }

  private forwardRequest(requesterId: string, hash: Uint8Array, htl: number): number {
    if (!shouldForward(htl)) {
      return 0;
    }

    const requester = this.peers.get(requesterId);
    if (!requester) {
      return 0;
    }

    const nextHtl = decrementHTL(htl, requester.getHtlConfig());
    let forwarded = 0;
    for (const peer of this.peers.values()) {
      if (peer.peerId === requesterId || !peer.canSend()) {
        continue;
      }

      if (peer.sendRequest(hash, nextHtl)) {
        forwarded += 1;
      }
    }
    return forwarded;
  }

  private startPeerQuery(hashKey: string, hash: Uint8Array, requesterId: string, htl: number): boolean {
    if (!this.queryPeers || !shouldForward(htl)) {
      return false;
    }

    const requester = this.peers.get(requesterId);
    if (!requester) {
      return false;
    }

    const nextHtl = decrementHTL(htl, requester.getHtlConfig());
    void this.queryPeers(hash, {
      excludePeerId: requesterId,
      htl: nextHtl,
    }).then(async (data) => {
      if (!data || !this.inFlightByHash.has(hashKey)) {
        return;
      }

      const valid = await verifyHash(data, hash);
      if (!valid) {
        return;
      }

      await this.resolve(hash, data);
    }).catch(() => undefined);
    return true;
  }

  private startUpstreamFetch(hashKey: string, hash: Uint8Array): boolean {
    if (!this.upstreamFetch) {
      return false;
    }

    const existing = this.pendingUpstreamFetches.get(hashKey);
    if (existing) {
      return true;
    }

    let pending: Promise<Uint8Array | null>;
    pending = this.upstreamFetch(hash)
      .then(async (data) => {
        if (!data) {
          return null;
        }

        const valid = await verifyHash(data, hash);
        if (!valid) {
          return null;
        }

        await this.resolve(hash, data);
        return data;
      })
      .catch(() => null)
      .finally(() => {
        if (this.pendingUpstreamFetches.get(hashKey) === pending) {
          this.pendingUpstreamFetches.delete(hashKey);
        }
      });

    this.pendingUpstreamFetches.set(hashKey, pending);
    return true;
  }
}

export function encodeForwardRequest(hash: Uint8Array, htl: number): Uint8Array {
  return new Uint8Array(encodeRequest(createRequest(hash, htl)));
}
