/**
 * Worker WebRTC Controller
 *
 * Controls WebRTC connections from the worker thread.
 * Main thread proxy executes RTCPeerConnection operations.
 *
 * Worker owns:
 * - Peer state tracking
 * - Connection lifecycle decisions
 * - Data protocol (request/response)
 * - Signaling message handling
 *
 * Main thread proxy owns:
 * - RTCPeerConnection instances (not available in workers)
 * - Data channel I/O
 */

import type { Store } from '../../../hashtree/src/types';
import type { WebRTCCommand, WebRTCEvent } from './protocol.js';
import type { SignalingMessage, PeerPool, DataRequest, DataResponse } from '../../../hashtree/src/webrtc/types';
import {
  MAX_HTL,
  MSG_TYPE_REQUEST,
  MSG_TYPE_RESPONSE,
  FRAGMENT_SIZE,
  PeerId,
  generateUuid,
} from '../../../hashtree/src/webrtc/types';
import {
  encodeRequest,
  encodeResponse,
  parseMessage,
  createRequest,
  createResponse,
  createFragmentResponse,
  hashToKey,
  verifyHash,
  generatePeerHTLConfig,
  decrementHTL,
  shouldForward,
  type PeerHTLConfig,
  type PendingRequest,
} from '../../../hashtree/src/webrtc/protocol';
import { LRUCache } from '../utils/lruCache';

// ============================================================================
// Types
// ============================================================================

interface WorkerPeer {
  peerId: string;
  pubkey: string;
  pool: PeerPool;
  direction: 'inbound' | 'outbound';
  state: 'connecting' | 'connected' | 'disconnected';
  dataChannelReady: boolean;
  answerCreated: boolean;  // Track if we've already created an answer (inbound only)
  htlConfig: PeerHTLConfig;
  pendingRequests: Map<string, PendingRequest>;
  theirRequests: LRUCache<string, { hash: Uint8Array; requestedAt: number }>;
  stats: PeerStats;
  createdAt: number;
  connectedAt?: number;
  // Backpressure state
  bufferPaused: boolean;
  deferredRequests: DataRequest[];
}

interface PeerStats {
  requestsSent: number;
  requestsReceived: number;
  responsesSent: number;
  responsesReceived: number;
  bytesSent: number;
  bytesReceived: number;
}

export interface WebRTCControllerConfig {
  pubkey: string;
  localStore: Store;
  sendCommand: (cmd: WebRTCCommand) => void;
  sendSignaling: (msg: SignalingMessage, recipientPubkey?: string) => Promise<void>;
  getFollows?: () => Set<string>;
  requestTimeout?: number;
  debug?: boolean;
}

type PeerClassifier = (pubkey: string) => PeerPool;

// ============================================================================
// Controller
// ============================================================================

export class WebRTCController {
  private myPeerId: PeerId;
  private peers = new Map<string, WorkerPeer>();
  private localStore: Store;
  private sendCommand: (cmd: WebRTCCommand) => void;
  private sendSignaling: (msg: SignalingMessage, recipientPubkey?: string) => Promise<void>;
  private classifyPeer: PeerClassifier;
  private requestTimeout: number;
  private debug: boolean;
  private recentRequests = new LRUCache<string, number>(1000);

  // Pool configuration - reasonable defaults, settings sync will override
  private poolConfig = {
    follows: { maxConnections: 20, satisfiedConnections: 10 },
    other: { maxConnections: 16, satisfiedConnections: 8 },
  };

  // Hello interval - 5s for faster peer discovery
  private helloInterval?: ReturnType<typeof setInterval>;
  private readonly HELLO_INTERVAL = 5000;

  constructor(config: WebRTCControllerConfig) {
    this.myPeerId = new PeerId(config.pubkey, generateUuid());
    this.localStore = config.localStore;
    this.sendCommand = config.sendCommand;
    this.sendSignaling = config.sendSignaling;
    this.requestTimeout = config.requestTimeout ?? 1000;
    this.debug = config.debug ?? false;

    // Default classifier: check if pubkey is in follows
    const getFollows = config.getFollows ?? (() => new Set<string>());
    this.classifyPeer = (pubkey: string) => {
      const follows = getFollows();
      const isFollow = follows.has(pubkey);
      return isFollow ? 'follows' : 'other';
    };
  }

  // ============================================================================
  // Lifecycle
  // ============================================================================

  start(): void {
    this.log('Starting WebRTC controller');

    // Send hello periodically
    this.helloInterval = setInterval(() => {
      this.sendHello();
    }, this.HELLO_INTERVAL);

    // Send initial hello
    this.sendHello();
  }

  stop(): void {
    this.log('Stopping WebRTC controller');

    if (this.helloInterval) {
      clearInterval(this.helloInterval);
      this.helloInterval = undefined;
    }

    // Close all peers
    for (const peerId of this.peers.keys()) {
      this.closePeer(peerId);
    }
  }

  // ============================================================================
  // Signaling
  // ============================================================================

  private sendHello(): void {
    const msg: SignalingMessage = {
      type: 'hello',
      peerId: this.myPeerId.uuid,
    };
    this.sendSignaling(msg).catch(err => {
      console.error('[WebRTC] sendSignaling error:', err);
    });
  }

  /**
   * Public method to trigger a hello broadcast.
   * Used for testing to force peer discovery after follows are set up.
   */
  broadcastHello(): void {
    this.sendHello();
  }

  /**
   * Handle incoming signaling message (from Nostr kind 25050)
   *
   * Note: For hello messages, msg.peerId is just the UUID (from Nostr tag).
   * For directed messages (offer/answer/candidate), msg.peerId is already the full
   * pubkey:uuid format from Rust's SignalingMessage.
   */
  async handleSignalingMessage(msg: SignalingMessage, senderPubkey: string): Promise<void> {
    this.log(`Signaling from ${senderPubkey.slice(0, 8)}:`, msg.type);

    switch (msg.type) {
      case 'hello':
        // For hello, msg.peerId is just UUID - handleHello constructs the full peer ID
        {
          const senderUuid = msg.peerId.includes(':') ? msg.peerId.split(':').slice(-1)[0] : msg.peerId;
          await this.handleHello(senderPubkey, senderUuid);
        }
        break;

      case 'offer':
        // For directed messages, msg.peerId is already full pubkey:uuid format
        if (msg.peerId === this.myPeerId.toString()) {
          return; // Skip messages from ourselves
        }
        if (this.isMessageForUs(msg)) {
          // Construct RTCSessionDescriptionInit from flat sdp field
          await this.handleOffer(msg.peerId, senderPubkey, { type: 'offer', sdp: msg.sdp });
        }
        break;

      case 'answer':
        if (msg.peerId === this.myPeerId.toString()) {
          return;
        }
        if (this.isMessageForUs(msg)) {
          // Construct RTCSessionDescriptionInit from flat sdp field
          await this.handleAnswer(msg.peerId, { type: 'answer', sdp: msg.sdp });
        }
        break;

      case 'candidate':
        if (msg.peerId === this.myPeerId.toString()) {
          return;
        }
        if (this.isMessageForUs(msg)) {
          // Construct RTCIceCandidateInit from flat fields
          await this.handleIceCandidate(msg.peerId, {
            candidate: msg.candidate,
            sdpMLineIndex: msg.sdpMLineIndex,
            sdpMid: msg.sdpMid,
          });
        }
        break;

      case 'candidates':
        if (msg.peerId === this.myPeerId.toString()) {
          return;
        }
        if (this.isMessageForUs(msg)) {
          for (const c of msg.candidates) {
            await this.handleIceCandidate(msg.peerId, {
              candidate: c.candidate,
              sdpMLineIndex: c.sdpMLineIndex,
              sdpMid: c.sdpMid,
            });
          }
        }
        break;
    }
  }

  private isMessageForUs(msg: SignalingMessage): boolean {
    if ('targetPeerId' in msg && msg.targetPeerId) {
      return msg.targetPeerId === this.myPeerId.toString();
    }
    return true;
  }

  private async handleHello(senderPubkey: string, senderUuid: string): Promise<void> {
    const peerId = `${senderPubkey}:${senderUuid}`;

    // Already connected?
    if (this.peers.has(peerId)) {
      return;
    }

    // Check pool limits
    const pool = this.classifyPeer(senderPubkey);
    if (!this.shouldConnect(pool)) {
      this.log(`Pool ${pool} at capacity, ignoring hello`);
      return;
    }

    // In 'other' pool, only allow 1 connection per pubkey
    if (pool === 'other' && this.hasOtherPoolPubkey(senderPubkey)) {
      this.log(`Already have connection from ${senderPubkey.slice(0, 8)} in other pool`);
      return;
    }

    // Tie-breaking: lower UUID initiates
    const shouldInitiate = this.myPeerId.uuid < senderUuid;
    if (shouldInitiate) {
      this.log(`Initiating connection to ${peerId.slice(0, 20)}`);
      await this.createOutboundPeer(peerId, senderPubkey, pool);
    } else {
      this.log(`Waiting for offer from ${peerId.slice(0, 20)}`);
    }
  }

  private async handleOffer(peerId: string, pubkey: string, offer: RTCSessionDescriptionInit): Promise<void> {
    this.log(`handleOffer from ${pubkey.slice(0, 8)}, peerId: ${peerId.slice(0, 20)}`);
    let peer = this.peers.get(peerId);
    if (!peer) {
      const pool = this.classifyPeer(pubkey);
      if (!this.shouldConnect(pool)) {
        this.log(`Pool ${pool} at capacity, rejecting offer`);
        return;
      }
      if (pool === 'other' && this.hasOtherPoolPubkey(pubkey)) {
        this.log(`Already have connection from ${pubkey.slice(0, 8)} in other pool, rejecting offer`);
        return;
      }
      this.log(`Creating inbound peer for ${pubkey.slice(0, 8)}`);
      peer = this.createPeer(peerId, pubkey, pool, 'inbound');
    }

    this.log(`Setting remote description for ${peerId.slice(0, 20)}`);
    this.sendCommand({ type: 'rtc:setRemoteDescription', peerId, sdp: offer });
  }

  private async handleAnswer(peerId: string, answer: RTCSessionDescriptionInit): Promise<void> {
    const peer = this.peers.get(peerId);
    if (!peer) {
      this.log(`Answer for unknown peer: ${peerId}`);
      return;
    }

    this.sendCommand({ type: 'rtc:setRemoteDescription', peerId, sdp: answer });
  }

  private async handleIceCandidate(peerId: string, candidate: RTCIceCandidateInit): Promise<void> {
    const peer = this.peers.get(peerId);
    if (!peer) {
      return;
    }

    this.sendCommand({ type: 'rtc:addIceCandidate', peerId, candidate });
  }

  // ============================================================================
  // Peer Management
  // ============================================================================

  private shouldConnect(pool: PeerPool): boolean {
    const config = this.poolConfig[pool];
    const count = this.getPoolCount(pool);
    return count < config.maxConnections;
  }

  private getPoolCount(pool: PeerPool): number {
    let count = 0;
    for (const peer of this.peers.values()) {
      if (peer.pool === pool && peer.state !== 'disconnected') {
        count++;
      }
    }
    return count;
  }

  /**
   * Check if we already have a connection from this pubkey in the 'other' pool.
   * In the 'other' pool, we only allow 1 connection per pubkey to prevent spam.
   */
  private hasOtherPoolPubkey(pubkey: string): boolean {
    for (const peer of this.peers.values()) {
      if (peer.pool === 'other' && peer.pubkey === pubkey && peer.state !== 'disconnected') {
        return true;
      }
    }
    return false;
  }

  private createPeer(peerId: string, pubkey: string, pool: PeerPool, direction: 'inbound' | 'outbound'): WorkerPeer {
    const peer: WorkerPeer = {
      peerId,
      pubkey,
      pool,
      direction,
      state: 'connecting',
      dataChannelReady: false,
      answerCreated: false,
      htlConfig: generatePeerHTLConfig(),
      pendingRequests: new Map(),
      theirRequests: new LRUCache(200),
      stats: {
        requestsSent: 0,
        requestsReceived: 0,
        responsesSent: 0,
        responsesReceived: 0,
        bytesSent: 0,
        bytesReceived: 0,
      },
      createdAt: Date.now(),
      bufferPaused: false,
      deferredRequests: [],
    };

    this.peers.set(peerId, peer);
    this.sendCommand({ type: 'rtc:createPeer', peerId, pubkey });

    return peer;
  }

  private async createOutboundPeer(peerId: string, pubkey: string, pool: PeerPool): Promise<void> {
    this.createPeer(peerId, pubkey, pool, 'outbound');
    // Proxy will create peer and we'll get rtc:peerCreated, then request offer
  }

  private closePeer(peerId: string): void {
    const peer = this.peers.get(peerId);
    if (!peer) return;

    // Clear pending requests
    for (const pending of peer.pendingRequests.values()) {
      clearTimeout(pending.timeout);
      pending.resolve(null);
    }

    peer.state = 'disconnected';
    this.sendCommand({ type: 'rtc:closePeer', peerId });
    this.peers.delete(peerId);

    this.log(`Closed peer: ${peerId.slice(0, 20)}`);
  }

  // ============================================================================
  // Proxy Events
  // ============================================================================

  /**
   * Handle event from main thread proxy
   */
  handleProxyEvent(event: WebRTCEvent): void {
    switch (event.type) {
      case 'rtc:peerCreated':
        this.onPeerCreated(event.peerId);
        break;

      case 'rtc:peerStateChange':
        this.onPeerStateChange(event.peerId, event.state);
        break;

      case 'rtc:peerClosed':
        this.onPeerClosed(event.peerId);
        break;

      case 'rtc:offerCreated':
        this.onOfferCreated(event.peerId, event.sdp);
        break;

      case 'rtc:answerCreated':
        this.onAnswerCreated(event.peerId, event.sdp);
        break;

      case 'rtc:descriptionSet':
        this.onDescriptionSet(event.peerId, event.error);
        break;

      case 'rtc:iceCandidate':
        this.onIceCandidate(event.peerId, event.candidate);
        break;

      case 'rtc:dataChannelOpen':
        this.onDataChannelOpen(event.peerId);
        break;

      case 'rtc:dataChannelMessage':
        this.onDataChannelMessage(event.peerId, event.data);
        break;

      case 'rtc:dataChannelClose':
        this.onDataChannelClose(event.peerId);
        break;

      case 'rtc:dataChannelError':
        this.onDataChannelError(event.peerId, event.error);
        break;

      case 'rtc:bufferHigh':
        this.onBufferHigh(event.peerId);
        break;

      case 'rtc:bufferLow':
        this.onBufferLow(event.peerId);
        break;
    }
  }

  private onPeerCreated(peerId: string): void {
    const peer = this.peers.get(peerId);
    if (!peer) return;

    // If outbound, create offer
    if (peer.direction === 'outbound') {
      this.sendCommand({ type: 'rtc:createOffer', peerId });
    }
  }

  private onPeerStateChange(peerId: string, state: RTCPeerConnectionState): void {
    const peer = this.peers.get(peerId);
    if (!peer) return;

    this.log(`Peer ${peerId.slice(0, 20)} state: ${state}`);

    if (state === 'connected') {
      peer.state = 'connected';
      peer.connectedAt = Date.now();
    } else if (state === 'failed' || state === 'closed') {
      this.closePeer(peerId);
    }
  }

  private onPeerClosed(peerId: string): void {
    this.peers.delete(peerId);
  }

  private onOfferCreated(peerId: string, sdp: RTCSessionDescriptionInit): void {
    const peer = this.peers.get(peerId);
    if (!peer) return;

    // Set local description
    this.sendCommand({ type: 'rtc:setLocalDescription', peerId, sdp });

    // Send offer via signaling (use Rust-compatible format: full pubkey:uuid for peerId)
    const msg: SignalingMessage = {
      type: 'offer',
      sdp: sdp.sdp!,
      targetPeerId: peerId,
      peerId: this.myPeerId.toString(),  // Full pubkey:uuid
    };
    this.sendSignaling(msg, peer.pubkey);
  }

  private onAnswerCreated(peerId: string, sdp: RTCSessionDescriptionInit): void {
    this.log(`onAnswerCreated for ${peerId.slice(0, 20)}`);
    const peer = this.peers.get(peerId);
    if (!peer) {
      this.log(`onAnswerCreated: peer not found for ${peerId.slice(0, 20)}`);
      return;
    }

    this.sendCommand({ type: 'rtc:setLocalDescription', peerId, sdp });

    this.log(`Sending answer to ${peer.pubkey.slice(0, 8)}`);
    const msg: SignalingMessage = {
      type: 'answer',
      sdp: sdp.sdp!,
      targetPeerId: peerId,
      peerId: this.myPeerId.toString(),
    };
    this.sendSignaling(msg, peer.pubkey);
  }

  private onDescriptionSet(peerId: string, error?: string): void {
    if (error) {
      this.log(`Description set error for ${peerId.slice(0, 20)}: ${error}`);
      return;
    }

    const peer = this.peers.get(peerId);
    if (!peer) {
      this.log(`onDescriptionSet: peer not found for ${peerId.slice(0, 20)}`);
      return;
    }

    this.log(`onDescriptionSet for ${peerId.slice(0, 20)}: direction=${peer.direction}, state=${peer.state}, answerCreated=${peer.answerCreated}`);

    if (peer.direction === 'inbound' && peer.state === 'connecting' && !peer.answerCreated) {
      peer.answerCreated = true;
      this.log(`Creating answer for ${peerId.slice(0, 20)}`);
      this.sendCommand({ type: 'rtc:createAnswer', peerId });
    }
  }

  private onIceCandidate(peerId: string, candidate: RTCIceCandidateInit | null): void {
    if (!candidate || !candidate.candidate) return;

    const peer = this.peers.get(peerId);
    if (!peer) return;

    // Send candidate via signaling (use Rust-compatible format: full pubkey:uuid for peerId)
    const msg: SignalingMessage = {
      type: 'candidate',
      candidate: candidate.candidate,
      sdpMLineIndex: candidate.sdpMLineIndex ?? undefined,
      sdpMid: candidate.sdpMid ?? undefined,
      targetPeerId: peerId,
      peerId: this.myPeerId.toString(),  // Full pubkey:uuid
    };
    this.sendSignaling(msg, peer.pubkey);
  }

  private onDataChannelOpen(peerId: string): void {
    const peer = this.peers.get(peerId);
    if (!peer) return;

    peer.dataChannelReady = true;
    this.log(`Data channel open: ${peerId.slice(0, 20)}`);
  }

  private onDataChannelClose(peerId: string): void {
    const peer = this.peers.get(peerId);
    if (!peer) return;

    peer.dataChannelReady = false;
    this.closePeer(peerId);
  }

  private onDataChannelError(peerId: string, error: string): void {
    this.log(`Data channel error for ${peerId}: ${error}`);
  }

  private onBufferHigh(peerId: string): void {
    const peer = this.peers.get(peerId);
    if (!peer) return;

    peer.bufferPaused = true;
    this.log(`Buffer high for ${peerId.slice(0, 20)}, pausing responses`);
  }

  private onBufferLow(peerId: string): void {
    const peer = this.peers.get(peerId);
    if (!peer) return;

    peer.bufferPaused = false;
    this.log(`Buffer low for ${peerId.slice(0, 20)}, resuming responses`);

    // Process deferred requests
    this.processDeferredRequests(peer);
  }

  private async processDeferredRequests(peer: WorkerPeer): Promise<void> {
    while (!peer.bufferPaused && peer.deferredRequests.length > 0) {
      const req = peer.deferredRequests.shift()!;
      await this.processRequest(peer, req);
    }
  }

  // ============================================================================
  // Data Protocol
  // ============================================================================

  private async onDataChannelMessage(peerId: string, data: Uint8Array): Promise<void> {
    const peer = this.peers.get(peerId);
    if (!peer) return;

    const msg = parseMessage(data);
    if (!msg) {
      this.log(`Failed to parse message from ${peerId}`);
      return;
    }

    if (msg.type === MSG_TYPE_REQUEST) {
      await this.handleRequest(peer, msg.body);
    } else if (msg.type === MSG_TYPE_RESPONSE) {
      await this.handleResponse(peer, msg.body);
    }
  }

  private async handleRequest(peer: WorkerPeer, req: DataRequest): Promise<void> {
    peer.stats.requestsReceived++;

    // If buffer is full, defer the request for later processing
    if (peer.bufferPaused) {
      // Limit deferred requests to prevent memory issues
      if (peer.deferredRequests.length < 100) {
        peer.deferredRequests.push(req);
      }
      return;
    }

    await this.processRequest(peer, req);
  }

  private async processRequest(peer: WorkerPeer, req: DataRequest): Promise<void> {
    const hashKey = hashToKey(req.h);

    // Try to get from local store
    const data = await this.localStore.get(req.h);

    if (data) {
      // Send response
      await this.sendResponse(peer, req.h, data);
    } else {
      // Track their request for later push
      peer.theirRequests.set(hashKey, {
        hash: req.h,
        requestedAt: Date.now(),
      });

      // Forward if HTL allows
      const htl = req.htl ?? MAX_HTL;
      if (shouldForward(htl)) {
        const newHtl = decrementHTL(htl, peer.htlConfig);
        await this.forwardRequest(req.h, peer.peerId, newHtl);
      }
    }
  }

  private async handleResponse(peer: WorkerPeer, res: DataResponse): Promise<void> {
    peer.stats.responsesReceived++;
    peer.stats.bytesReceived += res.d.length;

    const hashKey = hashToKey(res.h);
    const pending = peer.pendingRequests.get(hashKey);

    if (!pending) {
      const hasRequesters = Array.from(this.peers.values()).some(p => p.theirRequests.has(hashKey));
      // Late response: cache if we requested this hash recently
      const requestedAt = this.recentRequests.get(hashKey);
      if (!requestedAt && !hasRequesters) return;
      if (requestedAt && Date.now() - requestedAt > 60000) {
        this.recentRequests.delete(hashKey);
        if (!hasRequesters) return;
      }

      const valid = await verifyHash(res.d, res.h);
      if (valid) {
        await this.localStore.put(res.h, res.d);
        if (requestedAt) {
          this.recentRequests.delete(hashKey);
        }
        if (hasRequesters) {
          await this.pushToRequesters(res.h, res.d, peer.peerId);
        }
      }
      return;
    }

    clearTimeout(pending.timeout);
    peer.pendingRequests.delete(hashKey);

    // Verify hash
    const valid = await verifyHash(res.d, res.h);
    if (valid) {
      // Store locally
      await this.localStore.put(res.h, res.d);
      pending.resolve(res.d);

      // Push to peers who requested this
      await this.pushToRequesters(res.h, res.d, peer.peerId);
    } else {
      this.log(`Hash mismatch from ${peer.peerId}`);
      pending.resolve(null);
    }
  }

  private async sendResponse(peer: WorkerPeer, hash: Uint8Array, data: Uint8Array): Promise<void> {
    if (!peer.dataChannelReady) return;

    peer.stats.responsesSent++;
    peer.stats.bytesSent += data.length;

    // Fragment if needed
    if (data.length > FRAGMENT_SIZE) {
      const totalFragments = Math.ceil(data.length / FRAGMENT_SIZE);
      for (let i = 0; i < totalFragments; i++) {
        const start = i * FRAGMENT_SIZE;
        const end = Math.min(start + FRAGMENT_SIZE, data.length);
        const fragment = data.slice(start, end);
        const res = createFragmentResponse(hash, fragment, i, totalFragments);
        const encoded = new Uint8Array(encodeResponse(res));
        this.sendCommand({ type: 'rtc:sendData', peerId: peer.peerId, data: encoded });
      }
    } else {
      const res = createResponse(hash, data);
      const encoded = new Uint8Array(encodeResponse(res));
      this.sendCommand({ type: 'rtc:sendData', peerId: peer.peerId, data: encoded });
    }
  }

  private async forwardRequest(hash: Uint8Array, excludePeerId: string, htl: number): Promise<void> {
    const hashKey = hashToKey(hash);

    // Forward to all connected peers except the one who sent it
    for (const [peerId, peer] of this.peers) {
      if (peerId === excludePeerId) continue;
      if (!peer.dataChannelReady) continue;

      // Set up pending request so we can process the response
      const timeout = setTimeout(() => {
        peer.pendingRequests.delete(hashKey);
      }, this.requestTimeout);

      peer.pendingRequests.set(hashKey, {
        hash,
        resolve: () => {
          // Response will be pushed to original requester via pushToRequesters
        },
        timeout,
      });

      const req = createRequest(hash, htl);
      const encoded = new Uint8Array(encodeRequest(req));
      this.sendCommand({ type: 'rtc:sendData', peerId, data: encoded });
    }
  }

  private async pushToRequesters(hash: Uint8Array, data: Uint8Array, excludePeerId: string): Promise<void> {
    const hashKey = hashToKey(hash);

    for (const [peerId, peer] of this.peers) {
      if (peerId === excludePeerId) continue;

      const theirReq = peer.theirRequests.get(hashKey);
      if (theirReq) {
        peer.theirRequests.delete(hashKey);
        await this.sendResponse(peer, hash, data);
      }
    }
  }

  // ============================================================================
  // Public API
  // ============================================================================

  /**
   * Request data from peers
   */
  async get(hash: Uint8Array): Promise<Uint8Array | null> {
    // Try connected peers
    const connectedPeers = Array.from(this.peers.values())
      .filter(p => p.dataChannelReady);

    if (connectedPeers.length === 0) {
      return null;
    }

    // Send request to all peers, first response wins
    return new Promise((resolve) => {
      let resolved = false;
      const hashKey = hashToKey(hash);
      this.recentRequests.set(hashKey, Date.now());

      for (const peer of connectedPeers) {
        const timeout = setTimeout(() => {
          peer.pendingRequests.delete(hashKey);
          checkDone();
        }, this.requestTimeout);

        peer.pendingRequests.set(hashKey, {
          hash,
          resolve: (data) => {
            if (!resolved && data) {
              resolved = true;
              resolve(data);
            }
            checkDone();
          },
          timeout,
        });

        peer.stats.requestsSent++;
        const req = createRequest(hash, MAX_HTL);
        const encoded = new Uint8Array(encodeRequest(req));
        this.sendCommand({ type: 'rtc:sendData', peerId: peer.peerId, data: encoded });
      }

      let pending = connectedPeers.length;
      const checkDone = () => {
        pending--;
        if (pending === 0 && !resolved) {
          resolve(null);
        }
      };
    });
  }

  /**
   * Get peer stats for UI
   */
  getPeerStats(): Array<{
    peerId: string;
    pubkey: string;
    connected: boolean;
    pool: PeerPool;
    requestsSent: number;
    requestsReceived: number;
    responsesSent: number;
    responsesReceived: number;
    bytesSent: number;
    bytesReceived: number;
  }> {
    return Array.from(this.peers.values()).map(peer => ({
      peerId: peer.peerId,
      pubkey: peer.pubkey,
      connected: peer.state === 'connected' && peer.dataChannelReady,
      pool: peer.pool,
      requestsSent: peer.stats.requestsSent,
      requestsReceived: peer.stats.requestsReceived,
      responsesSent: peer.stats.responsesSent,
      responsesReceived: peer.stats.responsesReceived,
      bytesSent: peer.stats.bytesSent,
      bytesReceived: peer.stats.bytesReceived,
    }));
  }

  /**
   * Get connected peer count
   */
  getConnectedCount(): number {
    let count = 0;
    for (const peer of this.peers.values()) {
      if (peer.state === 'connected' && peer.dataChannelReady) {
        count++;
      }
    }
    return count;
  }

  /**
   * Set pool configuration
   */
  setPoolConfig(config: { follows: { max: number; satisfied: number }; other: { max: number; satisfied: number } }): void {
    this.poolConfig = {
      follows: { maxConnections: config.follows.max, satisfiedConnections: config.follows.satisfied },
      other: { maxConnections: config.other.max, satisfiedConnections: config.other.satisfied },
    };
    this.log('Pool config updated:', this.poolConfig);

    // Re-broadcast hello to trigger peer discovery with new limits
    this.sendHello();
  }

  /**
   * Update identity (pubkey) and restart signaling if already running.
   * This keeps peerId consistent with the current account.
   */
  setIdentity(pubkey: string): void {
    if (this.myPeerId.pubkey === pubkey) return;

    const wasStarted = !!this.helloInterval;
    this.stop();
    this.myPeerId = new PeerId(pubkey, generateUuid());
    if (wasStarted) {
      this.start();
    }
  }

  // ============================================================================
  // Helpers
  // ============================================================================

  private log(...args: unknown[]): void {
    if (this.debug) {
      console.log('[WebRTC]', ...args);
    }
  }
}
