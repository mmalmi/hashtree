/**
 * WebRTC-based distributed store for hashtree
 *
 * Implements the Store interface, fetching data from P2P network.
 * Uses Nostr relays for WebRTC signaling with perfect negotiation (both peers can initiate).
 *
 * Signaling protocol (all use ephemeral kind 25050):
 * - Hello messages: #l: "hello" tag, broadcast for peer discovery (unencrypted)
 * - Directed signaling (offer, answer, candidate, candidates): #p tag with
 *   recipient pubkey, NIP-17 style gift wrap for privacy
 *
 * Pool-based peer management:
 * - 'follows' pool: Users in your social graph (followed or followers)
 * - 'other' pool: Everyone else (randos)
 * Each pool has its own connection limits.
 */
import { SimplePool, type Event, verifyEvent } from 'nostr-tools';
import type { Store, Hash } from '@hashtree/core';
import { fromHex, sha256, toHex } from '@hashtree/core';
import {
  PeerId,
  createMeshNostrEventFrame,
  type SignalingMessage,
  type DirectedMessage,
  type WebRTCStoreConfig,
  type PeerStatus,
  type WebRTCStoreEvent,
  type WebRTCStoreEventHandler,
  type EventSigner,
  type EventEncrypter,
  type EventDecrypter,
  type GiftWrapper,
  type GiftUnwrapper,
  type SignedEvent,
  type PeerPool,
  type PeerClassifier,
  type PoolConfig,
  type SelectionStrategy,
  type RequestDispatchConfig,
  type PeerMetadataSnapshot,
  type MeshStats,
  type MeshNostrFrame,
  validateMeshNostrFrame,
  MESH_EVENT_POLICY,
} from './types.js';
import { decrementHTLWithPolicy, shouldForwardHTL } from './protocol.js';
import {
  PeerSelector,
  buildHedgedWavePlan,
  normalizeDispatchConfig,
  syncSelectorPeers,
} from './peerSelector.js';
import { Peer } from './peer.js';

export const DEFAULT_RELAYS = [
  'wss://relay.damus.io',
  'wss://relay.primal.net',
  'wss://relay.nostr.band',
  'wss://temp.iris.to',
  'wss://relay.snort.social',
];

// All WebRTC signaling uses ephemeral kind 25050
// Hello messages use #l tag for broadcast discovery
// Directed messages use #p tag with gift wrap
const SIGNALING_KIND = 25050;
const HELLO_TAG = 'hello';
const HASH_GET_TAG = 'hashGet';
const SEEN_FRAME_CAP = 4096;
const SEEN_FRAME_TTL_MS = 120_000;
const SEEN_EVENT_CAP = 8192;
const SEEN_EVENT_TTL_MS = 600_000;
const PEER_METADATA_POINTER_SLOT_KEY = 'hashtree-webrtc/peer-metadata/latest/v1';

const DEFAULT_REQUEST_DISPATCH: RequestDispatchConfig = {
  initialFanout: 2,
  hedgeFanout: 1,
  maxFanout: 8,
  hedgeIntervalMs: 120,
};

function decodeHashGetTag(value: string | undefined): boolean {
  if (value === undefined) return true;
  return !['0', 'false', 'FALSE', 'no', 'NO'].includes(value);
}


// Pending request with callbacks
interface PendingReq {
  resolve: (data: Uint8Array | null) => void;
  timeout: ReturnType<typeof setTimeout>;
  triedPeers: Set<string>;
}

// Extended peer info with pool assignment
interface PeerInfo {
  peer: Peer;
  pool: PeerPool;
  hashGet: boolean;
}

interface InFlightPeerRequest {
  peerId: string;
  settled: boolean;
  promise: Promise<{ peerId: string; data: Uint8Array | null; elapsedMs: number }>;
}

export class WebRTCStore implements Store {
  private config: {
    helloInterval: number;
    messageTimeout: number;
    requestTimeout: number;
    peerQueryDelay: number;
    relays: string[];
    localStore: Store | null;
    fallbackStores: Store[];
    debug: boolean;
  };
  private routing: {
    selectionStrategy: SelectionStrategy;
    fairnessEnabled: boolean;
    dispatch: RequestDispatchConfig;
  };
  private readonly peerSelector: PeerSelector;
  private pools: { follows: PoolConfig; other: PoolConfig };
  private peerClassifier: PeerClassifier;
  private getFollowedPubkeys: (() => string[]) | null;
  private isPeerBlocked: ((pubkey: string) => boolean) | null;
  private signer: EventSigner;
  private encrypt: EventEncrypter;
  private decrypt: EventDecrypter;
  private giftWrap: GiftWrapper;
  private giftUnwrap: GiftUnwrapper;
  private myPeerId: PeerId;
  private pool: SimplePool;
  private subscriptions: ReturnType<SimplePool['subscribe']>[] = [];
  private helloSubscription: ReturnType<SimplePool['subscribe']> | null = null;
  private peers = new Map<string, PeerInfo>();
  // Track pubkeys we're currently connecting to in 'other' pool (prevents race conditions)
  private pendingOtherPubkeys = new Set<string>();
  private helloInterval: ReturnType<typeof setInterval> | null = null;
  private cleanupInterval: ReturnType<typeof setInterval> | null = null;
  private eventHandlers = new Set<WebRTCStoreEventHandler>();
  private running = false;
  private pendingReqs = new Map<Hash, PendingReq[]>();
  // Deduplicate concurrent get() calls for the same hash
  private pendingGets = new Map<string, Promise<Uint8Array | null>>();
  // Store-level stats (not per-peer)
  private blossomFetches = 0;
  // Track current hello subscription authors for change detection
  private currentHelloAuthors: string[] | null = null;
  // Relayless mesh dedupe
  private seenFrameIds = new Map<string, number>();
  private seenEventIds = new Map<string, number>();
  // Relayless mesh stats
  private meshReceived = 0;
  private meshForwarded = 0;
  private meshDroppedDuplicate = 0;

  constructor(config: WebRTCStoreConfig) {
    this.signer = config.signer;
    this.encrypt = config.encrypt;
    this.decrypt = config.decrypt;
    this.giftWrap = config.giftWrap;
    this.giftUnwrap = config.giftUnwrap;
    this.myPeerId = new PeerId(config.pubkey);

    // Default classifier: everyone is 'other' unless classifier provided
    this.peerClassifier = config.peerClassifier ?? (() => 'other');
    // Function to get followed pubkeys for subscription filtering
    this.getFollowedPubkeys = config.getFollowedPubkeys ?? null;
    // Function to check if a peer is blocked
    this.isPeerBlocked = config.isPeerBlocked ?? null;

    // Use pool config if provided, otherwise fall back to legacy config or defaults
    if (config.pools) {
      this.pools = config.pools;
    } else {
      // Legacy mode: single pool with old config values
      const maxConn = config.maxConnections ?? 6;
      const satConn = config.satisfiedConnections ?? 3;
      this.pools = {
        follows: { maxConnections: 0, satisfiedConnections: 0 }, // No follows pool in legacy
        other: { maxConnections: maxConn, satisfiedConnections: satConn },
      };
    }

    this.config = {
      helloInterval: config.helloInterval ?? 3000,
      messageTimeout: config.messageTimeout ?? 60000, // 60 seconds for relay propagation
      requestTimeout: config.requestTimeout ?? 500,
      peerQueryDelay: config.peerQueryDelay ?? 500,
      relays: config.relays ?? DEFAULT_RELAYS,
      localStore: config.localStore ?? null,
      fallbackStores: config.fallbackStores ?? [],
      debug: config.debug ?? false,
    };

    const dispatch = config.requestDispatch ?? (
      config.peerQueryDelay !== undefined
        ? {
          initialFanout: 1,
          hedgeFanout: 1,
          maxFanout: Number.MAX_SAFE_INTEGER,
          hedgeIntervalMs: Math.max(0, config.peerQueryDelay),
        }
        : DEFAULT_REQUEST_DISPATCH
    );
    this.routing = {
      // Retrieval should prefer the peer with the best reply odds.
      selectionStrategy: config.requestSelectionStrategy ?? 'weighted',
      fairnessEnabled: config.requestFairnessEnabled ?? true,
      dispatch,
    };
    this.peerSelector = PeerSelector.withStrategy(this.routing.selectionStrategy);
    this.peerSelector.setFairness(this.routing.fairnessEnabled);

    this.pool = new SimplePool();
  }

  private log(...args: unknown[]): void {
    if (this.config.debug) {
      console.log('[WebRTCStore]', ...args);
    }
  }

  /**
   * Get pool counts
   */
  private getPoolCounts(): { follows: { connected: number; total: number }; other: { connected: number; total: number } } {
    const counts = {
      follows: { connected: 0, total: 0 },
      other: { connected: 0, total: 0 },
    };

    for (const { peer, pool } of this.peers.values()) {
      counts[pool].total++;
      if (peer.isConnected) {
        counts[pool].connected++;
      }
    }

    return counts;
  }

  /**
   * Check if we can accept a peer in a given pool
   */
  private canAcceptPeer(pool: PeerPool): boolean {
    const counts = this.getPoolCounts();
    return counts[pool].total < this.pools[pool].maxConnections;
  }

  /**
   * Check if a pool is satisfied
   */
  private isPoolSatisfied(pool: PeerPool): boolean {
    const counts = this.getPoolCounts();
    return counts[pool].connected >= this.pools[pool].satisfiedConnections;
  }

  /**
   * Check if we already have a connection from a pubkey in the 'other' pool.
   * In the 'other' pool, we only allow 1 instance per pubkey.
   * Also checks pendingOtherPubkeys to prevent race conditions.
   */
  private hasOtherPoolPubkey(pubkey: string): boolean {
    if (this.pendingOtherPubkeys.has(pubkey)) {
      return true;
    }
    for (const { peer, pool } of this.peers.values()) {
      if (pool === 'other' && peer.pubkey === pubkey) {
        return true;
      }
    }
    return false;
  }

  /**
   * Start the WebRTC store - connect to relays and begin peer discovery
   */
  start(): void {
    if (this.running) return;
    this.running = true;

    this.log('Starting with peerId:', this.myPeerId.short());
    this.log('Pool config:', this.pools);
    this.log('Relays:', this.config.relays);

    // Subscribe to signaling messages
    this.startSubscription();

    // Send hello messages when not satisfied
    this.helloInterval = setInterval(() => {
      this.maybeSendHello();
    }, this.config.helloInterval);

    // Send initial hello
    this.maybeSendHello();

    // Cleanup stale connections
    this.cleanupInterval = setInterval(() => {
      this.cleanupConnections();
    }, 5000);
  }

  /**
   * Stop the WebRTC store
   */
  stop(): void {
    if (!this.running) return;
    this.running = false;

    this.log('Stopping');

    if (this.helloInterval) {
      clearInterval(this.helloInterval);
      this.helloInterval = null;
    }

    if (this.cleanupInterval) {
      clearInterval(this.cleanupInterval);
      this.cleanupInterval = null;
    }

    for (const sub of this.subscriptions) {
      sub.close();
    }
    this.subscriptions = [];

    // Close all peer connections
    for (const [peerId, { peer }] of this.peers.entries()) {
      this.peerSelector.removePeer(peerId);
      peer.close();
    }
    this.peers.clear();
    this.pendingOtherPubkeys.clear();
    this.seenFrameIds.clear();
    this.seenEventIds.clear();
  }

  /**
   * Set relays and reconnect. Useful for runtime relay configuration changes.
   * Closes existing subscriptions and starts new ones with the updated relays.
   */
  setRelays(relays: string[]): void {
    this.log('setRelays:', relays);
    this.config.relays = relays;

    // If running, restart subscriptions with new relays
    if (this.running) {
      // Close existing subscriptions
      for (const sub of this.subscriptions) {
        sub.close();
      }
      this.subscriptions = [];

      // Clear existing peers (they were discovered via old relays)
      for (const [peerId, { peer }] of this.peers.entries()) {
        this.peerSelector.removePeer(peerId);
        peer.close();
      }
      this.peers.clear();

      // Start new subscriptions with updated relays
      this.startSubscription();

      this.emit({ type: 'update' });
    }
  }

  /**
   * Add event listener
   */
  on(handler: WebRTCStoreEventHandler): () => void {
    this.eventHandlers.add(handler);
    return () => this.eventHandlers.delete(handler);
  }

  private emit(event: WebRTCStoreEvent): void {
    for (const handler of this.eventHandlers) {
      handler(event);
    }
  }

  private startSubscription(): void {
    const since = Math.floor((Date.now() - this.config.messageTimeout) / 1000);

    const subHandler = {
      onevent: (event: Event) => {
        this.handleSignalingEvent(event);
      },
      oneose: () => {},
    };

    // 1. Subscribe to hello messages based on pool configuration
    this.setupHelloSubscription(since, subHandler);

    // 2. Subscribe to directed signaling (kind 25050 with #p tag) for offers/answers/candidates
    // Always subscribe to directed messages (needed to receive offers/answers)
    this.subscriptions.push(
      this.pool.subscribe(
        this.config.relays,
        {
          kinds: [SIGNALING_KIND],
          '#p': [this.myPeerId.pubkey],
          since,
        },
        subHandler
      )
    );
  }

  /**
   * Setup hello subscription based on pool configuration
   * - If both pools are 0: don't subscribe to hellos
   * - If other pool is disabled but follows is enabled: subscribe to followed pubkeys only
   * - If other pool is enabled: subscribe to all hellos
   */
  private setupHelloSubscription(since: number, subHandler: { onevent: (event: Event) => void; oneose: () => void }): void {
    // Close existing hello subscription if any
    if (this.helloSubscription) {
      this.helloSubscription.close();
      this.helloSubscription = null;
    }

    const followsMax = this.pools.follows.maxConnections;
    const otherMax = this.pools.other.maxConnections;

    // If both pools are disabled, don't subscribe to hellos at all
    if (followsMax === 0 && otherMax === 0) {
      this.log('Both pools disabled, not subscribing to hellos');
      this.currentHelloAuthors = [];
      return;
    }

    // If other pool is disabled but follows pool is enabled, only subscribe to followed users
    if (otherMax === 0 && followsMax > 0) {
      const followedPubkeys = this.getFollowedPubkeys?.() ?? [];
      if (followedPubkeys.length === 0) {
        this.log('Follows pool enabled but no followed users, not subscribing to hellos');
        this.currentHelloAuthors = [];
        return;
      }

      this.log('Other pool disabled, subscribing to hellos from', followedPubkeys.length, 'followed users');
      this.currentHelloAuthors = [...followedPubkeys];
      this.helloSubscription = this.pool.subscribe(
        this.config.relays,
        {
          kinds: [SIGNALING_KIND],
          '#l': [HELLO_TAG],
          authors: followedPubkeys,
          since,
        },
        subHandler
      );
      return;
    }

    // Otherwise subscribe to all hellos
    this.log('Subscribing to all hellos');
    this.currentHelloAuthors = null; // null means all authors
    this.helloSubscription = this.pool.subscribe(
      this.config.relays,
      {
        kinds: [SIGNALING_KIND],
        '#l': [HELLO_TAG],
        since,
      },
      subHandler
    );
  }

  /**
   * Update hello subscription when follows list changes
   * Call this after social graph updates
   */
  updateHelloSubscription(): void {
    if (!this.running) return;

    const followsMax = this.pools.follows.maxConnections;
    const otherMax = this.pools.other.maxConnections;

    // Only need to update if we're in follows-only mode
    if (otherMax === 0 && followsMax > 0) {
      const followedPubkeys = this.getFollowedPubkeys?.() ?? [];
      const currentAuthors = this.currentHelloAuthors ?? [];

      // Check if follows list changed
      const changed = followedPubkeys.length !== currentAuthors.length ||
        !followedPubkeys.every(pk => currentAuthors.includes(pk));

      if (changed) {
        const since = Math.floor((Date.now() - this.config.messageTimeout) / 1000);
        const subHandler = {
          onevent: (event: Event) => {
            this.handleSignalingEvent(event);
          },
          oneose: () => {},
        };
        this.setupHelloSubscription(since, subHandler);
      }
    }
  }

  private async handleSignalingEvent(event: Event): Promise<void> {
    // Filter out old events (created more than messageTimeout ago)
    const eventAge = Date.now() / 1000 - (event.created_at ?? 0);
    if (eventAge > this.config.messageTimeout / 1000) {
      return;
    }

    // Check expiration
    const expirationTag = event.tags.find(t => t[0] === 'expiration');
    if (expirationTag) {
      const expiration = parseInt(expirationTag[1], 10);
      if (expiration < Date.now() / 1000) {
        return;
      }
    }

    // Check if this is a hello message (#l: hello tag)
    const lTag = event.tags.find(t => t[0] === 'l')?.[1];
    if (lTag === HELLO_TAG) {
      if (event.tags.some((tag) => tag[0] === 'peerId' && typeof tag[1] === 'string')) {
        const hashGet = decodeHashGetTag(event.tags.find((tag) => tag[0] === HASH_GET_TAG)?.[1]);
        await this.handleHello(event.pubkey, hashGet);
      }
      return;
    }

    // Check if this is a directed message (#p tag pointing to us)
    const pTag = event.tags.find(t => t[0] === 'p')?.[1];
    if (pTag === this.myPeerId.pubkey) {
      // Gift-wrapped signaling message - try to unwrap
      try {
        const inner = await this.giftUnwrap(event as SignedEvent);
        if (!inner) {
          return; // Can't decrypt - not for us
        }

        const msg = JSON.parse(inner.content) as DirectedMessage;
        await this.handleSignalingMessage(msg, inner.pubkey);
      } catch {
        // Not for us or invalid - ignore silently
      }
    }
  }

  private async handleSignalingMessage(msg: DirectedMessage, senderPubkey: string): Promise<void> {
    const targetPeerId = PeerId.fromString(msg.targetPeerId).toString();

    // Directed message - check if it's for us
    if (targetPeerId !== this.myPeerId.toString()) {
      return;
    }

    const peerId = new PeerId(senderPubkey);
    const peerIdStr = peerId.toString();
    const normalizedMsg = {
      ...msg,
      peerId: peerIdStr,
      targetPeerId,
    } as DirectedMessage;

    if (normalizedMsg.type === 'offer') {
      await this.handleOffer(peerId, normalizedMsg);
    } else {
      // answer or candidate
      const peerInfo = this.peers.get(peerIdStr);
      if (peerInfo) {
        await peerInfo.peer.handleSignaling(normalizedMsg);
      }
    }
  }

  private async handleHello(senderPubkey: string, hashGet: boolean): Promise<void> {
    const peerId = new PeerId(senderPubkey);

    // Skip self
    if (peerId.toString() === this.myPeerId.toString()) {
      return;
    }

    // Skip blocked peers
    if (this.isPeerBlocked?.(senderPubkey)) {
      this.log('Ignoring hello from blocked peer:', senderPubkey.slice(0, 8));
      return;
    }

    // Check if we already have this peer
    const existing = this.peers.get(peerId.toString());
    if (existing) {
      existing.hashGet = hashGet;
      return;
    }

    // Classify the peer
    let pool: 'follows' | 'other';
    try {
      pool = this.peerClassifier(senderPubkey);
    } catch (e) {
      this.log('Error classifying peer:', e);
      pool = 'other';
    }

    // Check if we can accept this peer in their pool
    if (!this.canAcceptPeer(pool)) {
      return;
    }

    // In 'other' pool, only allow 1 instance per pubkey
    if (pool === 'other' && this.hasOtherPoolPubkey(senderPubkey)) {
      return;
    }

    // Perfect negotiation: both peers initiate
    // Collision is handled by Peer class (polite peer rolls back)
    // Mark as pending before async operation to prevent race conditions
    if (pool === 'other') {
      this.pendingOtherPubkeys.add(senderPubkey);
    }
    try {
      await this.connectToPeer(peerId, pool, hashGet);
    } finally {
      this.pendingOtherPubkeys.delete(senderPubkey);
    }
  }

  private async handleOffer(peerId: PeerId, msg: SignalingMessage): Promise<void> {
    // Skip self (exact same peerId)
    if (peerId.toString() === this.myPeerId.toString()) {
      return;
    }

    // Skip blocked peers
    if (this.isPeerBlocked?.(peerId.pubkey)) {
      this.log('Ignoring offer from blocked peer:', peerId.pubkey.slice(0, 8));
      return;
    }

    const peerIdStr = peerId.toString();

    // Classify the peer
    const pool = this.peerClassifier(peerId.pubkey);

    // Check if we can accept (unless we already have this peer)
    if (!this.peers.has(peerIdStr) && !this.canAcceptPeer(pool)) {
      return;
    }

    // In 'other' pool, only allow 1 instance per pubkey (unless we already have this exact peer)
    if (!this.peers.has(peerIdStr) && pool === 'other' && this.hasOtherPoolPubkey(peerId.pubkey)) {
      return;
    }

    // Mark as pending before any async gaps to prevent race conditions
    if (pool === 'other' && !this.peers.has(peerIdStr)) {
      this.pendingOtherPubkeys.add(peerId.pubkey);
    }

    // Clean up existing connection if any
    const existing = this.peers.get(peerIdStr);
    const hashGet = existing?.hashGet ?? true;
    if (existing) {
      this.peerSelector.removePeer(peerIdStr);
      existing.peer.close();
      this.peers.delete(peerIdStr);
    }

    const peer = new Peer({
      peerId,
      myPeerId: this.myPeerId.toString(),
      direction: 'inbound',
      localStore: this.config.localStore,
      sendSignaling: (m) => this.dispatchSignaling(m, peerId.pubkey),
      onClose: () => this.handlePeerClose(peerIdStr),
      onConnected: () => {
        this.emit({ type: 'peer-connected', peerId: peerIdStr });
        this.emit({ type: 'update' });
        this.maybeSendHello();
        this.tryPendingReqs(peer);
      },
      onForwardRequest: (hash, exclude, htl) => this.forwardRequest(hash, exclude, htl),
      onMeshFrame: (fromPeerId, frame) => this.handleMeshFrame(fromPeerId, frame),
      requestTimeout: this.config.requestTimeout,
      debug: this.config.debug,
    });

    this.peers.set(peerIdStr, { peer, pool, hashGet });
    this.peerSelector.addPeer(peerIdStr);
    // Clear pending now that peer is in the map
    this.pendingOtherPubkeys.delete(peerId.pubkey);
    await peer.handleSignaling(msg);
  }

  private async connectToPeer(peerId: PeerId, pool: PeerPool, hashGet = true): Promise<void> {
    const peerIdStr = peerId.toString();

    if (this.peers.has(peerIdStr)) {
      return;
    }

    this.log('Initiating connection to', peerId.short(), 'pool:', pool);

    const peer = new Peer({
      peerId,
      myPeerId: this.myPeerId.toString(),
      direction: 'outbound',
      localStore: this.config.localStore,
      sendSignaling: (m) => this.dispatchSignaling(m, peerId.pubkey),
      onClose: () => this.handlePeerClose(peerIdStr),
      onConnected: () => {
        this.emit({ type: 'peer-connected', peerId: peerIdStr });
        this.emit({ type: 'update' });
        this.maybeSendHello();
        this.tryPendingReqs(peer);
      },
      onForwardRequest: (hash, exclude, htl) => this.forwardRequest(hash, exclude, htl),
      onMeshFrame: (fromPeerId, frame) => this.handleMeshFrame(fromPeerId, frame),
      requestTimeout: this.config.requestTimeout,
      debug: this.config.debug,
    });

    this.peers.set(peerIdStr, { peer, pool, hashGet });
    this.peerSelector.addPeer(peerIdStr);
    await peer.connect();
  }

  private handlePeerClose(peerIdStr: string): void {
    this.peerSelector.removePeer(peerIdStr);
    this.peers.delete(peerIdStr);
    this.emit({ type: 'peer-disconnected', peerId: peerIdStr });
    this.emit({ type: 'update' });
  }

  private orderedConnectedPeers(excludePeerId?: string): Peer[] {
    const connectedAll = Array.from(this.peers.values())
      .filter(({ peer, hashGet }) => peer.isConnected && hashGet);
    if (connectedAll.length === 0) return [];

    const currentPeerIds = connectedAll.map(({ peer }) => peer.peerId);
    syncSelectorPeers(this.peerSelector, currentPeerIds);

    const connected = connectedAll
      .filter(({ peer }) => !excludePeerId || peer.peerId !== excludePeerId);
    const order = this.peerSelector.selectPeers();
    const rank = new Map<string, number>(order.map((peerId, idx) => [peerId, idx]));

    connected.sort((a, b) => {
      if (a.pool === 'follows' && b.pool !== 'follows') return -1;
      if (a.pool !== 'follows' && b.pool === 'follows') return 1;
      return (rank.get(a.peer.peerId) ?? Number.MAX_SAFE_INTEGER) -
        (rank.get(b.peer.peerId) ?? Number.MAX_SAFE_INTEGER);
    });

    return connected.map(({ peer }) => peer);
  }

  private createInFlightPeerRequest(peer: Peer, hash: Hash, htl?: number): InFlightPeerRequest {
    const startedAt = Date.now();
    this.peerSelector.recordRequest(peer.peerId, 40);
    const promise = peer.request(hash, htl).then((data) => ({
      peerId: peer.peerId,
      data,
      elapsedMs: Math.max(1, Date.now() - startedAt),
    })).catch(() => ({
      peerId: peer.peerId,
      data: null,
      elapsedMs: Math.max(1, Date.now() - startedAt),
    }));
    return { peerId: peer.peerId, settled: false, promise };
  }

  private async waitForNextPeerResult(
    inFlight: InFlightPeerRequest[],
    waitMs: number,
  ): Promise<{ task: InFlightPeerRequest; data: Uint8Array | null; elapsedMs: number } | null> {
    const active = inFlight.filter((task) => !task.settled);
    if (active.length === 0 || waitMs <= 0) return null;
    const timeout = this.delay(waitMs).then(() => null);
    const outcome = await Promise.race([
      timeout,
      ...active.map((task) => task.promise.then((result) => ({
        task,
        data: result.data,
        elapsedMs: result.elapsedMs,
      }))),
    ]);
    if (!outcome) return null;
    outcome.task.settled = true;
    return outcome;
  }

  private async queryPeersWithDispatch(
    hash: Hash,
    orderedPeers: Peer[],
    triedPeers: Set<string>,
    htl?: number,
  ): Promise<Uint8Array | null> {
    if (orderedPeers.length === 0) return null;

    const dispatch = normalizeDispatchConfig(this.routing.dispatch, orderedPeers.length);
    const wavePlan = buildHedgedWavePlan(orderedPeers.length, dispatch);
    if (wavePlan.length === 0) return null;

    const deadline = Date.now() + this.config.requestTimeout;
    const inFlight: InFlightPeerRequest[] = [];
    let nextPeerIdx = 0;
    const expectedHashHex = toHex(hash);

    for (let waveIdx = 0; waveIdx < wavePlan.length; waveIdx++) {
      const waveSize = wavePlan[waveIdx];
      const from = nextPeerIdx;
      const to = Math.min(from + waveSize, orderedPeers.length);
      nextPeerIdx = to;

      for (const peer of orderedPeers.slice(from, to)) {
        triedPeers.add(peer.peerId);
        inFlight.push(this.createInFlightPeerRequest(peer, hash, htl));
      }

      const isLastWave = waveIdx === wavePlan.length - 1 || nextPeerIdx >= orderedPeers.length;
      const windowEnd = isLastWave
        ? deadline
        : Math.min(deadline, Date.now() + dispatch.hedgeIntervalMs);

      while (Date.now() < windowEnd) {
        const remaining = windowEnd - Date.now();
        const result = await this.waitForNextPeerResult(inFlight, remaining);
        if (!result) break;

        if (!result.data) {
          this.peerSelector.recordTimeout(result.task.peerId);
          continue;
        }

        const computedHash = await sha256(result.data);
        if (toHex(computedHash) !== expectedHashHex) {
          this.peerSelector.recordFailure(result.task.peerId);
          continue;
        }

        this.peerSelector.recordSuccess(result.task.peerId, result.elapsedMs, result.data.length);
        if (this.config.localStore) {
          await this.config.localStore.put(hash, result.data);
        }
        return result.data;
      }

      if (Date.now() >= deadline) break;
    }

    for (const task of inFlight) {
      if (!task.settled) this.peerSelector.recordTimeout(task.peerId);
    }
    return null;
  }

  /**
   * Forward a request to other peers (excluding the requester)
   * Called by Peer when it receives a request it can't fulfill locally
   * Uses selector ordering + staged hedged dispatch
   * @param htl - Hops To Live (already decremented by calling peer)
   */
  private async forwardRequest(hash: Uint8Array, excludePeerId: string, htl: number): Promise<Uint8Array | null> {
    const triedPeers = new Set<string>();
    const orderedPeers = this.orderedConnectedPeers(excludePeerId);
    return this.queryPeersWithDispatch(hash, orderedPeers, triedPeers, htl);
  }

  /**
   * Send data to all peers who have requested this hash
   * Called when we receive data that peers may be waiting for
   */
  sendToInterestedPeers(hash: Uint8Array, data: Uint8Array): number {
    let sendCount = 0;
    for (const { peer } of this.peers.values()) {
      if (peer.isConnected && peer.sendData(hash, data)) {
        sendCount++;
      }
    }
    if (sendCount > 0) {
      this.log('Sent data to', sendCount, 'interested peers for hash:', toHex(hash).slice(0, 16));
    }
    return sendCount;
  }

  private pruneSeenSet(seen: Map<string, number>, ttlMs: number, cap: number): void {
    const now = Date.now();
    for (const [key, ts] of seen) {
      if (now - ts >= ttlMs) {
        seen.delete(key);
      }
    }
    while (seen.size > cap) {
      const oldest = seen.keys().next().value as string | undefined;
      if (!oldest) break;
      seen.delete(oldest);
    }
  }

  private markSeenFrameId(frameId: string): boolean {
    this.pruneSeenSet(this.seenFrameIds, SEEN_FRAME_TTL_MS, SEEN_FRAME_CAP);
    if (this.seenFrameIds.has(frameId)) return false;
    this.seenFrameIds.set(frameId, Date.now());
    return true;
  }

  private markSeenEventId(eventId: string): boolean {
    this.pruneSeenSet(this.seenEventIds, SEEN_EVENT_TTL_MS, SEEN_EVENT_CAP);
    if (this.seenEventIds.has(eventId)) return false;
    this.seenEventIds.set(eventId, Date.now());
    return true;
  }

  private forwardMeshFrame(frame: MeshNostrFrame, excludePeerId?: string): number {
    let forwarded = 0;
    for (const { peer } of this.peers.values()) {
      if (!peer.isConnected) continue;
      if (excludePeerId && peer.peerId === excludePeerId) continue;

      const nextHtl = decrementHTLWithPolicy(frame.htl, MESH_EVENT_POLICY, peer.getHTLConfig());
      if (!shouldForwardHTL(nextHtl)) continue;

      const outbound: MeshNostrFrame = {
        ...frame,
        htl: nextHtl,
      };
      if (peer.sendMeshFrameText(outbound)) {
        forwarded++;
      }
    }
    return forwarded;
  }

  private async handleMeshFrame(fromPeerId: string, frame: MeshNostrFrame): Promise<void> {
    const validationError = validateMeshNostrFrame(frame);
    if (validationError) {
      return;
    }

    if (!this.markSeenFrameId(frame.frame_id)) {
      this.meshDroppedDuplicate++;
      return;
    }

    const event = frame.payload.event as Event;
    if (!this.markSeenEventId(event.id)) {
      this.meshDroppedDuplicate++;
      return;
    }

    if (!verifyEvent(event)) {
      return;
    }

    this.meshReceived++;
    await this.handleSignalingEvent(event);

    const forwarded = this.forwardMeshFrame(frame, fromPeerId);
    if (forwarded > 0) {
      this.meshForwarded += forwarded;
    }
  }

  private async dispatchSignaling(msg: SignalingMessage, recipientPubkey?: string): Promise<void> {
    const event = await this.sendSignaling(msg, recipientPubkey);
    if (!event) {
      return;
    }

    const frame = createMeshNostrEventFrame(
      event as SignedEvent,
      this.myPeerId.toString(),
      MESH_EVENT_POLICY.maxHtl,
    );
    if (!this.markSeenFrameId(frame.frame_id)) {
      this.meshDroppedDuplicate++;
      return;
    }
    if (!this.markSeenEventId(frame.payload.event.id)) {
      this.meshDroppedDuplicate++;
      return;
    }

    const forwarded = this.forwardMeshFrame(frame);
    if (forwarded > 0) {
      this.meshForwarded += forwarded;
    }
  }

  private async sendSignaling(
    msg: SignalingMessage,
    recipientPubkey?: string,
  ): Promise<Event | null> {
    // Fill in our peer ID
    if ('peerId' in msg && msg.peerId === '') {
      msg.peerId = this.myPeerId.toString();
    }

    if (recipientPubkey) {
      // Directed message (offer, answer, candidate, candidates)
      // Use NIP-17 style gift wrap with kind 25050
      const innerEvent = {
        kind: SIGNALING_KIND,
        content: JSON.stringify(msg),
        tags: [] as string[][],
      };

      const wrappedEvent = await this.giftWrap(innerEvent, recipientPubkey);
      await this.pool.publish(this.config.relays, wrappedEvent as Event);
      return wrappedEvent as Event;
    } else {
      // Hello message - broadcast for peer discovery (kind 25050 with #l: hello)
      const expiration = Math.floor((Date.now() + 5 * 60 * 1000) / 1000); // 5 minutes
      const tags = [
        ['l', HELLO_TAG],
        ['peerId', msg.peerId],
        [HASH_GET_TAG, msg.type === 'hello' && msg.hashGet === false ? '0' : '1'],
        ['expiration', expiration.toString()],
      ];

      const eventTemplate = {
        kind: SIGNALING_KIND,
        created_at: Math.floor(Date.now() / 1000),
        tags,
        content: '',
      };

      const event = await this.signer(eventTemplate) as Event;
      await this.pool.publish(this.config.relays, event);
      return event;
    }
  }

  /**
   * Force send a hello message (useful for testing after pool config changes)
   */
  sendHello(): void {
    if (!this.running) return;
    this.maybeSendHello();
  }

  private maybeSendHello(): void {
    if (!this.running) return;

    // Check if both pools are satisfied
    const followsSatisfied = this.isPoolSatisfied('follows');
    const otherSatisfied = this.isPoolSatisfied('other');

    if (followsSatisfied && otherSatisfied) {
      return;
    }

    void this.dispatchSignaling({
      type: 'hello',
      peerId: this.myPeerId.toString(),
      hashGet: true,
    });
  }

  private cleanupConnections(): void {
    const now = Date.now();
    const connectionTimeout = 15000; // 15 seconds to establish connection

    for (const [peerIdStr, { peer }] of this.peers) {
      const state = peer.state;
      const isStale = state === 'new' && (now - peer.createdAt) > connectionTimeout;

      if (state === 'failed' || state === 'closed' || state === 'disconnected' || isStale) {
        this.log('Cleaning up', state, 'connection', isStale ? '(stale)' : '');
        peer.close();
        this.peerSelector.removePeer(peerIdStr);
        this.peers.delete(peerIdStr);
        this.emit({ type: 'update' });
      }
    }
  }

  /**
   * Get number of connected peers
   */
  getConnectedCount(): number {
    return Array.from(this.peers.values())
      .filter(({ peer }) => peer.isConnected).length;
  }

  /**
   * Get all peer statuses
   */
  getPeers(): PeerStatus[] {
    return Array.from(this.peers.values()).map(({ peer, pool, hashGet }) => ({
      peerId: peer.peerId,
      pubkey: peer.pubkey,
      state: peer.state,
      direction: peer.direction,
      connectedAt: peer.connectedAt,
      isSelf: peer.pubkey === this.myPeerId.pubkey,
      pool,
      isConnected: peer.isConnected, // Includes data channel state
      hashGet,
    }));
  }

  /**
   * Disconnect all peers with a given pubkey
   * Used when blocking a peer to immediately disconnect them
   */
  disconnectPeerByPubkey(pubkey: string): void {
    for (const [peerIdStr, peerInfo] of this.peers) {
      if (peerInfo.peer.pubkey === pubkey) {
        this.log('Disconnecting blocked peer:', pubkey.slice(0, 8));
        peerInfo.peer.close();
        this.peerSelector.removePeer(peerIdStr);
        this.peers.delete(peerIdStr);
        this.emit({ type: 'peer-disconnected', peerId: peerIdStr });
      }
    }
    this.emit({ type: 'update' });
  }

  /**
   * Get my endpoint ID
   */
  getMyPeerId(): string {
    return this.myPeerId.toString();
  }

  /**
   * Check if store is satisfied (has enough connections in all pools)
   */
  isSatisfied(): boolean {
    return this.isPoolSatisfied('follows') && this.isPoolSatisfied('other');
  }

  /**
   * Update peer classifier (e.g., when social graph updates)
   */
  setPeerClassifier(classifier: PeerClassifier): void {
    this.peerClassifier = classifier;
    // Re-classify existing peers (they keep their connections, just update pool assignment)
    for (const [peerIdStr, peerInfo] of this.peers) {
      const newPool = classifier(peerInfo.peer.pubkey);
      if (newPool !== peerInfo.pool) {
        this.log('Reclassified peer', peerIdStr.slice(0, 16), 'from', peerInfo.pool, 'to', newPool);
        peerInfo.pool = newPool;
      }
    }
    this.emit({ type: 'update' });
  }

  /**
   * Update pool configuration (e.g., from settings)
   */
  setPoolConfig(pools: { follows: PoolConfig; other: PoolConfig }): void {
    const oldOtherMax = this.pools.other.maxConnections;
    const oldFollowsMax = this.pools.follows.maxConnections;
    this.pools = pools;
    this.log('Pool config updated:', pools);

    // Check if subscription mode needs to change
    const newOtherMax = pools.other.maxConnections;
    const newFollowsMax = pools.follows.maxConnections;
    const subscriptionModeChanged =
      (oldOtherMax === 0) !== (newOtherMax === 0) ||
      (oldFollowsMax === 0) !== (newFollowsMax === 0);

    if (subscriptionModeChanged && this.running) {
      const since = Math.floor((Date.now() - this.config.messageTimeout) / 1000);
      const subHandler = {
        onevent: (event: Event) => {
          this.handleSignalingEvent(event);
        },
        oneose: () => {},
      };
      this.setupHelloSubscription(since, subHandler);
    }

    // Existing connections remain, but new limits apply for future connections
    this.emit({ type: 'update' });
  }

  /**
   * Get current pool configuration
   */
  getPoolConfig(): { follows: PoolConfig; other: PoolConfig } {
    return { ...this.pools };
  }

  private async peerMetadataPointerHash(): Promise<Hash> {
    return sha256(new TextEncoder().encode(PEER_METADATA_POINTER_SLOT_KEY));
  }

  /**
   * Persist selector metadata snapshot to local store.
   * Returns the snapshot hash, or null when local storage is unavailable.
   */
  async persistPeerMetadata(): Promise<Uint8Array | null> {
    if (!this.config.localStore) return null;

    const snapshot = this.peerSelector.exportPeerMetadataSnapshot();
    const bytes = new TextEncoder().encode(JSON.stringify(snapshot));
    const snapshotHash = await sha256(bytes);
    await this.config.localStore.put(snapshotHash, bytes);

    const pointerHash = await this.peerMetadataPointerHash();
    await this.config.localStore.delete(pointerHash);
    await this.config.localStore.put(pointerHash, new TextEncoder().encode(toHex(snapshotHash)));
    return snapshotHash;
  }

  /**
   * Load selector metadata snapshot from local store, if available.
   */
  async loadPeerMetadata(): Promise<boolean> {
    if (!this.config.localStore) return false;

    const pointerHash = await this.peerMetadataPointerHash();
    const pointerBytes = await this.config.localStore.get(pointerHash);
    if (!pointerBytes) return false;

    const pointerHex = new TextDecoder().decode(pointerBytes).trim();
    if (pointerHex.length !== 64) return false;
    const snapshotHash = fromHex(pointerHex);
    if (snapshotHash.length !== 32) return false;

    const snapshotBytes = await this.config.localStore.get(snapshotHash);
    if (!snapshotBytes) return false;

    let snapshot: PeerMetadataSnapshot;
    try {
      snapshot = JSON.parse(new TextDecoder().decode(snapshotBytes)) as PeerMetadataSnapshot;
    } catch {
      return false;
    }

    this.peerSelector.importPeerMetadataSnapshot(snapshot);
    syncSelectorPeers(this.peerSelector, Array.from(this.peers.keys()));
    return true;
  }

  /**
   * Get fallback stores count
   */
  getFallbackStoresCount(): number {
    return this.config.fallbackStores.length;
  }

  /**
   * Get WebRTC stats (aggregate and per-peer)
   */
  getStats(): {
    aggregate: MeshStats;
    perPeer: Map<string, {
      pubkey: string;
      pool: PeerPool;
      stats: ReturnType<Peer['getStats']>;
    }>;
  } {
    // Aggregate stats from all peers + store-level stats
    const aggregate: MeshStats = {
      requestsSent: 0,
      requestsReceived: 0,
      responsesSent: 0,
      responsesReceived: 0,
      receiveErrors: 0,
      blossomFetches: this.blossomFetches,
      fragmentsSent: 0,
      fragmentsReceived: 0,
      fragmentTimeouts: 0,
      reassembliesCompleted: 0,
      bytesSent: 0,
      bytesReceived: 0,
      bytesForwarded: 0,
      meshReceived: this.meshReceived,
      meshForwarded: this.meshForwarded,
      meshDroppedDuplicate: this.meshDroppedDuplicate,
    };

    const perPeer = new Map<string, {
      pubkey: string;
      pool: PeerPool;
      stats: ReturnType<Peer['getStats']>;
    }>();

    for (const [peerIdStr, { peer, pool }] of this.peers) {
      const peerStats = peer.getStats();
      aggregate.requestsSent += peerStats.requestsSent;
      aggregate.requestsReceived += peerStats.requestsReceived;
      aggregate.responsesSent += peerStats.responsesSent;
      aggregate.responsesReceived += peerStats.responsesReceived;
      aggregate.receiveErrors += peerStats.receiveErrors;
      aggregate.fragmentsSent += peerStats.fragmentsSent;
      aggregate.fragmentsReceived += peerStats.fragmentsReceived;
      aggregate.fragmentTimeouts += peerStats.fragmentTimeouts;
      aggregate.reassembliesCompleted += peerStats.reassembliesCompleted;
      aggregate.bytesSent += peerStats.bytesSent;
      aggregate.bytesReceived += peerStats.bytesReceived;
      aggregate.bytesForwarded += peerStats.bytesForwarded;
      perPeer.set(peerIdStr, {
        pubkey: peer.pubkey,
        pool,
        stats: peerStats,
      });
    }

    return { aggregate, perPeer };
  }

  // Store interface implementation

  async put(hash: Hash, data: Uint8Array): Promise<boolean> {
    // Write to local store if available
    const success = this.config.localStore
      ? await this.config.localStore.put(hash, data)
      : false;

    // Send to any peers who have requested this hash
    this.sendToInterestedPeers(hash, data);

    // Fire-and-forget writes to fallback stores (e.g. blossom servers)
    // Don't await - let them complete in background
    for (const store of this.config.fallbackStores) {
      store.put(hash, data).catch(() => {
        // Silently ignore failures - fallback stores are best-effort
      });
    }

    return success;
  }

  async get(hash: Hash): Promise<Uint8Array | null> {
    // Guard against undefined hash
    if (!hash) return null;

    // Try local store first
    if (this.config.localStore) {
      const local = await this.config.localStore.get(hash);
      if (local) return local;
    }

    // Deduplicate: if there's already a pending request for this hash, wait for it
    const hashHex = toHex(hash);
    const pendingGet = this.pendingGets.get(hashHex);
    if (pendingGet) {
      return pendingGet;
    }

    // Create the actual fetch promise
    const fetchPromise = this.fetchFromPeers(hash);

    // Store it for deduplication
    this.pendingGets.set(hashHex, fetchPromise);

    // Clean up when done
    try {
      const result = await fetchPromise;
      return result;
    } finally {
      this.pendingGets.delete(hashHex);
    }
  }

  /**
   * Internal method to fetch data from peers (separated for deduplication)
   */
  private async fetchFromPeers(hash: Hash): Promise<Uint8Array | null> {
    const triedPeers = new Set<string>();
    const orderedPeers = this.orderedConnectedPeers();
    const webRtcData = await this.queryPeersWithDispatch(hash, orderedPeers, triedPeers);
    if (webRtcData) {
      return webRtcData;
    }

    // All WebRTC peers failed - try fallback stores in order
    if (this.config.fallbackStores.length > 0) {
      for (const store of this.config.fallbackStores) {
        try {
          const data = await store.get(hash);
          if (data) {
            this.blossomFetches++;
            if (this.config.localStore) {
              await this.config.localStore.put(hash, data);
            }
            return data;
          }
        } catch (e) {
          this.log('Fallback store error:', e);
        }
      }
    }

    // If running and either:
    // 1. Not satisfied (still seeking more peers), OR
    // 2. We haven't tried any peers yet (no peers connected, but might connect soon)
    // Then add to pending reqs and wait for new peers
    if (this.running && (!this.isSatisfied() || triedPeers.size === 0)) {
      return this.waitForHash(hash, triedPeers);
    }

    return null;
  }

  /**
   * Helper to create a delay promise
   */
  private delay(ms: number): Promise<void> {
    return new Promise(resolve => setTimeout(resolve, ms));
  }

  /**
   * Add hash to pending requests list and wait for it to be resolved by peers
   * Also immediately tries any connected peers that weren't tried yet
   */
  private waitForHash(hash: Hash, triedPeers: Set<string>): Promise<Uint8Array | null> {
    return new Promise((resolve) => {
      // Use longer timeout for pending requests - we need to wait for peers to connect
      const reqTimeout = Math.max(this.config.requestTimeout * 6, 30000);
      const timeout = setTimeout(() => {
        this.removePendingReq(hash, req);
        resolve(null);
      }, reqTimeout);

      const req: PendingReq = { resolve, timeout, triedPeers };

      const existing = this.pendingReqs.get(hash);
      if (existing) {
        existing.push(req);
      } else {
        this.pendingReqs.set(hash, [req]);
      }

      this.log('Added to pending reqs:', hash.slice(0, 16), 'tried', triedPeers.size, 'peers');

      // Immediately try any connected peers that weren't tried yet
      // This handles the race condition where peers connect while we're setting up the request
      this.tryConnectedPeersForHash(hash);
    });
  }

  /**
   * Try all currently connected peers for a specific hash in the pending requests
   */
  private async tryConnectedPeersForHash(hash: Hash): Promise<void> {
    const reqs = this.pendingReqs.get(hash);
    if (!reqs || reqs.length === 0) return;

    const connectedPeers = this.orderedConnectedPeers();
    const expectedHashHex = toHex(hash);

    for (const peer of connectedPeers) {
      const peerIdStr = peer.peerId;

      // Find requests that haven't tried this peer yet
      const untried = reqs.filter(r => !r.triedPeers.has(peerIdStr));
      if (untried.length === 0) continue;

      // Mark as tried
      for (const r of untried) {
        r.triedPeers.add(peerIdStr);
      }

      this.log('Trying pending req from connected peer:', hash.slice(0, 16));

      const startedAt = Date.now();
      this.peerSelector.recordRequest(peerIdStr, 40);
      const data = await peer.request(hash);
      if (data) {
        const computedHash = await sha256(data);
        if (toHex(computedHash) !== expectedHashHex) {
          this.peerSelector.recordFailure(peerIdStr);
          continue;
        }
        this.peerSelector.recordSuccess(peerIdStr, Math.max(1, Date.now() - startedAt), data.length);
        // Store locally
        if (this.config.localStore) {
          await this.config.localStore.put(hash, data);
        }

        // Resolve all waiting requests
        const currentReqs = this.pendingReqs.get(hash);
        if (currentReqs) {
          for (const r of currentReqs) {
            clearTimeout(r.timeout);
            r.resolve(data);
          }
          this.pendingReqs.delete(hash);
        }

        this.log('Resolved pending req:', hash.slice(0, 16));
        return;
      }

      this.peerSelector.recordTimeout(peerIdStr);
    }
  }

  /**
   * Remove a pending request from the list
   */
  private removePendingReq(hash: Hash, req: PendingReq): void {
    const reqs = this.pendingReqs.get(hash);
    if (!reqs) return;

    const idx = reqs.indexOf(req);
    if (idx !== -1) {
      reqs.splice(idx, 1);
      if (reqs.length === 0) {
        this.pendingReqs.delete(hash);
      }
    }
  }

  /**
   * Try pending requests with a newly connected peer
   */
  private async tryPendingReqs(peer: Peer): Promise<void> {
    const peerIdStr = peer.peerId;
    const expectedByHash = new Map<string, string>();

    for (const [hash, reqs] of this.pendingReqs.entries()) {
      const hashKey = toHex(hash);
      expectedByHash.set(hashKey, hashKey);
      // Find requests that haven't tried this peer yet
      const untried = reqs.filter(r => !r.triedPeers.has(peerIdStr));
      if (untried.length === 0) continue;

      // Mark as tried
      for (const r of untried) {
        r.triedPeers.add(peerIdStr);
      }


      const startedAt = Date.now();
      this.peerSelector.recordRequest(peerIdStr, 40);
      const data = await peer.request(hash);
      if (data) {
        const computedHash = await sha256(data);
        const expectedHashHex = expectedByHash.get(hashKey)!;
        if (toHex(computedHash) !== expectedHashHex) {
          this.peerSelector.recordFailure(peerIdStr);
          continue;
        }
        this.peerSelector.recordSuccess(peerIdStr, Math.max(1, Date.now() - startedAt), data.length);
        // Store locally
        if (this.config.localStore) {
          await this.config.localStore.put(hash, data);
        }

        // Resolve all waiting requests
        for (const r of reqs) {
          clearTimeout(r.timeout);
          r.resolve(data);
        }
        this.pendingReqs.delete(hash);

        this.log('Resolved pending req:', hash.slice(0, 16));
      } else {
        this.peerSelector.recordTimeout(peerIdStr);
      }
    }
  }

  async has(hash: Hash): Promise<boolean> {
    // Check local store
    if (this.config.localStore) {
      const hasLocal = await this.config.localStore.has(hash);
      if (hasLocal) return true;
    }

    // Could query peers, but for now just check locally
    return false;
  }

  async delete(hash: Hash): Promise<boolean> {
    // Only delete from local store
    if (this.config.localStore) {
      return this.config.localStore.delete(hash);
    }
    return false;
  }
}
