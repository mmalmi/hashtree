/**
 * WebRTC peer connection for hashtree data exchange
 */
import type { Store, Hash } from '../types.js';
import type {
  SignalingMessage,
  DataRequest,
  DataResponse,
  PeerId,
  PendingReassembly,
} from './types.js';
import {
  MAX_HTL,
  MSG_TYPE_REQUEST,
  MSG_TYPE_RESPONSE,
  FRAGMENT_SIZE,
  FRAGMENT_STALL_TIMEOUT,
  FRAGMENT_TOTAL_TIMEOUT,
  MAX_PENDING_REASSEMBLIES,
} from './types.js';
import { LRUCache } from './lruCache.js';
import {
  PendingRequest,
  PeerHTLConfig,
  encodeRequest,
  encodeResponse,
  parseMessage,
  createRequest,
  createResponse,
  createFragmentResponse,
  isFragmented,
  clearPendingRequests,
  generatePeerHTLConfig,
  decrementHTL,
  shouldForward,
  hashToKey,
  verifyHash,
} from './protocol.js';

const ICE_SERVERS = [
  { urls: 'stun:stun.iris.to:3478' },
  { urls: 'stun:stun.l.google.com:19302' },
  { urls: 'stun:stun.cloudflare.com:3478' },
];

// Batch ICE candidates to reduce signaling messages
const ICE_BATCH_DELAY = 100; // ms to wait before sending batched candidates

// Default LRU cache size
const THEIR_REQUESTS_SIZE = 200;

// Request this peer sent us that we couldn't fulfill locally
// We track it so we can push data back when/if we get it
interface TheirRequest {
  hash: Uint8Array;
  requestedAt: number;
}

export class Peer {
  readonly peerId: string;
  readonly pubkey: string;
  readonly direction: 'inbound' | 'outbound';

  private pc: RTCPeerConnection;
  private dataChannel: RTCDataChannel | null = null;
  private localStore: Store | null;
  private sendSignaling: (msg: SignalingMessage) => Promise<void>;
  private onClose: () => void;
  private onConnected?: () => void;
  private onConnectedFired = false;  // Guard against double-firing
  private debug: boolean;

  // Perfect negotiation state
  private makingOffer = false;
  private ignoreOffer = false;
  private isPolite: boolean; // true if we should rollback on collision
  private myPeerId: string; // our peer ID for comparison

  // Requests we sent TO this peer (keyed by hash hex)
  private ourRequests = new Map<string, PendingRequest>();
  // Requests this peer sent TO US that we couldn't fulfill (keyed by hash hex)
  // We track these so we can push data back if we get it later
  private theirRequests = new LRUCache<string, TheirRequest>(THEIR_REQUESTS_SIZE);

  private requestTimeout: number;
  private pendingCandidates: RTCIceCandidateInit[] = [];
  private candidateBatchTimeout: ReturnType<typeof setTimeout> | null = null;
  private queuedRemoteCandidates: RTCIceCandidateInit[] = [];

  // Callback to forward request to other peers when we don't have data locally
  // htl parameter is the decremented HTL to use when forwarding
  private onForwardRequest?: (hash: Uint8Array, excludePeerId: string, htl: number) => Promise<Uint8Array | null>;

  // Per-peer stats tracking
  private stats = {
    requestsSent: 0,
    requestsReceived: 0,
    responsesSent: 0,
    responsesReceived: 0,
    receiveErrors: 0,
    fragmentsSent: 0,
    fragmentsReceived: 0,
    fragmentTimeouts: 0,
    reassembliesCompleted: 0,
    bytesSent: 0,
    bytesReceived: 0,
    bytesForwarded: 0,
  };

  // Fragment reassembly tracking
  private pendingReassemblies = new Map<string, PendingReassembly>();
  private reassemblyCleanupInterval?: ReturnType<typeof setInterval>;

  // Per-peer HTL decrement config (Freenet-style probabilistic)
  private htlConfig: PeerHTLConfig;

  readonly createdAt: number;
  connectedAt?: number;

  constructor(options: {
    peerId: PeerId;
    myPeerId: string; // Our peer ID (uuid) for perfect negotiation
    direction: 'inbound' | 'outbound';
    localStore: Store | null;
    sendSignaling: (msg: SignalingMessage) => Promise<void>;
    onClose: () => void;
    onConnected?: () => void;
    onForwardRequest?: (hash: Uint8Array, excludePeerId: string, htl: number) => Promise<Uint8Array | null>;
    requestTimeout?: number;
    debug?: boolean;
  }) {
    this.peerId = options.peerId.toString();
    this.pubkey = options.peerId.pubkey;
    this.direction = options.direction;
    this.localStore = options.localStore;
    this.sendSignaling = options.sendSignaling;
    this.onClose = options.onClose;
    this.onConnected = options.onConnected;
    this.onForwardRequest = options.onForwardRequest;
    this.requestTimeout = options.requestTimeout ?? 500;
    this.debug = options.debug ?? false;
    this.createdAt = Date.now();
    // Generate random HTL config for this peer (Freenet-style)
    this.htlConfig = generatePeerHTLConfig();

    // Perfect negotiation: polite peer (smaller ID) rolls back on collision
    this.myPeerId = options.myPeerId;
    this.isPolite = options.myPeerId < options.peerId.uuid;

    // Start fragment reassembly cleanup interval
    this.reassemblyCleanupInterval = setInterval(
      () => this.cleanupStaleReassemblies(),
      5000
    );

    this.pc = new RTCPeerConnection({ iceServers: ICE_SERVERS });
    this.setupPeerConnection();
  }

  private log(...args: unknown[]): void {
    if (this.debug) {
      console.log(`[Peer ${this.peerId.slice(0, 12)}]`, ...args);
    }
  }

  get state(): RTCPeerConnectionState {
    return this.pc.connectionState;
  }

  get isConnected(): boolean {
    return this.pc.connectionState === 'connected' &&
           this.dataChannel?.readyState === 'open';
  }

  get pendingTheirRequestsCount(): number {
    return this.theirRequests.size;
  }

  private scheduleCandidateBatch(): void {
    if (this.candidateBatchTimeout) return;

    this.candidateBatchTimeout = setTimeout(() => {
      this.candidateBatchTimeout = null;
      if (this.pendingCandidates.length > 0) {
        const candidates = this.pendingCandidates;
        this.pendingCandidates = [];

        // Send as batch (convert RTCIceCandidateInit to IceCandidate format)
        this.sendSignaling({
          type: 'candidates',
          candidates: candidates.map((c) => ({
            candidate: c.candidate!,
            sdpMLineIndex: c.sdpMLineIndex ?? undefined,
            sdpMid: c.sdpMid ?? undefined,
          })),
          targetPeerId: this.peerId,
          peerId: '', // Will be set by caller
        }).catch((err) => {
          this.log('Failed to send candidates batch:', err);
        });
      }
    }, ICE_BATCH_DELAY);
  }

  private setupPeerConnection(): void {
    this.pc.onicecandidate = (event) => {
      if (event.candidate) {
        this.pendingCandidates.push(event.candidate.toJSON());
        this.scheduleCandidateBatch();
      }
    };

    this.pc.onconnectionstatechange = () => {
      if (this.pc.connectionState === 'connected') {
        this.connectedAt = Date.now();
        // Only trigger onConnected if data channel is also ready
        // (it may already be open, or will fire via channel.onopen)
        if (this.dataChannel?.readyState === 'open' && !this.onConnectedFired) {
          this.onConnectedFired = true;
          this.onConnected?.();
        }
      } else if (
        this.pc.connectionState === 'failed' ||
        this.pc.connectionState === 'closed' ||
        this.pc.connectionState === 'disconnected'
      ) {
        this.close();
      }
    };

    this.pc.ondatachannel = (event) => {
      this.dataChannel = event.channel;
      this.setupDataChannel(this.dataChannel);
    };
  }

  private setupDataChannel(channel: RTCDataChannel): void {
    channel.binaryType = 'arraybuffer';

    channel.onopen = () => {
      // If PC is already connected, fire onConnected now
      // (handles case where data channel opens after PC connects)
      if (this.pc.connectionState === 'connected' && !this.onConnectedFired) {
        this.onConnectedFired = true;
        this.onConnected?.();
      }
    };

    channel.onclose = () => {
      this.log('Data channel closed');
      this.close();
    };

    channel.onmessage = async (event) => {
      // All messages are binary with type prefix
      if (event.data instanceof ArrayBuffer) {
        await this.handleMessage(event.data);
      }
    };
  }

  private async handleMessage(data: ArrayBuffer): Promise<void> {
    try {
      const msg = parseMessage(data);
      if (!msg) {
        this.log('Failed to parse message');
        this.stats.receiveErrors++;
        return;
      }

      if (msg.type === MSG_TYPE_REQUEST) {
        await this.handleRequest(msg.body);
      } else if (msg.type === MSG_TYPE_RESPONSE) {
        const res = msg.body as DataResponse;

        // Handle fragmented vs unfragmented responses
        let finalData: Uint8Array;
        let hash: Uint8Array;

        if (isFragmented(res)) {
          // Fragmented response - reassemble
          const assembled = this.handleFragmentResponse(res);
          if (!assembled) {
            return; // Incomplete, wait for more fragments
          }
          finalData = assembled;
          hash = res.h;
        } else {
          // Unfragmented response - use directly
          finalData = res.d;
          hash = res.h;
        }

        // Now handle the complete response (verify hash and resolve pending request)
        const hashKey = hashToKey(hash);
        const pending = this.ourRequests.get(hashKey);
        if (!pending) {
          return; // No pending request for this hash
        }

        clearTimeout(pending.timeout);
        this.ourRequests.delete(hashKey);

        const isValid = await verifyHash(finalData, hash);
        if (isValid) {
          pending.resolve(finalData);
          this.stats.responsesReceived++;
          this.stats.bytesReceived += finalData.length;
        } else {
          pending.resolve(null);
          this.stats.receiveErrors++;
        }
      }
    } catch (err) {
      this.log('Error handling message:', err);
      this.stats.receiveErrors++;
    }
  }

  private async handleRequest(req: DataRequest): Promise<void> {
    const htl = req.htl ?? MAX_HTL;
    const hash = req.h;
    const hashKey = hashToKey(hash);

    this.stats.requestsReceived++;

    // Try local store first
    if (this.localStore) {
      const data = await this.localStore.get(hash);

      if (data) {
        this.sendResponse(hash, data);
        this.stats.responsesSent++;
        return;
      }
    }

    // Not found locally - check if we should forward based on HTL
    if (this.onForwardRequest && shouldForward(htl)) {
      // Track this request so we can push data back later if we get it
      this.theirRequests.set(hashKey, {
        hash,
        requestedAt: Date.now(),
      });

      // Decrement HTL before forwarding (Freenet-style per-peer decrement)
      const forwardHTL = decrementHTL(htl, this.htlConfig);

      // Forward to other peers (excluding this one)
      const data = await this.onForwardRequest(hash, this.peerId, forwardHTL);

      if (data) {
        // Got it from another peer, send response (mark as forwarded)
        this.theirRequests.delete(hashKey);
        this.sendResponse(hash, data, true);
        this.stats.responsesSent++;
        return;
      }
      // If not found, keep in theirRequests for later push
    }

    // Not found anywhere - stay silent, let requester timeout.
  }

  private sendResponse(hash: Uint8Array, data: Uint8Array, isForwarded = false): void {
    if (!this.dataChannel || this.dataChannel.readyState !== 'open') return;

    // Track bytes sent
    this.stats.bytesSent += data.length;
    if (isForwarded) {
      this.stats.bytesForwarded += data.length;
    }

    if (data.length <= FRAGMENT_SIZE) {
      // Small enough - send unfragmented (backward compatible)
      const res = createResponse(hash, data);
      this.dataChannel.send(encodeResponse(res));
    } else {
      // Fragment large responses
      const totalFragments = Math.ceil(data.length / FRAGMENT_SIZE);
      for (let i = 0; i < totalFragments; i++) {
        const start = i * FRAGMENT_SIZE;
        const end = Math.min(start + FRAGMENT_SIZE, data.length);
        const fragment = data.slice(start, end);

        const res = createFragmentResponse(hash, fragment, i, totalFragments);
        this.dataChannel.send(encodeResponse(res));
        this.stats.fragmentsSent++;
      }
    }
  }

  /**
   * Handle a fragmented response - buffer and reassemble
   * Returns assembled data when complete, null when waiting for more fragments
   */
  private handleFragmentResponse(res: DataResponse): Uint8Array | null {
    const hashKey = hashToKey(res.h);
    const now = Date.now();

    this.stats.fragmentsReceived++;

    let pending = this.pendingReassemblies.get(hashKey);
    if (!pending) {
      pending = {
        hash: res.h,
        fragments: new Map(),
        totalExpected: res.n!,
        receivedBytes: 0,
        firstFragmentAt: now,
        lastFragmentAt: now,
      };
      this.pendingReassemblies.set(hashKey, pending);
    }

    // Reset request timeout on each fragment received
    // This way we timeout if no fragment arrives for FRAGMENT_STALL_TIMEOUT
    const pendingReq = this.ourRequests.get(hashKey);
    if (pendingReq) {
      clearTimeout(pendingReq.timeout);
      pendingReq.timeout = setTimeout(() => {
        this.ourRequests.delete(hashKey);
        this.pendingReassemblies.delete(hashKey);
        this.stats.fragmentTimeouts++;
        pendingReq.resolve(null);
      }, FRAGMENT_STALL_TIMEOUT);
    }

    // Store fragment if not duplicate
    if (!pending.fragments.has(res.i!)) {
      pending.fragments.set(res.i!, res.d);
      pending.receivedBytes += res.d.length;
      pending.lastFragmentAt = now;
    }

    // Check if complete
    if (pending.fragments.size === pending.totalExpected) {
      this.pendingReassemblies.delete(hashKey);
      this.stats.reassembliesCompleted++;
      return this.assembleFragments(pending);
    }

    return null; // Not yet complete
  }

  /**
   * Assemble fragments in order into a single buffer
   */
  private assembleFragments(pending: PendingReassembly): Uint8Array {
    const result = new Uint8Array(pending.receivedBytes);
    let offset = 0;
    for (let i = 0; i < pending.totalExpected; i++) {
      const fragment = pending.fragments.get(i)!;
      result.set(fragment, offset);
      offset += fragment.length;
    }
    return result;
  }

  /**
   * Clean up stale reassemblies (stalled or timed out)
   */
  private cleanupStaleReassemblies(): void {
    const now = Date.now();
    for (const [key, pending] of this.pendingReassemblies) {
      const stallTime = now - pending.lastFragmentAt;
      const totalTime = now - pending.firstFragmentAt;

      if (stallTime > FRAGMENT_STALL_TIMEOUT || totalTime > FRAGMENT_TOTAL_TIMEOUT) {
        this.pendingReassemblies.delete(key);
        this.stats.fragmentTimeouts++;
        this.log('Fragment reassembly timed out:', key.slice(0, 16),
          `stall=${stallTime}ms, total=${totalTime}ms, fragments=${pending.fragments.size}/${pending.totalExpected}`);
      }
    }

    // Memory cap enforcement - evict oldest if over limit
    if (this.pendingReassemblies.size > MAX_PENDING_REASSEMBLIES) {
      const oldest = [...this.pendingReassemblies.entries()]
        .sort((a, b) => a[1].firstFragmentAt - b[1].firstFragmentAt)[0];
      if (oldest) {
        this.pendingReassemblies.delete(oldest[0]);
        this.stats.fragmentTimeouts++;
        this.log('Fragment reassembly evicted (memory cap):', oldest[0].slice(0, 16));
      }
    }
  }

  /**
   * Request data by hash from this peer
   * @param htl Hops To Live - decremented before sending
   */
  async request(hash: Hash, htl: number = MAX_HTL): Promise<Uint8Array | null> {
    if (!this.dataChannel || this.dataChannel.readyState !== 'open') {
      return null;
    }

    const hashKey = hashToKey(hash);

    // Check if we already have a pending request for this hash
    const existing = this.ourRequests.get(hashKey);
    if (existing) {
      // Return a new promise that resolves when the existing one does
      return new Promise((resolve) => {
        const originalResolve = existing.resolve;
        existing.resolve = (data) => {
          originalResolve(data);
          resolve(data);
        };
      });
    }

    // Decrement HTL before sending (Freenet-style per-peer decrement)
    const sendHTL = decrementHTL(htl, this.htlConfig);

    this.stats.requestsSent++;

    return new Promise((resolve) => {
      const timeout = setTimeout(() => {
        this.ourRequests.delete(hashKey);
        resolve(null);
      }, this.requestTimeout);

      this.ourRequests.set(hashKey, { hash, resolve, timeout });

      const req = createRequest(hash, sendHTL);
      this.dataChannel!.send(encodeRequest(req));
    });
  }

  /**
   * Get per-peer statistics
   */
  getStats(): {
    requestsSent: number;
    requestsReceived: number;
    responsesSent: number;
    responsesReceived: number;
    receiveErrors: number;
    fragmentsSent: number;
    fragmentsReceived: number;
    fragmentTimeouts: number;
    reassembliesCompleted: number;
    bytesSent: number;
    bytesReceived: number;
    bytesForwarded: number;
  } {
    return { ...this.stats };
  }

  /**
   * Send data to this peer for a hash they previously requested
   * Returns true if this peer had requested this hash
   */
  sendData(hash: Uint8Array, data: Uint8Array): boolean {
    const hashKey = hashToKey(hash);
    const theirReq = this.theirRequests.get(hashKey);
    if (!theirReq) {
      return false;
    }

    this.theirRequests.delete(hashKey);

    // Send response with data
    this.sendResponse(hash, data);
    this.stats.responsesSent++;

    this.log('Sent data for hash:', hashKey.slice(0, 16));
    return true;
  }

  /**
   * Check if this peer has requested a hash
   */
  hasRequested(hash: Uint8Array): boolean {
    return this.theirRequests.has(hashToKey(hash));
  }

  /**
   * Get count of pending requests from this peer
   */
  getTheirRequestCount(): number {
    return this.theirRequests.size;
  }

  /**
   * Initiate connection (create offer)
   * Uses perfect negotiation pattern - both peers can call this
   */
  async connect(): Promise<void> {
    // Create data channel if we don't have one yet
    if (!this.dataChannel) {
      // Unordered for better performance - protocol is stateless (each message self-describes)
      this.dataChannel = this.pc.createDataChannel('hashtree', { ordered: false });
      this.setupDataChannel(this.dataChannel);
    }

    try {
      this.makingOffer = true;
      const offer = await this.pc.createOffer();
      await this.pc.setLocalDescription(offer);

      await this.sendSignaling({
        type: 'offer',
        sdp: offer.sdp!,
        targetPeerId: this.peerId,
        peerId: this.myPeerId,
      });
    } finally {
      this.makingOffer = false;
    }
  }

  /**
   * Handle incoming signaling message
   * Implements perfect negotiation pattern for collision handling
   */
  async handleSignaling(msg: SignalingMessage): Promise<void> {
    if (msg.type === 'offer') {
      // Perfect negotiation: check for offer collision
      const offerCollision =
        this.makingOffer ||
        (this.pc.signalingState !== 'stable' && this.pc.signalingState !== 'closed');

      this.ignoreOffer = !this.isPolite && offerCollision;

      if (this.ignoreOffer) {
        this.log('Ignoring offer collision (impolite peer)');
        return;
      }

      // If we're polite and have a collision, we rollback and accept their offer
      if (offerCollision) {
        this.log('Rolling back local offer (polite peer)');
      }

      // Construct RTCSessionDescriptionInit from flat sdp field
      await this.pc.setRemoteDescription(new RTCSessionDescription({ type: 'offer', sdp: msg.sdp }));
      await this.processQueuedCandidates();
      const answer = await this.pc.createAnswer();
      await this.pc.setLocalDescription(answer);

      await this.sendSignaling({
        type: 'answer',
        sdp: answer.sdp!,
        targetPeerId: this.peerId,
        peerId: this.myPeerId,
      });
    } else if (msg.type === 'answer') {
      // Ignore answer if we're not expecting one (e.g., after rollback)
      if (this.pc.signalingState === 'stable') {
        this.log('Ignoring unexpected answer in stable state');
        return;
      }
      // Construct RTCSessionDescriptionInit from flat sdp field
      await this.pc.setRemoteDescription(new RTCSessionDescription({ type: 'answer', sdp: msg.sdp }));
      await this.processQueuedCandidates();
    } else if (msg.type === 'candidate') {
      // Construct RTCIceCandidateInit from flat fields
      await this.addRemoteCandidate({
        candidate: msg.candidate,
        sdpMLineIndex: msg.sdpMLineIndex,
        sdpMid: msg.sdpMid,
      });
    } else if (msg.type === 'candidates') {
      // Handle batched candidates
      for (const c of msg.candidates) {
        await this.addRemoteCandidate({
          candidate: c.candidate,
          sdpMLineIndex: c.sdpMLineIndex,
          sdpMid: c.sdpMid,
        });
      }
    }
  }

  private async addRemoteCandidate(candidate: RTCIceCandidateInit): Promise<void> {
    // Queue candidates if remote description not set yet
    if (!this.pc.remoteDescription) {
      this.queuedRemoteCandidates.push(candidate);
      return;
    }

    try {
      await this.pc.addIceCandidate(new RTCIceCandidate(candidate));
    } catch (err) {
      this.log('Failed to add ICE candidate:', err);
    }
  }

  private async processQueuedCandidates(): Promise<void> {
    const candidates = this.queuedRemoteCandidates;
    this.queuedRemoteCandidates = [];

    for (const candidate of candidates) {
      try {
        await this.pc.addIceCandidate(new RTCIceCandidate(candidate));
      } catch (err) {
        this.log('Failed to add queued ICE candidate:', err);
      }
    }
  }

  /**
   * Close the peer connection
   */
  close(): void {
    if (this.candidateBatchTimeout) {
      clearTimeout(this.candidateBatchTimeout);
      this.candidateBatchTimeout = null;
    }

    // Clean up fragment reassembly
    if (this.reassemblyCleanupInterval) {
      clearInterval(this.reassemblyCleanupInterval);
      this.reassemblyCleanupInterval = undefined;
    }
    this.pendingReassemblies.clear();

    clearPendingRequests(this.ourRequests);

    if (this.dataChannel) {
      this.dataChannel.onopen = null;
      this.dataChannel.onclose = null;
      this.dataChannel.onmessage = null;
      this.dataChannel.close();
      this.dataChannel = null;
    }

    this.pc.onicecandidate = null;
    this.pc.onconnectionstatechange = null;
    this.pc.ondatachannel = null;
    this.pc.close();

    this.onClose();
  }
}
