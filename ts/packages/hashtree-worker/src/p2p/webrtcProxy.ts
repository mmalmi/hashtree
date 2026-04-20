/**
 * WebRTC Proxy
 *
 * Thin transport layer that manages RTCPeerConnection in main thread.
 * Worker controls all logic - this just executes commands and reports events.
 *
 * Main thread owns RTCPeerConnection because it's not available in workers.
 * See: https://github.com/w3c/webrtc-extensions/issues/77
 */

import type { WebRTCCommand, WebRTCEvent } from './protocol.js';
import { BoundedQueue } from './boundedQueue.js';
import { getErrorMessage } from './errorMessage.js';
import { UploadRateLimiter } from './uploadRateLimiter.js';

const REQUEST_MESSAGE_TYPE = 0x00;

const isTestMode = typeof globalThis !== 'undefined' &&
  Boolean((globalThis as { __HTREE_P2P_TEST_MODE__?: boolean }).__HTREE_P2P_TEST_MODE__);
const ICE_SERVERS: RTCIceServer[] = isTestMode
  ? []
  : [
      { urls: 'stun:stun.l.google.com:19302' },
      { urls: 'stun:stun.cloudflare.com:3478' },
    ];

interface PeerConnection {
  pc: RTCPeerConnection;
  dataChannel: RTCDataChannel | null;
  pubkey: string;
  pendingCandidates: RTCIceCandidateInit[];
  sendQueue: BoundedQueue<Uint8Array>;
  bufferHighSignaled: boolean;  // Track if we've signaled high buffer to worker
  bytesSent: number;
  bytesReceived: number;
  bandwidthDebt: number;
  queueSequence: number;
}

type EventCallback = (event: WebRTCEvent) => void;
type WebRTCProxyConfig = {
  maxUploadBytesPerSecond?: number | null;
};

export class WebRTCProxy {
  private peers = new Map<string, PeerConnection>();
  private onEvent: EventCallback;
  private readonly uploadRateLimiter: UploadRateLimiter;
  private draining = false;
  private drainTimeoutId: ReturnType<typeof setTimeout> | null = null;
  private nextQueueSequence = 1;

  // Queue limits to prevent memory blowup on slow/stalled connections
  private static readonly MAX_QUEUE_BYTES = 8 * 1024 * 1024;  // 8MB per peer
  private static readonly MAX_QUEUE_ITEMS = 100;

  constructor(onEvent: EventCallback, config: WebRTCProxyConfig = {}) {
    this.onEvent = onEvent;
    this.uploadRateLimiter = new UploadRateLimiter({
      bytesPerSecond: config.maxUploadBytesPerSecond,
    });
  }

  private createSendQueue(peerId: string): BoundedQueue<Uint8Array> {
    return new BoundedQueue<Uint8Array>({
      maxItems: WebRTCProxy.MAX_QUEUE_ITEMS,
      maxBytes: WebRTCProxy.MAX_QUEUE_BYTES,
      getBytes: (item) => item.byteLength,
      onDrop: (item) => {
        console.warn(`[WebRTCProxy] Queue overflow for ${peerId.slice(0, 8)}, dropped ${item.byteLength}B`);
      },
    });
  }

  /**
   * Handle command from worker
   */
  handleCommand(cmd: WebRTCCommand): void {
    switch (cmd.type) {
      case 'rtc:createPeer':
        this.createPeer(cmd.peerId, cmd.pubkey);
        break;
      case 'rtc:closePeer':
        this.closePeer(cmd.peerId);
        break;
      case 'rtc:createOffer':
        this.createOffer(cmd.peerId);
        break;
      case 'rtc:createAnswer':
        this.createAnswer(cmd.peerId);
        break;
      case 'rtc:setLocalDescription':
        this.setLocalDescription(cmd.peerId, cmd.sdp);
        break;
      case 'rtc:setRemoteDescription':
        this.setRemoteDescription(cmd.peerId, cmd.sdp);
        break;
      case 'rtc:addIceCandidate':
        this.addIceCandidate(cmd.peerId, cmd.candidate);
        break;
      case 'rtc:sendData':
        this.sendData(cmd.peerId, cmd.data);
        break;
    }
  }

  private createPeer(peerId: string, pubkey: string): void {
    // Clean up existing if present
    if (this.peers.has(peerId)) {
      this.closePeer(peerId);
    }

    const pc = new RTCPeerConnection({ iceServers: ICE_SERVERS });

    const peer: PeerConnection = {
      pc,
      dataChannel: null,
      pubkey,
      pendingCandidates: [],
      sendQueue: this.createSendQueue(peerId),
      bufferHighSignaled: false,
      bytesSent: 0,
      bytesReceived: 0,
      bandwidthDebt: 0,
      queueSequence: 0,
    };

    // Only the offerer should create the data channel. The answerer receives
    // it via ondatachannel after the remote offer is applied.
    pc.ondatachannel = (event) => {
      this.setupDataChannel(peerId, event.channel);
      peer.dataChannel = event.channel;
    };

    // ICE candidate gathering
    pc.onicecandidate = (event) => {
      this.onEvent({
        type: 'rtc:iceCandidate',
        peerId,
        candidate: event.candidate?.toJSON() ?? null,
      });

      if (!event.candidate) {
        this.onEvent({ type: 'rtc:iceGatheringComplete', peerId });
      }
    };

    // Connection state changes
    pc.onconnectionstatechange = () => {
      this.onEvent({
        type: 'rtc:peerStateChange',
        peerId,
        state: pc.connectionState,
      });

      if (pc.connectionState === 'closed' || pc.connectionState === 'failed') {
        this.cleanupPeer(peerId);
      }
    };

    this.peers.set(peerId, peer);
    this.onEvent({ type: 'rtc:peerCreated', peerId });
  }

  private setupDataChannel(peerId: string, dc: RTCDataChannel): void {
    dc.binaryType = 'arraybuffer';

    dc.onopen = () => {
      this.onEvent({ type: 'rtc:dataChannelOpen', peerId });
    };

    // If channel is already open (can happen with ondatachannel), fire event immediately
    if (dc.readyState === 'open') {
      this.onEvent({ type: 'rtc:dataChannelOpen', peerId });
    }

    dc.onclose = () => {
      this.onEvent({ type: 'rtc:dataChannelClose', peerId });
    };

    dc.onerror = (event) => {
      const errorEvent = event as RTCErrorEvent;
      this.onEvent({
        type: 'rtc:dataChannelError',
        peerId,
        error: errorEvent.error?.message || 'Unknown error',
      });
    };

    dc.onmessage = (event) => {
      const data = event.data instanceof ArrayBuffer
        ? new Uint8Array(event.data)
        : new Uint8Array(0);
      const peer = this.peers.get(peerId);
      if (peer) {
        peer.bytesReceived += data.byteLength;
      }

      this.onEvent({
        type: 'rtc:dataChannelMessage',
        peerId,
        data,
      });
    };
  }

  private async createOffer(peerId: string): Promise<void> {
    const peer = this.peers.get(peerId);
    if (!peer) return;

    try {
      if (!peer.dataChannel || peer.dataChannel.readyState === 'closed') {
        const dc = peer.pc.createDataChannel('hashtree', {
          ordered: true,
        });
        this.setupDataChannel(peerId, dc);
        peer.dataChannel = dc;
      }
      const offer = await peer.pc.createOffer();
      this.onEvent({
        type: 'rtc:offerCreated',
        peerId,
        sdp: offer,
      });
    } catch (err) {
      console.error('[WebRTCProxy] Failed to create offer:', err);
    }
  }

  private async createAnswer(peerId: string): Promise<void> {
    const peer = this.peers.get(peerId);
    if (!peer) return;

    try {
      const answer = await peer.pc.createAnswer();
      this.onEvent({
        type: 'rtc:answerCreated',
        peerId,
        sdp: answer,
      });
    } catch (err) {
      console.error('[WebRTCProxy] Failed to create answer:', err);
    }
  }

  private async setLocalDescription(peerId: string, sdp: RTCSessionDescriptionInit): Promise<void> {
    const peer = this.peers.get(peerId);
    if (!peer) return;

    try {
      await peer.pc.setLocalDescription(sdp);
      this.onEvent({ type: 'rtc:descriptionSet', peerId });
    } catch (err) {
      this.onEvent({
        type: 'rtc:descriptionSet',
        peerId,
        error: getErrorMessage(err),
      });
    }
  }

  private async setRemoteDescription(peerId: string, sdp: RTCSessionDescriptionInit): Promise<void> {
    const peer = this.peers.get(peerId);
    if (!peer) return;

    try {
      if (sdp.type === 'offer'
        && peer.pc.signalingState !== 'stable'
        && peer.pc.signalingState !== 'closed') {
        await peer.pc.setLocalDescription({ type: 'rollback' });
      }

      await peer.pc.setRemoteDescription(sdp);

      // Apply any pending ICE candidates
      for (const candidate of peer.pendingCandidates) {
        await peer.pc.addIceCandidate(candidate);
      }
      peer.pendingCandidates = [];

      this.onEvent({ type: 'rtc:descriptionSet', peerId });
    } catch (err) {
      this.onEvent({
        type: 'rtc:descriptionSet',
        peerId,
        error: getErrorMessage(err),
      });
    }
  }

  private async addIceCandidate(peerId: string, candidate: RTCIceCandidateInit): Promise<void> {
    const peer = this.peers.get(peerId);
    if (!peer) return;

    // Queue if remote description not set yet
    if (!peer.pc.remoteDescription) {
      peer.pendingCandidates.push(candidate);
      return;
    }

    try {
      await peer.pc.addIceCandidate(candidate);
    } catch (err) {
      console.error('[WebRTCProxy] Failed to add ICE candidate:', err);
    }
  }

  // 256KB threshold - pause sending when buffer exceeds this
  private static readonly BUFFER_THRESHOLD = 256 * 1024;
  // 4MB threshold for sendQueue - signal worker to pause when exceeded
  private static readonly QUEUE_HIGH_THRESHOLD = 4 * 1024 * 1024;
  // 1MB threshold for sendQueue - signal worker to resume when below
  private static readonly QUEUE_LOW_THRESHOLD = 1 * 1024 * 1024;

  private getQueueSize(peer: PeerConnection): number {
    return peer.sendQueue.bytes;
  }

  private isPriorityDataMessage(data: Uint8Array): boolean {
    return data.byteLength > 0 && data[0] === REQUEST_MESSAGE_TYPE;
  }

  private sendData(peerId: string, data: Uint8Array): void {
    const peer = this.peers.get(peerId);
    if (!peer?.dataChannel || peer.dataChannel.readyState !== 'open') {
      return;
    }

    const wasEmpty = peer.sendQueue.isEmpty;

    // Small request frames should overtake bulky response traffic so cache misses
    // are not starved by background uploads on the same peer connection.
    if (this.isPriorityDataMessage(data)) {
      peer.sendQueue.unshift(data);
    } else {
      peer.sendQueue.push(data);
    }

    if (wasEmpty) {
      peer.queueSequence = this.nextQueueSequence++;
    }

    // Check if queue is getting too large - signal worker to slow down
    const queueSize = this.getQueueSize(peer);
    if (!peer.bufferHighSignaled && queueSize > WebRTCProxy.QUEUE_HIGH_THRESHOLD) {
      peer.bufferHighSignaled = true;
      this.onEvent({ type: 'rtc:bufferHigh', peerId });
    }

    this.drainQueuedPeers();
  }

  private drainQueuedPeers(): void {
    if (this.draining) {
      return;
    }
    if (this.drainTimeoutId) {
      clearTimeout(this.drainTimeoutId);
      this.drainTimeoutId = null;
    }

    this.draining = true;
    try {
      while (true) {
        const next = this.selectNextQueuedPeer();
        if (!next) {
          break;
        }

        const reservation = this.uploadRateLimiter.reserve(next.data.byteLength);
        if (!reservation.allowed) {
          this.scheduleRateLimitedDrain(reservation.delayMs);
          break;
        }

        const peer = next.peer;
        const dc = peer.dataChannel;
        if (!dc || dc.readyState !== 'open') {
          peer.sendQueue.shift();
          if (peer.sendQueue.isEmpty) {
            peer.queueSequence = 0;
          }
          continue;
        }

        const data = peer.sendQueue.shift();
        if (!data) {
          continue;
        }

        try {
          const payload = data.byteOffset === 0 && data.byteLength === data.buffer.byteLength
            ? data.buffer
            : data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength);
          dc.send(payload as ArrayBuffer);
          peer.bytesSent += data.byteLength;
          peer.bandwidthDebt = next.finish;
        } catch {
          // Drop on failure instead of infinite re-queue (prevents memory blowup)
          console.warn(`[WebRTCProxy] Send failed for ${next.peerId.slice(0, 8)}, dropped ${data.byteLength}B`);
        }

        if (peer.sendQueue.isEmpty) {
          peer.queueSequence = 0;
        }
        this.maybeSignalBufferLow(next.peerId, peer);
        this.normalizeBandwidthDebt();
      }
    } finally {
      this.draining = false;
    }

    this.refreshBufferedAmountWatchers();
    if (!this.hasQueuedTraffic()) {
      this.resetBandwidthDebt();
    }
  }

  private selectNextQueuedPeer(): { peerId: string; peer: PeerConnection; data: Uint8Array; finish: number } | null {
    const ready: Array<{ peerId: string; peer: PeerConnection; data: Uint8Array; finish: number }> = [];
    const readyPriority: Array<{ peerId: string; peer: PeerConnection; data: Uint8Array; finish: number }> = [];

    for (const [peerId, peer] of this.peers) {
      const dc = peer.dataChannel;
      if (!dc || dc.readyState !== 'open' || peer.sendQueue.isEmpty) {
        continue;
      }
      if (dc.bufferedAmount >= WebRTCProxy.BUFFER_THRESHOLD) {
        continue;
      }

      const data = peer.sendQueue.peek();
      if (!data) {
        continue;
      }

      const weight = this.reciprocityWeight(peer);
      const finish = peer.bandwidthDebt + data.byteLength / weight;
      const entry = { peerId, peer, data, finish };
      ready.push(entry);
      if (this.isPriorityDataMessage(data)) {
        readyPriority.push(entry);
      }
    }

    const candidates = readyPriority.length > 0 ? readyPriority : ready;
    if (candidates.length === 0) {
      return null;
    }

    candidates.sort((left, right) =>
      (left.finish - right.finish)
      || (left.peer.queueSequence - right.peer.queueSequence)
      || left.peerId.localeCompare(right.peerId));
    return candidates[0] ?? null;
  }

  private reciprocityWeight(peer: PeerConnection): number {
    const sent = peer.bytesSent;
    const received = peer.bytesReceived;
    const rawRatio = (received + 1024) / (sent + 1024);
    const boundedRatio = rawRatio / (1 + rawRatio);
    return 0.5 + 1.5 * boundedRatio;
  }

  private normalizeBandwidthDebt(): void {
    const queued = Array.from(this.peers.values()).filter((peer) => !peer.sendQueue.isEmpty);
    if (queued.length === 0) {
      this.resetBandwidthDebt();
      return;
    }

    const floor = Math.min(...queued.map((peer) => peer.bandwidthDebt));
    if (!Number.isFinite(floor) || floor <= 0) {
      return;
    }

    for (const peer of queued) {
      peer.bandwidthDebt = Math.max(0, peer.bandwidthDebt - floor);
    }
  }

  private resetBandwidthDebt(): void {
    for (const peer of this.peers.values()) {
      peer.bandwidthDebt = 0;
      if (peer.sendQueue.isEmpty) {
        peer.queueSequence = 0;
      }
    }
  }

  private hasQueuedTraffic(): boolean {
    for (const peer of this.peers.values()) {
      if (!peer.sendQueue.isEmpty) {
        return true;
      }
    }
    return false;
  }

  private maybeSignalBufferLow(peerId: string, peer: PeerConnection): void {
    if (!peer.bufferHighSignaled) {
      return;
    }
    const queueSize = this.getQueueSize(peer);
    if (queueSize < WebRTCProxy.QUEUE_LOW_THRESHOLD) {
      peer.bufferHighSignaled = false;
      this.onEvent({ type: 'rtc:bufferLow', peerId });
    }
  }

  private refreshBufferedAmountWatchers(): void {
    for (const [peerId, peer] of this.peers) {
      const dc = peer.dataChannel;
      if (!dc || dc.readyState !== 'open') {
        continue;
      }

      if (!peer.sendQueue.isEmpty && dc.bufferedAmount >= WebRTCProxy.BUFFER_THRESHOLD) {
        dc.bufferedAmountLowThreshold = WebRTCProxy.BUFFER_THRESHOLD / 2;
        dc.onbufferedamountlow = () => {
          dc.onbufferedamountlow = null;
          this.drainQueuedPeers();
        };
        continue;
      }

      dc.onbufferedamountlow = null;
    }
  }

  private closePeer(peerId: string): void {
    const peer = this.peers.get(peerId);
    if (!peer) return;

    this.cleanupPeer(peerId);
    this.onEvent({ type: 'rtc:peerClosed', peerId });
  }

  private cleanupPeer(peerId: string): void {
    const peer = this.peers.get(peerId);
    if (!peer) return;

    // Clear send queue
    peer.sendQueue.clear();

    // Close data channel
    if (peer.dataChannel) {
      peer.dataChannel.onopen = null;
      peer.dataChannel.onclose = null;
      peer.dataChannel.onerror = null;
      peer.dataChannel.onmessage = null;
      peer.dataChannel.onbufferedamountlow = null;
      peer.dataChannel.close();
    }

    // Close peer connection
    peer.pc.onicecandidate = null;
    peer.pc.ondatachannel = null;
    peer.pc.onconnectionstatechange = null;
    peer.pc.close();

    this.peers.delete(peerId);
    if (!this.hasQueuedTraffic()) {
      this.resetBandwidthDebt();
    }
  }

  /**
   * Close all connections
   */
  close(): void {
    for (const peerId of this.peers.keys()) {
      this.closePeer(peerId);
    }
  }

  /**
   * Get connected peer count
   */
  getConnectedCount(): number {
    let count = 0;
    for (const peer of this.peers.values()) {
      if (peer.pc.connectionState === 'connected' &&
          peer.dataChannel?.readyState === 'open') {
        count++;
      }
    }
    return count;
  }

  /**
   * Get all peer IDs
   */
  getPeerIds(): string[] {
    return Array.from(this.peers.keys());
  }

  setUploadLimitBytesPerSecond(maxUploadBytesPerSecond?: number | null): void {
    this.uploadRateLimiter.setBytesPerSecond(maxUploadBytesPerSecond);
    this.drainQueuedPeers();
  }

  private scheduleRateLimitedDrain(delayMs: number): void {
    if (this.drainTimeoutId) {
      return;
    }

    this.drainTimeoutId = setTimeout(() => {
      this.drainTimeoutId = null;
      this.drainQueuedPeers();
    }, delayMs);
  }
}

// Singleton instance
let instance: WebRTCProxy | null = null;

export function initWebRTCProxy(onEvent: EventCallback): WebRTCProxy {
  if (instance) {
    instance.close();
  }
  instance = new WebRTCProxy(onEvent);
  return instance;
}

export function getWebRTCProxy(): WebRTCProxy | null {
  return instance;
}

export function closeWebRTCProxy(): void {
  if (instance) {
    instance.close();
    instance = null;
  }
}
