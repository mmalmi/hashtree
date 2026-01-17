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
import { SimplePool, type Event } from 'nostr-tools';
import type { Store, Hash } from '../types.js';
import { toHex } from '../types.js';
import {
  PeerId,
  generateUuid,
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
  type WebRTCStats,
} from './types.js';
import { Peer } from './peer.js';

export const DEFAULT_RELAYS = [
  'wss://relay.damus.io',
  'wss://relay.primal.net',
  'wss://nos.lol',
  'wss://relay.nostr.band',
  'wss://temp.iris.to',
  'wss://relay.snort.social',
];

// All WebRTC signaling uses ephemeral kind 25050
// Hello messages use #l tag for broadcast discovery
// Directed messages use #p tag with gift wrap
const SIGNALING_KIND = 25050;
const HELLO_TAG = 'hello';


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

  constructor(config: WebRTCStoreConfig) {
    this.signer = config.signer;
    this.encrypt = config.encrypt;
    this.decrypt = config.decrypt;
    this.giftWrap = config.giftWrap;
    this.giftUnwrap = config.giftUnwrap;
    this.myPeerId = new PeerId(config.pubkey, generateUuid());

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
      helloInterval: config.helloInterval ?? 10000,
      messageTimeout: config.messageTimeout ?? 60000, // 60 seconds for relay propagation
      requestTimeout: config.requestTimeout ?? 500,
      peerQueryDelay: config.peerQueryDelay ?? 500,
      relays: config.relays ?? DEFAULT_RELAYS,
      localStore: config.localStore ?? null,
      fallbackStores: config.fallbackStores ?? [],
      debug: config.debug ?? false,
    };

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
    for (const { peer } of this.peers.values()) {
      peer.close();
    }
    this.peers.clear();
    this.pendingOtherPubkeys.clear();
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
      for (const { peer } of this.peers.values()) {
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
      const peerIdTag = event.tags.find(t => t[0] === 'peerId')?.[1];
      if (peerIdTag) {
        await this.handleHello(peerIdTag, event.pubkey);
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
    // Directed message - check if it's for us
    if (msg.targetPeerId !== this.myPeerId.toString()) {
      return;
    }

    const peerId = new PeerId(senderPubkey, msg.peerId);
    const peerIdStr = peerId.toString();

    if (msg.type === 'offer') {
      await this.handleOffer(peerId, msg);
    } else {
      // answer or candidate
      const peerInfo = this.peers.get(peerIdStr);
      if (peerInfo) {
        await peerInfo.peer.handleSignaling(msg);
      }
    }
  }

  private async handleHello(peerUuid: string, senderPubkey: string): Promise<void> {
    const peerId = new PeerId(senderPubkey, peerUuid);

    // Skip self (exact same peerId = same session)
    if (peerId.toString() === this.myPeerId.toString()) {
      return;
    }

    // Skip blocked peers
    if (this.isPeerBlocked?.(senderPubkey)) {
      this.log('Ignoring hello from blocked peer:', senderPubkey.slice(0, 8));
      return;
    }

    // Check if we already have this peer
    if (this.peers.has(peerId.toString())) {
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
      await this.connectToPeer(peerId, pool);
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
    if (existing) {
      existing.peer.close();
      this.peers.delete(peerIdStr);
    }

    const peer = new Peer({
      peerId,
      myPeerId: this.myPeerId.uuid,
      direction: 'inbound',
      localStore: this.config.localStore,
      sendSignaling: (m) => this.sendSignaling(m, peerId.pubkey),
      onClose: () => this.handlePeerClose(peerIdStr),
      onConnected: () => {
        this.emit({ type: 'peer-connected', peerId: peerIdStr });
        this.emit({ type: 'update' });
        this.tryPendingReqs(peer);
      },
      onForwardRequest: (hash, exclude, htl) => this.forwardRequest(hash, exclude, htl),
      requestTimeout: this.config.requestTimeout,
      debug: this.config.debug,
    });

    this.peers.set(peerIdStr, { peer, pool });
    // Clear pending now that peer is in the map
    this.pendingOtherPubkeys.delete(peerId.pubkey);
    await peer.handleSignaling(msg);
  }

  private async connectToPeer(peerId: PeerId, pool: PeerPool): Promise<void> {
    const peerIdStr = peerId.toString();

    if (this.peers.has(peerIdStr)) {
      return;
    }

    this.log('Initiating connection to', peerId.short(), 'pool:', pool);

    const peer = new Peer({
      peerId,
      myPeerId: this.myPeerId.uuid,
      direction: 'outbound',
      localStore: this.config.localStore,
      sendSignaling: (m) => this.sendSignaling(m, peerId.pubkey),
      onClose: () => this.handlePeerClose(peerIdStr),
      onConnected: () => {
        this.emit({ type: 'peer-connected', peerId: peerIdStr });
        this.emit({ type: 'update' });
        this.tryPendingReqs(peer);
      },
      onForwardRequest: (hash, exclude, htl) => this.forwardRequest(hash, exclude, htl),
      requestTimeout: this.config.requestTimeout,
      debug: this.config.debug,
    });

    this.peers.set(peerIdStr, { peer, pool });
    await peer.connect();
  }

  private handlePeerClose(peerIdStr: string): void {
    this.peers.delete(peerIdStr);
    this.emit({ type: 'peer-disconnected', peerId: peerIdStr });
    this.emit({ type: 'update' });
  }

  /**
   * Forward a request to other peers (excluding the requester)
   * Called by Peer when it receives a request it can't fulfill locally
   * Uses sequential queries with delays between attempts
   * @param htl - Hops To Live (already decremented by calling peer)
   */
  private async forwardRequest(hash: Uint8Array, excludePeerId: string, htl: number): Promise<Uint8Array | null> {
    // Try all connected peers except the one who requested
    const otherPeers = Array.from(this.peers.values())
      .filter(({ peer }) => peer.isConnected && peer.peerId !== excludePeerId);

    // Sort: follows first
    otherPeers.sort((a, b) => {
      if (a.pool === 'follows' && b.pool !== 'follows') return -1;
      if (a.pool !== 'follows' && b.pool === 'follows') return 1;
      return 0;
    });

    // Query peers sequentially with delay between attempts
    for (let i = 0; i < otherPeers.length; i++) {
      const { peer } = otherPeers[i];

      // Start request to this peer with the decremented HTL
      const requestPromise = peer.request(hash, htl);

      // Race between request completing and delay timeout
      const result = await Promise.race([
        requestPromise.then(data => ({ type: 'data' as const, data })),
        this.delay(this.config.peerQueryDelay).then(() => ({ type: 'timeout' as const })),
      ]);

      if (result.type === 'data' && result.data) {
        // Got data from this peer
        if (this.config.localStore) {
          await this.config.localStore.put(hash, result.data);
        }
        return result.data;
      }

      // If timeout, continue to next peer
      if (result.type === 'timeout') {
        this.log('Forward: peer', peer.peerId.slice(0, 12), 'timeout, trying next');
      }
    }

    return null;
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

  private async sendSignaling(msg: SignalingMessage, recipientPubkey?: string): Promise<void> {
    // Fill in our peer ID
    if ('peerId' in msg && msg.peerId === '') {
      msg.peerId = this.myPeerId.uuid;
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
    } else {
      // Hello message - broadcast for peer discovery (kind 25050 with #l: hello)
      const expiration = Math.floor((Date.now() + 5 * 60 * 1000) / 1000); // 5 minutes
      const tags = [
        ['l', HELLO_TAG],
        ['peerId', msg.peerId],
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

    this.sendSignaling({
      type: 'hello',
      peerId: this.myPeerId.uuid,
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
    return Array.from(this.peers.values()).map(({ peer, pool }) => ({
      peerId: peer.peerId,
      pubkey: peer.pubkey,
      state: peer.state,
      direction: peer.direction,
      connectedAt: peer.connectedAt,
      isSelf: peer.pubkey === this.myPeerId.pubkey,
      pool,
      isConnected: peer.isConnected, // Includes data channel state
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
        this.peers.delete(peerIdStr);
        this.emit({ type: 'peer-disconnected', peerId: peerIdStr });
      }
    }
    this.emit({ type: 'update' });
  }

  /**
   * Get my peer ID (uuid part only)
   */
  getMyPeerId(): string {
    return this.myPeerId.uuid;
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
    aggregate: WebRTCStats;
    perPeer: Map<string, {
      pubkey: string;
      pool: PeerPool;
      stats: ReturnType<Peer['getStats']>;
    }>;
  } {
    // Aggregate stats from all peers + store-level stats
    const aggregate: WebRTCStats = {
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
    // Get currently connected peers (prioritize follows pool)
    const triedPeers = new Set<string>();
    const allPeers = Array.from(this.peers.values())
      .filter(({ peer }) => peer.isConnected);

    // Sort: follows first, then others
    allPeers.sort((a, b) => {
      if (a.pool === 'follows' && b.pool !== 'follows') return -1;
      if (a.pool !== 'follows' && b.pool === 'follows') return 1;
      return 0;
    });

    // Query peers sequentially with delay between attempts
    for (let i = 0; i < allPeers.length; i++) {
      const { peer } = allPeers[i];
      triedPeers.add(peer.peerId);

      // Start request to this peer
      const requestPromise = peer.request(hash);

      // Race between request completing and delay timeout
      // If request completes within delay, we're done
      // If delay passes first, start next peer while still waiting
      const result = await Promise.race([
        requestPromise.then(data => ({ type: 'data' as const, data })),
        this.delay(this.config.peerQueryDelay).then(() => ({ type: 'timeout' as const })),
      ]);

      if (result.type === 'data' && result.data) {
        // Got data from this peer
        if (this.config.localStore) {
          await this.config.localStore.put(hash, result.data);
        }
        return result.data;
      }

      // If timeout, continue to next peer but also await the original request
      // in case it eventually returns data
      if (result.type === 'timeout') {
        // Fire-and-forget: if this peer eventually responds, we'll miss it
        // but that's fine - we're trying the next peer
        this.log('Peer', peer.peerId.slice(0, 12), 'timeout after', this.config.peerQueryDelay, 'ms, trying next');
      }
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

    // Get all connected peers
    const connectedPeers = Array.from(this.peers.values())
      .filter(({ peer }) => peer.isConnected)
      .map(({ peer }) => peer);

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

      const data = await peer.request(hash);
      if (data) {
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

    for (const [hash, reqs] of this.pendingReqs.entries()) {
      // Find requests that haven't tried this peer yet
      const untried = reqs.filter(r => !r.triedPeers.has(peerIdStr));
      if (untried.length === 0) continue;

      // Mark as tried
      for (const r of untried) {
        r.triedPeers.add(peerIdStr);
      }

  
      const data = await peer.request(hash);
      if (data) {
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
