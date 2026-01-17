/**
 * WebRTC Proxy
 *
 * Thin transport layer that manages RTCPeerConnection in main thread.
 * Worker controls all logic - this just executes commands and reports events.
 *
 * Main thread owns RTCPeerConnection because it's not available in workers.
 * See: https://github.com/w3c/webrtc-extensions/issues/77
 */

import type { WebRTCCommand, WebRTCEvent } from 'hashtree';
import { BoundedQueue } from './utils/boundedQueue';
import { getErrorMessage } from './utils/errorMessage';

const isTestMode = !!import.meta.env.VITE_TEST_MODE;
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
  sending: boolean;
  bufferHighSignaled: boolean;  // Track if we've signaled high buffer to worker
}

type EventCallback = (event: WebRTCEvent) => void;

export class WebRTCProxy {
  private peers = new Map<string, PeerConnection>();
  private onEvent: EventCallback;

  // Queue limits to prevent memory blowup on slow/stalled connections
  private static readonly MAX_QUEUE_BYTES = 8 * 1024 * 1024;  // 8MB per peer
  private static readonly MAX_QUEUE_ITEMS = 100;

  constructor(onEvent: EventCallback) {
    this.onEvent = onEvent;
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
      sending: false,
      bufferHighSignaled: false,
    };

    // Create data channel (offerer creates, answerer receives via ondatachannel)
    const dc = pc.createDataChannel('hashtree', {
      ordered: true,
    });
    this.setupDataChannel(peerId, dc);
    peer.dataChannel = dc;

    // Handle incoming data channel (for answerer)
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
    let size = 0;
    for (const data of peer.sendQueue) {
      size += data.length;
    }
    return size;
  }

  private sendData(peerId: string, data: Uint8Array): void {
    const peer = this.peers.get(peerId);
    if (!peer?.dataChannel || peer.dataChannel.readyState !== 'open') {
      return;
    }

    // BoundedQueue handles overflow automatically (drops oldest, logs via onDrop)
    peer.sendQueue.push(data);

    // Check if queue is getting too large - signal worker to slow down
    const queueSize = this.getQueueSize(peer);
    if (!peer.bufferHighSignaled && queueSize > WebRTCProxy.QUEUE_HIGH_THRESHOLD) {
      peer.bufferHighSignaled = true;
      this.onEvent({ type: 'rtc:bufferHigh', peerId });
    }

    // Start draining if not already
    if (!peer.sending) {
      this.drainQueue(peerId);
    }
  }

  private drainQueue(peerId: string): void {
    const peer = this.peers.get(peerId);
    if (!peer?.dataChannel || peer.dataChannel.readyState !== 'open') {
      return;
    }

    peer.sending = true;
    const dc = peer.dataChannel;

    // Send as much as we can without overflowing the buffer
    while (!peer.sendQueue.isEmpty && dc.bufferedAmount < WebRTCProxy.BUFFER_THRESHOLD) {
      const data = peer.sendQueue.shift()!;
      try {
        dc.send(data.buffer);
      } catch {
        // Drop on failure instead of infinite re-queue (prevents memory blowup)
        console.warn(`[WebRTCProxy] Send failed for ${peerId.slice(0, 8)}, dropped ${data.byteLength}B`);
        break;
      }
    }

    // Check if queue has drained enough to signal worker to resume
    if (peer.bufferHighSignaled) {
      const queueSize = this.getQueueSize(peer);
      if (queueSize < WebRTCProxy.QUEUE_LOW_THRESHOLD) {
        peer.bufferHighSignaled = false;
        this.onEvent({ type: 'rtc:bufferLow', peerId });
      }
    }

    // If there's more to send, wait for buffer to drain
    if (!peer.sendQueue.isEmpty) {
      dc.bufferedAmountLowThreshold = WebRTCProxy.BUFFER_THRESHOLD / 2;
      dc.onbufferedamountlow = () => {
        dc.onbufferedamountlow = null;
        this.drainQueue(peerId);
      };
    } else {
      peer.sending = false;
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
    peer.sending = false;

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
