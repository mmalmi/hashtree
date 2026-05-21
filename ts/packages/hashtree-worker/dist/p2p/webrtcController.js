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
import { fromHex, sha256, toHex } from '@hashtree/core';
import { MAX_HTL, MSG_TYPE_REQUEST, MSG_TYPE_RESPONSE, FRAGMENT_SIZE, PeerId, encodeRequest, encodeResponse, parseMessage, createRequest, createResponse, createFragmentResponse, hashToKey, verifyHash, generatePeerHTLConfig, PeerSelector, buildHedgedWavePlan, normalizeDispatchConfig, syncSelectorPeers, } from '@hashtree/mesh';
import { LRUCache } from './lruCache.js';
import { MeshQueryRouter, encodeForwardRequest } from './meshQueryRouter.js';
const PEER_METADATA_POINTER_SLOT_KEY = 'hashtree-webrtc/peer-metadata/latest/v1';
const DEFAULT_REQUEST_DISPATCH = {
    initialFanout: 2,
    hedgeFanout: 1,
    maxFanout: 8,
    hedgeIntervalMs: 120,
};
const ACTIVE_REQUEST_RANK_PENALTY = 3;
const DISCONNECTED_PEER_RETRY_DELAY_MS = 2_500;
const STALE_CONNECTING_PEER_MS = 15_000;
const STALE_DATA_CHANNEL_WAIT_MS = 5_000;
const MIN_FRAGMENT_STALL_TIMEOUT_MS = 5_000;
const DEFAULT_POOL_CONFIG = {
    follows: { maxConnections: 20, satisfiedConnections: 10 },
    other: { maxConnections: 16, satisfiedConnections: 8 },
};
function clonePoolConfig(config) {
    return {
        follows: { ...config.follows },
        other: { ...config.other },
    };
}
function normalizePoolConfig(config) {
    if (!config) {
        return clonePoolConfig(DEFAULT_POOL_CONFIG);
    }
    return {
        follows: { maxConnections: config.follows.max, satisfiedConnections: config.follows.satisfied },
        other: { maxConnections: config.other.max, satisfiedConnections: config.other.satisfied },
    };
}
// ============================================================================
// Controller
// ============================================================================
export class WebRTCController {
    myPeerId;
    peers = new Map();
    pendingRemoteCandidates = new Map();
    localStore;
    sendCommand;
    sendSignaling;
    classifyPeer;
    requestTimeout;
    debug;
    recentRequests = new LRUCache(1000);
    activePeerRequests = new Map();
    meshRouter;
    peerSelector;
    routing;
    // Pool configuration - reasonable defaults, settings sync will override
    poolConfig = clonePoolConfig(DEFAULT_POOL_CONFIG);
    // Hello interval - 5s for faster peer discovery
    helloInterval;
    HELLO_INTERVAL = 5000;
    constructor(config) {
        this.myPeerId = new PeerId(config.pubkey);
        this.localStore = config.localStore;
        this.sendCommand = config.sendCommand;
        this.sendSignaling = config.sendSignaling;
        this.requestTimeout = config.requestTimeout ?? 1000;
        this.debug = config.debug ?? false;
        this.routing = {
            // Read-side routing should chase the peer most likely to answer.
            selectionStrategy: config.requestSelectionStrategy ?? 'weighted',
            fairnessEnabled: config.requestFairnessEnabled ?? true,
            dispatch: config.requestDispatch ?? DEFAULT_REQUEST_DISPATCH,
        };
        this.peerSelector = PeerSelector.withStrategy(this.routing.selectionStrategy);
        this.peerSelector.setFairness(this.routing.fairnessEnabled);
        this.meshRouter = new MeshQueryRouter({
            localStore: this.localStore,
            requestTimeoutMs: this.requestTimeout,
            upstreamFetch: config.upstreamFetch,
            queryPeers: (hash, options) => this.queryPeersWithDispatch(hash, options),
            maxForwardsPerPeerWindow: config.forwardRateLimit?.maxForwardsPerPeerWindow,
            forwardRateLimitWindowMs: config.forwardRateLimit?.windowMs,
        });
        // Default classifier: check if pubkey is in follows
        const getFollows = config.getFollows ?? (() => new Set());
        this.classifyPeer = (pubkey) => {
            const follows = getFollows();
            const isFollow = follows.has(pubkey);
            return isFollow ? 'follows' : 'other';
        };
    }
    // ============================================================================
    // Lifecycle
    // ============================================================================
    start() {
        this.log('Starting WebRTC controller');
        // Send hello periodically
        this.helloInterval = setInterval(() => {
            this.sendHello();
        }, this.HELLO_INTERVAL);
        // Send initial hello
        this.sendHello();
    }
    stop() {
        this.log('Stopping WebRTC controller');
        if (this.helloInterval) {
            clearInterval(this.helloInterval);
            this.helloInterval = undefined;
        }
        // Close all peers
        for (const peerId of this.peers.keys()) {
            this.closePeer(peerId);
        }
        this.meshRouter.stop();
    }
    // ============================================================================
    // Signaling
    // ============================================================================
    sendHello() {
        const msg = {
            type: 'hello',
            peerId: this.myPeerId.toString(),
            hashGet: true,
        };
        this.sendSignaling(msg).catch(err => {
            console.error('[WebRTC] sendSignaling error:', err);
        });
    }
    /**
     * Public method to trigger a hello broadcast.
     * Used for testing to force peer discovery after follows are set up.
     */
    broadcastHello() {
        this.sendHello();
    }
    /**
     * Handle incoming signaling message (from Nostr kind 25050)
     *
     * `peerId` is the remote endpoint identity.
     */
    async handleSignalingMessage(msg, senderPubkey) {
        this.log(`Signaling from ${senderPubkey.slice(0, 8)}:`, msg.type);
        switch (msg.type) {
            case 'hello':
                await this.handleHello(senderPubkey, msg.hashGet !== false);
                break;
            case 'offer':
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
    isMessageForUs(msg) {
        if ('targetPeerId' in msg && msg.targetPeerId) {
            return msg.targetPeerId === this.myPeerId.toString();
        }
        return true;
    }
    async handleHello(senderPubkey, hashGet) {
        const peerId = new PeerId(senderPubkey).toString();
        // Already connected?
        const existing = this.peers.get(peerId);
        if (existing) {
            existing.hashGet = hashGet;
            if (!this.shouldReplacePeer(existing)) {
                return;
            }
            this.log(`Replacing stale peer before hello handling: ${peerId.slice(0, 20)}`);
            this.closePeer(peerId);
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
        // Tie-breaking: lower endpoint ID initiates
        const shouldInitiate = this.myPeerId.toString() < peerId;
        if (shouldInitiate) {
            this.log(`Initiating connection to ${peerId.slice(0, 20)}`);
            await this.createOutboundPeer(peerId, senderPubkey, pool, hashGet);
        }
        else {
            this.log(`Waiting for offer from ${peerId.slice(0, 20)}`);
        }
    }
    async handleOffer(peerId, pubkey, offer) {
        this.log(`handleOffer from ${pubkey.slice(0, 8)}, peerId: ${peerId.slice(0, 20)}`);
        let peer = this.peers.get(peerId);
        if (peer && this.shouldReplacePeer(peer)) {
            this.log(`Replacing stale peer before offer handling: ${peerId.slice(0, 20)}`);
            this.closePeer(peerId);
            peer = undefined;
        }
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
        else if (peer.direction === 'outbound' && peer.state === 'connecting') {
            const isPolite = this.myPeerId.toString() < peerId;
            if (!isPolite) {
                this.log(`Ignoring offer collision from ${pubkey.slice(0, 8)} as impolite peer`);
                return;
            }
            // Perfect negotiation: the polite peer abandons its local offer and
            // switches into answerer mode for the remote offer.
            peer.direction = 'inbound';
            peer.answerCreated = false;
        }
        this.log(`Setting remote description for ${peerId.slice(0, 20)}`);
        this.sendCommand({ type: 'rtc:setRemoteDescription', peerId, sdp: offer });
    }
    async handleAnswer(peerId, answer) {
        const peer = this.peers.get(peerId);
        if (!peer) {
            this.log(`Answer for unknown peer: ${peerId}`);
            return;
        }
        this.sendCommand({ type: 'rtc:setRemoteDescription', peerId, sdp: answer });
    }
    async handleIceCandidate(peerId, candidate) {
        const peer = this.peers.get(peerId);
        if (!peer) {
            const queued = this.pendingRemoteCandidates.get(peerId) ?? [];
            queued.push(candidate);
            this.pendingRemoteCandidates.set(peerId, queued);
            return;
        }
        this.sendCommand({ type: 'rtc:addIceCandidate', peerId, candidate });
    }
    // ============================================================================
    // Peer Management
    // ============================================================================
    shouldConnect(pool) {
        const config = this.poolConfig[pool];
        const count = this.getPoolCount(pool);
        return count < config.maxConnections;
    }
    getPoolCount(pool) {
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
    hasOtherPoolPubkey(pubkey) {
        for (const peer of this.peers.values()) {
            if (peer.pool === 'other' && peer.pubkey === pubkey && peer.state !== 'disconnected') {
                return true;
            }
        }
        return false;
    }
    clearPeerRecoveryTimer(peer) {
        if (!peer.recoveryTimer) {
            return;
        }
        clearTimeout(peer.recoveryTimer);
        peer.recoveryTimer = undefined;
    }
    schedulePeerRecovery(peer) {
        this.clearPeerRecoveryTimer(peer);
        peer.recoveryTimer = setTimeout(() => {
            const activePeer = this.peers.get(peer.peerId);
            if (!activePeer || activePeer.state !== 'disconnected') {
                return;
            }
            this.log(`Recovering disconnected peer: ${peer.peerId.slice(0, 20)}`);
            this.closePeer(peer.peerId);
            this.sendHello();
        }, DISCONNECTED_PEER_RETRY_DELAY_MS);
    }
    shouldReplacePeer(peer, now = Date.now()) {
        if (peer.state === 'disconnected') {
            return true;
        }
        if (peer.state === 'connecting') {
            return now - peer.createdAt >= STALE_CONNECTING_PEER_MS;
        }
        if (peer.state === 'connected' && !peer.dataChannelReady) {
            const connectedAt = peer.connectedAt ?? peer.createdAt;
            return now - connectedAt >= STALE_DATA_CHANNEL_WAIT_MS;
        }
        return false;
    }
    createPeer(peerId, pubkey, pool, direction, hashGet = true) {
        const peer = {
            peerId,
            pubkey,
            pool,
            hashGet,
            direction,
            state: 'connecting',
            dataChannelReady: false,
            answerCreated: false,
            htlConfig: generatePeerHTLConfig(),
            pendingRequests: new Map(),
            pendingReassemblies: new Map(),
            stats: {
                requestsSent: 0,
                requestsReceived: 0,
                responsesSent: 0,
                responsesReceived: 0,
                bytesSent: 0,
                bytesReceived: 0,
                forwardedRequests: 0,
                forwardedResolved: 0,
                forwardedSuppressed: 0,
            },
            createdAt: Date.now(),
            disconnectedAt: undefined,
            recoveryTimer: undefined,
            bufferPaused: false,
            deferredRequests: [],
        };
        this.peers.set(peerId, peer);
        this.peerSelector.addPeer(peerId);
        this.meshRouter.registerPeer({
            peerId,
            canSend: () => peer.dataChannelReady,
            getHtlConfig: () => peer.htlConfig,
            sendRequest: (hash, htl) => this.sendRequestToPeer(peer, hash, htl),
            sendResponse: async (hash, data) => this.sendResponse(peer, hash, data),
            onForwardedRequest: () => {
                peer.stats.forwardedRequests++;
            },
            onForwardedResolved: () => {
                peer.stats.forwardedResolved++;
            },
            onForwardedSuppressed: () => {
                peer.stats.forwardedSuppressed++;
            },
        });
        this.sendCommand({ type: 'rtc:createPeer', peerId, pubkey });
        return peer;
    }
    async createOutboundPeer(peerId, pubkey, pool, hashGet) {
        this.createPeer(peerId, pubkey, pool, 'outbound', hashGet);
        // Proxy will create peer and we'll get rtc:peerCreated, then request offer
    }
    closePeer(peerId) {
        const peer = this.peers.get(peerId);
        if (!peer)
            return;
        this.clearPeerRecoveryTimer(peer);
        // Clear pending requests
        for (const [hashKey, pending] of peer.pendingRequests.entries()) {
            clearTimeout(pending.timeout);
            peer.pendingRequests.delete(hashKey);
            this.releasePeerRequest(peer.peerId);
            pending.resolve(null);
        }
        for (const [hashKey, pending] of peer.pendingReassemblies.entries()) {
            clearTimeout(pending.timeout);
            peer.pendingReassemblies.delete(hashKey);
        }
        peer.state = 'disconnected';
        this.sendCommand({ type: 'rtc:closePeer', peerId });
        this.peers.delete(peerId);
        this.pendingRemoteCandidates.delete(peerId);
        this.peerSelector.removePeer(peerId);
        this.meshRouter.removePeer(peerId);
        this.log(`Closed peer: ${peerId.slice(0, 20)}`);
    }
    // ============================================================================
    // Proxy Events
    // ============================================================================
    /**
     * Handle event from main thread proxy
     */
    handleProxyEvent(event) {
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
    onPeerCreated(peerId) {
        const peer = this.peers.get(peerId);
        if (!peer)
            return;
        const queuedCandidates = this.pendingRemoteCandidates.get(peerId);
        if (queuedCandidates?.length) {
            for (const candidate of queuedCandidates) {
                this.sendCommand({ type: 'rtc:addIceCandidate', peerId, candidate });
            }
            this.pendingRemoteCandidates.delete(peerId);
        }
        // If outbound, create offer
        if (peer.direction === 'outbound') {
            this.sendCommand({ type: 'rtc:createOffer', peerId });
        }
    }
    onPeerStateChange(peerId, state) {
        const peer = this.peers.get(peerId);
        if (!peer)
            return;
        this.log(`Peer ${peerId.slice(0, 20)} state: ${state}`);
        if (state === 'connected') {
            peer.state = 'connected';
            peer.connectedAt = Date.now();
            peer.disconnectedAt = undefined;
            this.clearPeerRecoveryTimer(peer);
        }
        else if (state === 'connecting') {
            peer.state = 'connecting';
        }
        else if (state === 'disconnected') {
            peer.state = 'disconnected';
            peer.dataChannelReady = false;
            peer.disconnectedAt = Date.now();
            this.schedulePeerRecovery(peer);
        }
        else if (state === 'failed' || state === 'closed') {
            this.closePeer(peerId);
        }
    }
    onPeerClosed(peerId) {
        const peer = this.peers.get(peerId);
        if (peer) {
            this.clearPeerRecoveryTimer(peer);
        }
        this.peers.delete(peerId);
        this.peerSelector.removePeer(peerId);
    }
    onOfferCreated(peerId, sdp) {
        const peer = this.peers.get(peerId);
        if (!peer)
            return;
        // Set local description
        this.sendCommand({ type: 'rtc:setLocalDescription', peerId, sdp });
        // Send offer via signaling using endpoint identities.
        const msg = {
            type: 'offer',
            sdp: sdp.sdp,
            targetPeerId: peerId,
            peerId: this.myPeerId.toString(),
        };
        this.sendSignaling(msg, peer.pubkey);
    }
    onAnswerCreated(peerId, sdp) {
        this.log(`onAnswerCreated for ${peerId.slice(0, 20)}`);
        const peer = this.peers.get(peerId);
        if (!peer) {
            this.log(`onAnswerCreated: peer not found for ${peerId.slice(0, 20)}`);
            return;
        }
        this.sendCommand({ type: 'rtc:setLocalDescription', peerId, sdp });
        this.log(`Sending answer to ${peer.pubkey.slice(0, 8)}`);
        const msg = {
            type: 'answer',
            sdp: sdp.sdp,
            targetPeerId: peerId,
            peerId: this.myPeerId.toString(),
        };
        this.sendSignaling(msg, peer.pubkey);
    }
    onDescriptionSet(peerId, error) {
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
    onIceCandidate(peerId, candidate) {
        if (!candidate || !candidate.candidate)
            return;
        const peer = this.peers.get(peerId);
        if (!peer)
            return;
        // Send candidate via signaling using endpoint identities.
        const msg = {
            type: 'candidate',
            candidate: candidate.candidate,
            sdpMLineIndex: candidate.sdpMLineIndex ?? undefined,
            sdpMid: candidate.sdpMid ?? undefined,
            targetPeerId: peerId,
            peerId: this.myPeerId.toString(),
        };
        this.sendSignaling(msg, peer.pubkey);
    }
    onDataChannelOpen(peerId) {
        const peer = this.peers.get(peerId);
        if (!peer)
            return;
        peer.dataChannelReady = true;
        peer.disconnectedAt = undefined;
        this.clearPeerRecoveryTimer(peer);
        this.log(`Data channel open: ${peerId.slice(0, 20)}`);
    }
    onDataChannelClose(peerId) {
        const peer = this.peers.get(peerId);
        if (!peer)
            return;
        peer.dataChannelReady = false;
        this.closePeer(peerId);
    }
    onDataChannelError(peerId, error) {
        this.log(`Data channel error for ${peerId}: ${error}`);
    }
    onBufferHigh(peerId) {
        const peer = this.peers.get(peerId);
        if (!peer)
            return;
        peer.bufferPaused = true;
        this.log(`Buffer high for ${peerId.slice(0, 20)}, pausing responses`);
    }
    onBufferLow(peerId) {
        const peer = this.peers.get(peerId);
        if (!peer)
            return;
        peer.bufferPaused = false;
        this.log(`Buffer low for ${peerId.slice(0, 20)}, resuming responses`);
        // Process deferred requests
        this.processDeferredRequests(peer);
    }
    async processDeferredRequests(peer) {
        while (!peer.bufferPaused && peer.deferredRequests.length > 0) {
            const req = peer.deferredRequests.shift();
            await this.processRequest(peer, req);
        }
    }
    orderedConnectedPeers(excludePeerId) {
        const connectedAll = Array.from(this.peers.values())
            .filter((peer) => peer.dataChannelReady && peer.hashGet);
        if (connectedAll.length === 0)
            return [];
        const peerIds = connectedAll.map((peer) => peer.peerId);
        syncSelectorPeers(this.peerSelector, peerIds);
        const connectedPeers = connectedAll
            .filter((peer) => !excludePeerId || peer.peerId !== excludePeerId);
        const selectorOrder = this.peerSelector.selectPeers();
        const rank = new Map(selectorOrder.map((peerId, idx) => [peerId, idx]));
        connectedPeers.sort((a, b) => {
            const leftBackedOff = this.peerSelector.isPeerBackedOff(a.peerId);
            const rightBackedOff = this.peerSelector.isPeerBackedOff(b.peerId);
            if (leftBackedOff !== rightBackedOff)
                return leftBackedOff ? 1 : -1;
            if (a.pool === 'follows' && b.pool !== 'follows')
                return -1;
            if (a.pool !== 'follows' && b.pool === 'follows')
                return 1;
            const leftRank = rank.get(a.peerId) ?? Number.MAX_SAFE_INTEGER;
            const rightRank = rank.get(b.peerId) ?? Number.MAX_SAFE_INTEGER;
            const leftLoad = this.activePeerRequests.get(a.peerId) ?? 0;
            const rightLoad = this.activePeerRequests.get(b.peerId) ?? 0;
            return (leftRank + leftLoad * ACTIVE_REQUEST_RANK_PENALTY) -
                (rightRank + rightLoad * ACTIVE_REQUEST_RANK_PENALTY);
        });
        return connectedPeers;
    }
    async peerMetadataPointerHash() {
        return sha256(new TextEncoder().encode(PEER_METADATA_POINTER_SLOT_KEY));
    }
    createInFlightRequest(peer, hash, htl) {
        const hashKey = hashToKey(hash);
        const startedAt = Date.now();
        this.reservePeerRequest(peer.peerId);
        this.peerSelector.recordRequest(peer.peerId, 40);
        const promise = new Promise((resolve) => {
            const timeout = setTimeout(() => {
                peer.pendingRequests.delete(hashKey);
                this.releasePeerRequest(peer.peerId);
                this.peerSelector.recordTimeout(peer.peerId);
                resolve({ peerId: peer.peerId, data: null, elapsedMs: Math.max(1, Date.now() - startedAt) });
            }, this.requestTimeout);
            peer.pendingRequests.set(hashKey, {
                hash,
                startedAt,
                resolve: (data) => {
                    resolve({ peerId: peer.peerId, data, elapsedMs: Math.max(1, Date.now() - startedAt) });
                },
                timeout,
            });
            peer.stats.requestsSent++;
            const req = createRequest(hash, htl);
            const encoded = new Uint8Array(encodeRequest(req));
            this.sendDataToPeer(peer, encoded);
        });
        return {
            peerId: peer.peerId,
            settled: false,
            promise,
        };
    }
    resetPendingRequestTimeout(peer, hashKey, pending) {
        clearTimeout(pending.timeout);
        pending.timeout = setTimeout(() => {
            peer.pendingRequests.delete(hashKey);
            peer.pendingReassemblies.delete(hashKey);
            this.releasePeerRequest(peer.peerId);
            this.peerSelector.recordTimeout(peer.peerId);
            pending.resolve(null);
        }, this.requestTimeout);
    }
    async waitForInFlightResult(inFlight, waitMs) {
        const active = inFlight.filter((task) => !task.settled);
        if (active.length === 0)
            return null;
        if (waitMs !== undefined && waitMs <= 0)
            return null;
        const outcomes = active.map((task) => task.promise.then((result) => ({
            task,
            data: result.data,
            elapsedMs: result.elapsedMs,
        })));
        const outcome = waitMs === undefined
            ? await Promise.race(outcomes)
            : await Promise.race([
                new Promise((resolve) => {
                    setTimeout(() => resolve(null), waitMs);
                }),
                ...outcomes,
            ]);
        if (!outcome)
            return null;
        outcome.task.settled = true;
        return outcome;
    }
    clearPendingHashFromPeers(hashKey, keepPeerId, recordTimeout = false) {
        for (const peer of this.peers.values()) {
            if (keepPeerId && peer.peerId === keepPeerId)
                continue;
            const pending = peer.pendingRequests.get(hashKey);
            if (!pending)
                continue;
            clearTimeout(pending.timeout);
            peer.pendingRequests.delete(hashKey);
            this.releasePeerRequest(peer.peerId);
            if (recordTimeout) {
                this.peerSelector.recordTimeout(peer.peerId);
            }
            pending.resolve(null);
        }
    }
    reservePeerRequest(peerId) {
        this.activePeerRequests.set(peerId, (this.activePeerRequests.get(peerId) ?? 0) + 1);
    }
    releasePeerRequest(peerId) {
        const next = (this.activePeerRequests.get(peerId) ?? 0) - 1;
        if (next <= 0) {
            this.activePeerRequests.delete(peerId);
            return;
        }
        this.activePeerRequests.set(peerId, next);
    }
    /**
     * Persist selector metadata snapshot to local store.
     * Returns the snapshot hash.
     */
    async persistPeerMetadata() {
        const snapshot = this.peerSelector.exportPeerMetadataSnapshot();
        const bytes = new TextEncoder().encode(JSON.stringify(snapshot));
        const snapshotHash = await sha256(bytes);
        await this.localStore.put(snapshotHash, bytes);
        const pointerHash = await this.peerMetadataPointerHash();
        await this.localStore.delete(pointerHash);
        await this.localStore.put(pointerHash, new TextEncoder().encode(toHex(snapshotHash)));
        return snapshotHash;
    }
    /**
     * Load selector metadata snapshot from local store.
     */
    async loadPeerMetadata() {
        const pointerHash = await this.peerMetadataPointerHash();
        const pointerBytes = await this.localStore.get(pointerHash);
        if (!pointerBytes)
            return false;
        const pointerHex = new TextDecoder().decode(pointerBytes).trim();
        if (pointerHex.length !== 64)
            return false;
        const snapshotHash = fromHex(pointerHex);
        if (snapshotHash.length !== 32)
            return false;
        const snapshotBytes = await this.localStore.get(snapshotHash);
        if (!snapshotBytes)
            return false;
        let snapshot;
        try {
            snapshot = JSON.parse(new TextDecoder().decode(snapshotBytes));
        }
        catch {
            return false;
        }
        this.peerSelector.importPeerMetadataSnapshot(snapshot);
        syncSelectorPeers(this.peerSelector, Array.from(this.peers.keys()));
        return true;
    }
    // ============================================================================
    // Data Protocol
    // ============================================================================
    sendDataToPeer(peer, data) {
        peer.stats.bytesSent += data.byteLength;
        this.sendCommand({ type: 'rtc:sendData', peerId: peer.peerId, data });
    }
    async onDataChannelMessage(peerId, data) {
        const peer = this.peers.get(peerId);
        if (!peer)
            return;
        // Count all inbound DataChannel bytes (requests + responses + protocol overhead).
        peer.stats.bytesReceived += data.byteLength;
        const msg = parseMessage(data);
        if (!msg) {
            this.log(`Failed to parse message from ${peerId}`);
            return;
        }
        if (msg.type === MSG_TYPE_REQUEST) {
            await this.handleRequest(peer, msg.body);
        }
        else if (msg.type === MSG_TYPE_RESPONSE) {
            await this.handleResponse(peer, msg.body);
        }
    }
    async handleRequest(peer, req) {
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
    async processRequest(peer, req) {
        await this.meshRouter.handleRequest(peer.peerId, req);
    }
    isFragmentedResponse(res) {
        return Number.isInteger(res.i) && Number.isInteger(res.n) && (res.n ?? 0) > 0;
    }
    handleFragmentResponse(peer, res) {
        const hashKey = hashToKey(res.h);
        const fragmentIndex = res.i ?? 0;
        const totalExpected = res.n ?? 0;
        if (fragmentIndex < 0 || totalExpected <= 0 || fragmentIndex >= totalExpected) {
            return null;
        }
        let pending = peer.pendingReassemblies.get(hashKey);
        if (!pending) {
            pending = {
                fragments: new Map(),
                totalExpected,
                receivedBytes: 0,
                timeout: setTimeout(() => {
                    peer.pendingReassemblies.delete(hashKey);
                    const pendingRequest = peer.pendingRequests.get(hashKey);
                    if (!pendingRequest) {
                        return;
                    }
                    clearTimeout(pendingRequest.timeout);
                    peer.pendingRequests.delete(hashKey);
                    this.releasePeerRequest(peer.peerId);
                    pendingRequest.resolve(null);
                }, this.fragmentStallTimeoutMs()),
            };
            peer.pendingReassemblies.set(hashKey, pending);
        }
        else {
            clearTimeout(pending.timeout);
            pending.timeout = setTimeout(() => {
                peer.pendingReassemblies.delete(hashKey);
                const pendingRequest = peer.pendingRequests.get(hashKey);
                if (!pendingRequest) {
                    return;
                }
                clearTimeout(pendingRequest.timeout);
                peer.pendingRequests.delete(hashKey);
                this.releasePeerRequest(peer.peerId);
                pendingRequest.resolve(null);
            }, this.fragmentStallTimeoutMs());
        }
        const pendingRequest = peer.pendingRequests.get(hashKey);
        if (pendingRequest) {
            this.resetPendingRequestTimeout(peer, hashKey, pendingRequest);
        }
        if (!pending.fragments.has(fragmentIndex)) {
            const fragment = res.d.slice();
            pending.fragments.set(fragmentIndex, fragment);
            pending.receivedBytes += fragment.byteLength;
        }
        if (pending.fragments.size !== pending.totalExpected) {
            return null;
        }
        clearTimeout(pending.timeout);
        peer.pendingReassemblies.delete(hashKey);
        const assembled = new Uint8Array(pending.receivedBytes);
        let offset = 0;
        for (let index = 0; index < pending.totalExpected; index += 1) {
            const fragment = pending.fragments.get(index);
            if (!fragment) {
                return null;
            }
            assembled.set(fragment, offset);
            offset += fragment.byteLength;
        }
        return assembled;
    }
    async handleResponse(peer, res) {
        peer.stats.responsesReceived++;
        const hashKey = hashToKey(res.h);
        const responseData = this.isFragmentedResponse(res)
            ? this.handleFragmentResponse(peer, res)
            : res.d.slice();
        if (!responseData) {
            return;
        }
        const pending = peer.pendingRequests.get(hashKey);
        if (!pending) {
            const hasRequesters = this.meshRouter.hasInFlight(hashKey);
            // Late response: cache if we requested this hash recently
            const requestedAt = this.recentRequests.get(hashKey);
            if (!requestedAt && !hasRequesters)
                return;
            if (requestedAt && Date.now() - requestedAt > 60000) {
                this.recentRequests.delete(hashKey);
                if (!hasRequesters)
                    return;
            }
            const valid = await verifyHash(responseData, res.h);
            if (valid) {
                const stableData = responseData.slice();
                await this.localStore.put(res.h, stableData);
                if (requestedAt) {
                    this.recentRequests.delete(hashKey);
                }
                if (hasRequesters) {
                    await this.meshRouter.resolve(res.h, stableData);
                }
            }
            return;
        }
        clearTimeout(pending.timeout);
        peer.pendingRequests.delete(hashKey);
        this.releasePeerRequest(peer.peerId);
        // Verify hash
        const valid = await verifyHash(responseData, res.h);
        const elapsedMs = pending.startedAt ? Math.max(1, Date.now() - pending.startedAt) : this.requestTimeout;
        if (valid) {
            const stableData = responseData.slice();
            // Store locally
            await this.localStore.put(res.h, stableData);
            this.peerSelector.recordSuccess(peer.peerId, elapsedMs, stableData.length);
            pending.resolve(stableData);
            await this.meshRouter.resolve(res.h, stableData);
        }
        else {
            this.log(`Hash mismatch from ${peer.peerId}`);
            this.peerSelector.recordFailure(peer.peerId);
            pending.resolve(null);
        }
    }
    async sendResponse(peer, hash, data) {
        if (!peer.dataChannelReady)
            return;
        peer.stats.responsesSent++;
        // Fragment if needed
        if (data.length > FRAGMENT_SIZE) {
            const totalFragments = Math.ceil(data.length / FRAGMENT_SIZE);
            for (let i = 0; i < totalFragments; i++) {
                const start = i * FRAGMENT_SIZE;
                const end = Math.min(start + FRAGMENT_SIZE, data.length);
                const fragment = data.slice(start, end);
                const res = createFragmentResponse(hash, fragment, i, totalFragments);
                const encoded = new Uint8Array(encodeResponse(res));
                this.sendDataToPeer(peer, encoded);
            }
        }
        else {
            const res = createResponse(hash, data);
            const encoded = new Uint8Array(encodeResponse(res));
            this.sendDataToPeer(peer, encoded);
        }
    }
    fragmentStallTimeoutMs() {
        return Math.max(MIN_FRAGMENT_STALL_TIMEOUT_MS, this.requestTimeout);
    }
    sendRequestToPeer(peer, hash, htl) {
        if (!peer.dataChannelReady) {
            return false;
        }
        const encoded = encodeForwardRequest(hash, htl);
        this.sendDataToPeer(peer, encoded);
        return true;
    }
    async queryPeersWithDispatch(hash, options) {
        const orderedPeers = this.orderedConnectedPeers(options.excludePeerId);
        if (orderedPeers.length === 0)
            return null;
        const dispatch = normalizeDispatchConfig(this.routing.dispatch, orderedPeers.length);
        const wavePlan = buildHedgedWavePlan(orderedPeers.length, dispatch);
        if (wavePlan.length === 0)
            return null;
        const deadline = Date.now() + this.requestTimeout;
        const inFlight = [];
        let nextPeerIdx = 0;
        for (let waveIdx = 0; waveIdx < wavePlan.length; waveIdx++) {
            const waveSize = wavePlan[waveIdx];
            const from = nextPeerIdx;
            const to = Math.min(from + waveSize, orderedPeers.length);
            nextPeerIdx = to;
            for (const peer of orderedPeers.slice(from, to)) {
                inFlight.push(this.createInFlightRequest(peer, hash, options.htl));
            }
            const isLastWave = waveIdx === wavePlan.length - 1 || nextPeerIdx >= orderedPeers.length;
            const windowEnd = isLastWave
                ? deadline
                : Math.min(deadline, Date.now() + dispatch.hedgeIntervalMs);
            while (isLastWave || Date.now() < windowEnd) {
                const remaining = isLastWave ? undefined : windowEnd - Date.now();
                const result = await this.waitForInFlightResult(inFlight, remaining);
                if (!result)
                    break;
                if (!result.data)
                    continue;
                this.clearPendingHashFromPeers(hashToKey(hash), result.task.peerId);
                return result.data;
            }
            if (!isLastWave && Date.now() >= deadline)
                break;
        }
        this.clearPendingHashFromPeers(hashToKey(hash), undefined, true);
        return null;
    }
    // ============================================================================
    // Public API
    // ============================================================================
    /**
     * Request data from peers
     */
    async get(hash) {
        const hashKey = hashToKey(hash);
        this.recentRequests.set(hashKey, Date.now());
        return this.queryPeersWithDispatch(hash, { htl: MAX_HTL });
    }
    getConnectedPeerIds(excludePeerId) {
        return this.orderedConnectedPeers(excludePeerId).map((peer) => peer.peerId);
    }
    async getFromPeer(peerId, hash, htl = MAX_HTL) {
        const peer = this.peers.get(peerId);
        if (!peer || !peer.dataChannelReady || !peer.hashGet) {
            return null;
        }
        const hashKey = hashToKey(hash);
        this.recentRequests.set(hashKey, Date.now());
        return (await this.createInFlightRequest(peer, hash, htl).promise).data;
    }
    /**
     * Get peer stats for UI
     */
    getPeerStats() {
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
            forwardedRequests: peer.stats.forwardedRequests,
            forwardedResolved: peer.stats.forwardedResolved,
            forwardedSuppressed: peer.stats.forwardedSuppressed,
        }));
    }
    /**
     * Get connected peer count
     */
    getConnectedCount() {
        let count = 0;
        for (const peer of this.peers.values()) {
            if (peer.state === 'connected' && peer.dataChannelReady) {
                count++;
            }
        }
        return count;
    }
    getConnectedHashGetPeerIds() {
        return Array.from(this.peers.values())
            .filter((peer) => peer.state === 'connected' && peer.dataChannelReady && peer.hashGet)
            .map((peer) => peer.peerId)
            .sort();
    }
    /**
     * Set pool configuration
     */
    setPoolConfig(config) {
        this.poolConfig = normalizePoolConfig(config);
        this.log('Pool config updated:', this.poolConfig);
        // Re-broadcast hello to trigger peer discovery with new limits
        this.sendHello();
    }
    setForwardRateLimit(config) {
        this.meshRouter.setForwardRateLimit(config);
    }
    /**
     * Update identity (pubkey) and restart signaling if already running.
     * This keeps peerId consistent with the current account.
     */
    setIdentity(pubkey) {
        if (this.myPeerId.pubkey === pubkey)
            return;
        const wasStarted = !!this.helloInterval;
        this.stop();
        this.myPeerId = new PeerId(pubkey);
        if (wasStarted) {
            this.start();
        }
    }
    // ============================================================================
    // Helpers
    // ============================================================================
    log(...args) {
        if (this.debug) {
            console.log('[WebRTC]', ...args);
        }
    }
}
//# sourceMappingURL=webrtcController.js.map