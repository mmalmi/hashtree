import type { WorkerFactory } from './client.js';
import { WebRTCProxy } from './p2p/webrtcProxy.js';
import type {
  BlossomBandwidthStats,
  BlossomServerConfig,
  PeerStats as RelayPeerStats,
  RelayStats,
  SignedEvent as RelayWorkerSignedEvent,
  TreeRootInfo,
  UnsignedEvent as RelayWorkerUnsignedEvent,
  WorkerConfig as RelayWorkerConfig,
  WorkerRequest as RelayWorkerRequest,
  WorkerResponse as RelayWorkerResponse,
} from './relay/protocol.js';

const REQUEST_TIMEOUT_MS = 30_000;

type PendingRequest = {
  resolve: (message: RelayWorkerResponse) => void;
  reject: (error: Error) => void;
  timeoutId: ReturnType<typeof setTimeout>;
};

type NostrExtension = {
  signEvent?: (event: RelayWorkerUnsignedEvent) => Promise<RelayWorkerSignedEvent>;
  nip44?: {
    encrypt?: (pubkey: string, plaintext: string) => Promise<string>;
    decrypt?: (pubkey: string, ciphertext: string) => Promise<string>;
  };
};

type RelayWorkerRequestPayload = RelayWorkerRequest extends infer T
  ? T extends { id: string }
    ? Omit<T, 'id'>
    : never
  : never;

type SignEventMessage = Extract<RelayWorkerResponse, { type: 'signEvent' }>;
type EncryptMessage = Extract<RelayWorkerResponse, { type: 'nip44Encrypt' }>;
type DecryptMessage = Extract<RelayWorkerResponse, { type: 'nip44Decrypt' }>;
type RelayWorkerRtcCommand = Extract<
  RelayWorkerResponse,
  { type: 'rtc:createPeer' | 'rtc:closePeer' | 'rtc:createOffer' | 'rtc:createAnswer' | 'rtc:setLocalDescription' | 'rtc:setRemoteDescription' | 'rtc:addIceCandidate' | 'rtc:sendData' }
>;

const WEBRTC_COMMAND_TYPES = new Set([
  'rtc:createPeer',
  'rtc:closePeer',
  'rtc:createOffer',
  'rtc:createAnswer',
  'rtc:setLocalDescription',
  'rtc:setRemoteDescription',
  'rtc:addIceCandidate',
  'rtc:sendData',
]);

function isRelayWorkerRtcCommand(message: RelayWorkerResponse): message is RelayWorkerRtcCommand {
  return WEBRTC_COMMAND_TYPES.has(message.type);
}

export interface TreeRootUpdate extends TreeRootInfo {
  npub: string;
  treeName: string;
}

export interface RelayWorkerClientConfig extends RelayWorkerConfig {
  maxWebRTCUploadBytesPerSecond?: number | null;
}

export type {
  BlossomBandwidthStats,
  BlossomServerConfig,
  RelayPeerStats,
  RelayStats,
  TreeRootInfo,
  RelayWorkerConfig,
  RelayWorkerRequest,
  RelayWorkerResponse,
};

export class RelayWorkerClient {
  private readonly workerFactory: WorkerFactory;
  private readonly config: RelayWorkerClientConfig;
  private worker: Worker | null = null;
  private webrtcProxy: WebRTCProxy | null = null;
  private initPromise: Promise<void> | null = null;
  private initPending:
    | {
        resolve: () => void;
        reject: (error: Error) => void;
        timeoutId: ReturnType<typeof setTimeout>;
      }
    | null = null;
  private pendingRequests = new Map<string, PendingRequest>();
  private treeRootListeners = new Set<(update: TreeRootUpdate) => void>();
  private blossomBandwidthListeners = new Set<(stats: BlossomBandwidthStats) => void>();

  constructor(workerFactory: WorkerFactory, config: RelayWorkerClientConfig) {
    this.workerFactory = workerFactory;
    this.config = config;
  }

  async init(): Promise<void> {
    if (this.initPromise) return this.initPromise;

    try {
      this.spawnWorker();
    } catch (err) {
      throw err instanceof Error ? err : new Error(String(err));
    }

    this.initPromise = new Promise<void>((resolve, reject) => {
      if (!this.worker) {
        reject(new Error('Failed to create worker'));
        return;
      }

      const timeoutId = setTimeout(() => {
        this.initPending = null;
        this.initPromise = null;
        reject(new Error('Worker init timed out'));
      }, REQUEST_TIMEOUT_MS);

      this.initPending = {
        resolve,
        reject,
        timeoutId,
      };

      const { maxWebRTCUploadBytesPerSecond: _maxUploadBytesPerSecond, ...workerConfig } = this.config;
      this.worker.postMessage({
        type: 'init',
        id: this.nextRequestId('worker_init'),
        config: workerConfig as RelayWorkerConfig,
      } as RelayWorkerRequest);
    });

    return this.initPromise;
  }

  private spawnWorker(): void {
    if (this.workerFactory instanceof URL) {
      this.worker = new Worker(this.workerFactory, { type: 'module' });
    } else if (typeof this.workerFactory === 'string') {
      this.worker = new Worker(this.workerFactory, { type: 'module' });
    } else {
      this.worker = new this.workerFactory();
    }

    this.webrtcProxy = new WebRTCProxy((event) => {
      if (event.type === 'rtc:dataChannelMessage' && event.data?.buffer) {
        this.worker?.postMessage(event, [event.data.buffer]);
        return;
      }
      this.worker?.postMessage(event);
    }, {
      maxUploadBytesPerSecond: this.config.maxWebRTCUploadBytesPerSecond ?? null,
    });

    this.worker.onmessage = (event: MessageEvent<RelayWorkerResponse>) => {
      const message = event.data;

      if (message.type === 'ready') {
        if (this.initPending) {
          clearTimeout(this.initPending.timeoutId);
          this.initPending.resolve();
          this.initPending = null;
        }
        return;
      }

      if (message.type === 'blossomBandwidth') {
        for (const listener of this.blossomBandwidthListeners) {
          listener(message.stats);
        }
        return;
      }

      if (message.type === 'treeRootUpdate') {
        for (const listener of this.treeRootListeners) {
          const { type: _type, ...update } = message;
          listener(update);
        }
        return;
      }

      if (isRelayWorkerRtcCommand(message)) {
        this.webrtcProxy?.handleCommand(message);
        return;
      }

      if (message.type === 'signEvent') {
        void this.handleSignRequest(message);
        return;
      }

      if (message.type === 'nip44Encrypt') {
        void this.handleEncryptRequest(message);
        return;
      }

      if (message.type === 'nip44Decrypt') {
        void this.handleDecryptRequest(message);
        return;
      }

      if (message.type === 'error' && message.id) {
        const errorMessage = typeof message.error === 'string' ? message.error : 'Worker error';
        this.rejectPending(message.id, new Error(errorMessage));
        return;
      }

      if ('id' in message && typeof message.id === 'string') {
        this.resolvePending(message.id, message);
      }
    };

    this.worker.onerror = (event) => {
      const errorMessage = event instanceof ErrorEvent ? event.message : 'Worker error';
      this.webrtcProxy?.close();
      this.webrtcProxy = null;
      this.rejectAllPending(new Error(errorMessage));
    };
  }

  private getNostrExtension(): NostrExtension | null {
    if (typeof window === 'undefined') {
      return null;
    }

    return (window as typeof window & { nostr?: NostrExtension }).nostr ?? null;
  }

  private async handleSignRequest(message: SignEventMessage): Promise<void> {
    try {
      const nostr = this.getNostrExtension();
      if (!nostr?.signEvent) {
        throw new Error('NIP-07 extension not available');
      }

      const signed = await nostr.signEvent(message.event);
      this.worker?.postMessage({
        type: 'signed',
        id: message.id,
        event: signed,
      } satisfies RelayWorkerRequest);
    } catch (error) {
      this.worker?.postMessage({
        type: 'signed',
        id: message.id,
        error: error instanceof Error ? error.message : String(error),
      } satisfies RelayWorkerRequest);
    }
  }

  private async handleEncryptRequest(message: EncryptMessage): Promise<void> {
    try {
      const nostr = this.getNostrExtension();
      if (!nostr?.nip44?.encrypt) {
        throw new Error('NIP-44 encryption not available');
      }

      const ciphertext = await nostr.nip44.encrypt(message.pubkey, message.plaintext);
      this.worker?.postMessage({
        type: 'encrypted',
        id: message.id,
        ciphertext,
      } satisfies RelayWorkerRequest);
    } catch (error) {
      this.worker?.postMessage({
        type: 'encrypted',
        id: message.id,
        error: error instanceof Error ? error.message : String(error),
      } satisfies RelayWorkerRequest);
    }
  }

  private async handleDecryptRequest(message: DecryptMessage): Promise<void> {
    try {
      const nostr = this.getNostrExtension();
      if (!nostr?.nip44?.decrypt) {
        throw new Error('NIP-44 decryption not available');
      }

      const plaintext = await nostr.nip44.decrypt(message.pubkey, message.ciphertext);
      this.worker?.postMessage({
        type: 'decrypted',
        id: message.id,
        plaintext,
      } satisfies RelayWorkerRequest);
    } catch (error) {
      this.worker?.postMessage({
        type: 'decrypted',
        id: message.id,
        error: error instanceof Error ? error.message : String(error),
      } satisfies RelayWorkerRequest);
    }
  }

  private nextRequestId(prefix: string): string {
    if (typeof crypto !== 'undefined' && 'randomUUID' in crypto) {
      return `${prefix}_${crypto.randomUUID()}`;
    }
    return `${prefix}_${Date.now().toString(36)}_${Math.random().toString(36).slice(2)}`;
  }

  private resolvePending(id: string, message: RelayWorkerResponse): void {
    const pending = this.pendingRequests.get(id);
    if (!pending) return;
    clearTimeout(pending.timeoutId);
    pending.resolve(message);
    this.pendingRequests.delete(id);
  }

  private rejectPending(id: string, error: Error): void {
    const pending = this.pendingRequests.get(id);
    if (!pending) return;
    clearTimeout(pending.timeoutId);
    pending.reject(error);
    this.pendingRequests.delete(id);
  }

  private rejectAllPending(error: Error): void {
    for (const [id, pending] of this.pendingRequests.entries()) {
      clearTimeout(pending.timeoutId);
      pending.reject(error);
      this.pendingRequests.delete(id);
    }

    if (this.initPending) {
      clearTimeout(this.initPending.timeoutId);
      this.initPending.reject(error);
      this.initPending = null;
    }

    this.initPromise = null;
  }

  private async request(
    payload: RelayWorkerRequestPayload,
    timeoutMs = REQUEST_TIMEOUT_MS,
    transfer: Transferable[] = [],
  ): Promise<RelayWorkerResponse> {
    await this.init();
    if (!this.worker) {
      throw new Error('Worker not initialized');
    }

    const id = this.nextRequestId(payload.type);
    const message = { ...payload, id } as RelayWorkerRequest;

    return new Promise<RelayWorkerResponse>((resolve, reject) => {
      const timeoutId = setTimeout(() => {
        this.pendingRequests.delete(id);
        reject(new Error(`Worker request timed out: ${payload.type}`));
      }, timeoutMs);

      this.pendingRequests.set(id, { resolve, reject, timeoutId });
      this.worker?.postMessage(message, transfer);
    });
  }

  async registerMediaPort(port: MessagePort, debug?: boolean): Promise<void> {
    await this.init();
    if (!this.worker) {
      throw new Error('Worker not initialized');
    }

    this.worker.postMessage({ type: 'registerMediaPort', port, debug } as RelayWorkerRequest, [port]);
  }

  async getTreeRootInfo(npub: string, treeName: string): Promise<TreeRootInfo | null> {
    const res = await this.request({ type: 'getTreeRootInfo', npub, treeName });
    if (res.type !== 'treeRootInfo') {
      throw new Error('Unexpected tree root response');
    }
    if (res.error) {
      throw new Error(res.error);
    }
    return res.record ?? null;
  }

  async getPeerStats(): Promise<RelayPeerStats[]> {
    const res = await this.request({ type: 'getPeerStats' });
    if (res.type !== 'peerStats') {
      throw new Error('Unexpected peer stats response');
    }
    return res.stats ?? [];
  }

  async getRelayStats(): Promise<RelayStats[]> {
    const res = await this.request({ type: 'getRelayStats' });
    if (res.type !== 'relayStats') {
      throw new Error('Unexpected relay stats response');
    }
    return res.stats ?? [];
  }

  async setIdentity(pubkey: string, nsecHex?: string): Promise<void> {
    const res = await this.request({ type: 'setIdentity', pubkey, nsec: nsecHex });
    if (res.type !== 'void') {
      throw new Error('Unexpected setIdentity response');
    }
    if (res.error) {
      throw new Error(res.error);
    }
  }

  async setWebRTCPools(
    pools: { follows: { max: number; satisfied: number }; other: { max: number; satisfied: number } },
  ): Promise<void> {
    const res = await this.request({ type: 'setWebRTCPools', pools });
    if (res.type !== 'void') {
      throw new Error('Unexpected setWebRTCPools response');
    }
    if (res.error) {
      throw new Error(res.error);
    }
  }

  setUploadLimitBytesPerSecond(maxUploadBytesPerSecond?: number | null): void {
    this.config.maxWebRTCUploadBytesPerSecond = maxUploadBytesPerSecond ?? null;
    this.webrtcProxy?.setUploadLimitBytesPerSecond(maxUploadBytesPerSecond ?? null);
  }

  async setFollows(follows: string[]): Promise<void> {
    const res = await this.request({ type: 'setFollows', follows });
    if (res.type !== 'void') {
      throw new Error('Unexpected setFollows response');
    }
    if (res.error) {
      throw new Error(res.error);
    }
  }

  async sendHello(): Promise<void> {
    const res = await this.request({ type: 'sendWebRTCHello' });
    if (res.type !== 'void') {
      throw new Error('Unexpected sendWebRTCHello response');
    }
    if (res.error) {
      throw new Error(res.error);
    }
  }

  async setBlossomServers(servers: BlossomServerConfig[]): Promise<void> {
    const res = await this.request({ type: 'setBlossomServers', servers });
    if (res.type !== 'void') {
      throw new Error('Unexpected setBlossomServers response');
    }
    if (res.error) {
      throw new Error(res.error);
    }
  }

  async setStorageMaxBytes(maxBytes: number): Promise<void> {
    const res = await this.request({ type: 'setStorageMaxBytes', maxBytes });
    if (res.type !== 'void') {
      throw new Error('Unexpected setStorageMaxBytes response');
    }
    if (res.error) {
      throw new Error(res.error);
    }
  }

  async setRelays(relays: string[]): Promise<void> {
    const res = await this.request({ type: 'setRelays', relays });
    if (res.type !== 'void') {
      throw new Error('Unexpected setRelays response');
    }
    if (res.error) {
      throw new Error(res.error);
    }
  }

  async subscribeTreeRoots(pubkey: string): Promise<void> {
    const res = await this.request({ type: 'subscribeTreeRoots', pubkey });
    if (res.type !== 'void') {
      throw new Error('Unexpected tree root subscribe response');
    }
    if (res.error) {
      throw new Error(res.error);
    }
  }

  async unsubscribeTreeRoots(pubkey: string): Promise<void> {
    const res = await this.request({ type: 'unsubscribeTreeRoots', pubkey });
    if (res.type !== 'void') {
      throw new Error('Unexpected tree root unsubscribe response');
    }
    if (res.error) {
      throw new Error(res.error);
    }
  }

  onTreeRootUpdate(listener: (update: TreeRootUpdate) => void): () => void {
    this.treeRootListeners.add(listener);
    return () => {
      this.treeRootListeners.delete(listener);
    };
  }

  onBlossomBandwidth(listener: (stats: BlossomBandwidthStats) => void): () => void {
    this.blossomBandwidthListeners.add(listener);
    return () => {
      this.blossomBandwidthListeners.delete(listener);
    };
  }

  async close(): Promise<void> {
    try {
      const res = await this.request({ type: 'close' });
      if (res.type !== 'void' && res.type !== 'error') {
        throw new Error('Unexpected response for close');
      }
    } catch {
      // Ignore close errors and always terminate locally.
    }

    this.blossomBandwidthListeners.clear();
    this.treeRootListeners.clear();
    this.webrtcProxy?.close();
    this.webrtcProxy = null;
    this.worker?.terminate();
    this.worker = null;
    this.initPromise = null;
    this.initPending = null;
    this.rejectAllPending(new Error('Worker closed'));
  }
}
