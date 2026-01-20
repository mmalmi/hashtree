/**
 * Worker Adapter
 *
 * Main thread adapter for communicating with the hashtree worker.
 * Provides a Promise-based API wrapping postMessage communication.
 * Handles worker crash recovery with exponential backoff.
 */

import type {
  WorkerRequest,
  WorkerResponse,
  WorkerConfig,
  WorkerNostrFilter as NostrFilter,
  WorkerSignedEvent as SignedEvent,
  WorkerUnsignedEvent as UnsignedEvent,
  WorkerPeerStats as PeerStats,
  WorkerRelayStats as RelayStats,
  WorkerDirEntry as DirEntry,
  WorkerSocialGraphEvent as SocialGraphEvent,
  WorkerBlossomUploadProgress as BlossomUploadProgress,
  WorkerBlossomServerConfig as BlossomServerConfig,
  CID,
  WebRTCCommand,
} from 'hashtree';
import { generateRequestId } from 'hashtree';
import { WebRTCProxy } from './webrtcProxy';
import { getErrorMessage } from './utils/errorMessage';
import type { TreeRootInfo } from './worker/protocol';

type PendingRequest = {
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
  timeoutId?: ReturnType<typeof setTimeout>;
};

type SubscriptionCallback = (event: SignedEvent) => void;
type EoseCallback = () => void;

// Worker constructor type - can be a URL object, URL string, or a Worker constructor from Vite
type WorkerConstructor = URL | string | (new () => Worker);

export class WorkerAdapter {
  private worker: Worker | null = null;
  private workerFactory: WorkerConstructor;
  private config: WorkerConfig;
  private ready = false;
  private readyPromise: Promise<void> | null = null;
  private readyResolve: (() => void) | null = null;
  private restartAttempts = 0;
  private maxRestartAttempts = 10;

  // Heartbeat monitoring - detects unresponsive workers (infinite loops, deadlocks)
  private heartbeatInterval: ReturnType<typeof setInterval> | null = null;
  private lastPong: number = Date.now();
  private readonly HEARTBEAT_INTERVAL_MS = 5000;  // Send ping every 5s
  private readonly HEARTBEAT_TIMEOUT_MS = 15000;  // 3 missed beats = unresponsive

  // Pending requests waiting for responses
  private pendingRequests = new Map<string, PendingRequest>();

  // Nostr subscription callbacks
  private subscriptions = new Map<string, { callback?: SubscriptionCallback; eose?: EoseCallback }>();
  private globalEventCallback: ((event: SignedEvent) => void) | null = null;

  // Stream callbacks (for readFileStream)
  private streamCallbacks = new Map<string, (chunk: Uint8Array, done: boolean) => void>();

  // SocialGraph version callback
  private socialGraphVersionCallback: ((version: number) => void) | null = null;

  // Blossom upload progress callback
  private blossomProgressCallback: ((progress: BlossomUploadProgress) => void) | null = null;

  // Background Blossom push progress callback (for automatic pushes)
  private blossomPushProgressCallback: ((treeName: string, current: number, total: number) => void) | null = null;
  private blossomPushCompleteCallback: ((treeName: string, pushed: number, skipped: number, failed: number) => void) | null = null;

  // Tree root update callbacks (worker → main thread notifications)
  private treeRootUpdateCallbacks = new Set<(npub: string, treeName: string, hash: Uint8Array, updatedAt: number, options: { key?: Uint8Array; visibility: string; encryptedKey?: string; keyId?: string; selfEncryptedKey?: string; selfEncryptedLinkKey?: string }) => void>();

  // Message queue for messages sent before worker is ready
  private messageQueue: WorkerRequest[] = [];

  // WebRTC proxy (main thread owns RTCPeerConnection, worker controls logic)
  private webrtcProxy: WebRTCProxy | null = null;

  /**
   * Create a WorkerAdapter
   * @param workerFactory - Either a URL string or a Worker constructor from Vite's `?worker` import
   * @param config - Worker configuration
   */
  constructor(workerFactory: WorkerConstructor, config: WorkerConfig) {
    this.workerFactory = workerFactory;
    this.config = config;
  }

  /**
   * Initialize the worker and wait for it to be ready
   */
  async init(): Promise<void> {
    if (this.ready) return;
    if (this.readyPromise) return this.readyPromise;

    this.readyPromise = new Promise((resolve) => {
      this.readyResolve = resolve;
    });

    this.spawnWorker();

    return this.readyPromise;
  }

  private spawnWorker() {
    if (this.workerFactory instanceof URL) {
      // URL object - recommended approach
      this.worker = new Worker(this.workerFactory, { type: 'module' });
    } else if (typeof this.workerFactory === 'string') {
      // URL string
      this.worker = new Worker(this.workerFactory, { type: 'module' });
    } else {
      // Vite worker constructor from ?worker import
      this.worker = new this.workerFactory();
    }
    this.setupMessageHandler();
    this.setupErrorHandler();

    // Send init message
    this.worker.postMessage({
      type: 'init',
      id: generateRequestId(),
      config: this.config,
    } as WorkerRequest);
  }

  private setupMessageHandler() {
    if (!this.worker) return;

    this.worker.onmessage = async (e: MessageEvent<WorkerResponse>) => {
      const msg = e.data;

      switch (msg.type) {
        case 'ready':
          this.ready = true;
          this.restartAttempts = 0;
          this.flushMessageQueue();
          this.initWebRTCProxy();
          this.startHeartbeat();
          this.readyResolve?.();
          console.log('[WorkerAdapter] Worker ready');
          break;

        case 'pong':
          this.lastPong = Date.now();
          break;

        case 'error':
          if (msg.id) {
            this.rejectPending(msg.id, new Error(msg.error));
          } else {
            console.error('[WorkerAdapter] Worker error:', msg.error);
          }
          break;

        case 'result':
        case 'bool':
        case 'cid':
        case 'void':
        case 'dirListing':
        case 'peerStats':
        case 'relayStats':
        case 'storageStats':
        case 'treeRootInfo':
          this.resolvePending(msg.id, msg);
          break;

        case 'streamChunk':
          this.handleStreamChunk(msg.id, msg.chunk, msg.done);
          break;

        case 'event':
          this.handleNostrEvent(msg.subId, msg.event);
          break;

        case 'eose':
          this.handleEose(msg.subId);
          break;

        // NIP-07 requests from worker - delegate to main thread extension
        case 'signEvent':
          await this.handleSignRequest(msg.id, msg.event);
          break;

        case 'nip44Encrypt':
          await this.handleEncryptRequest(msg.id, msg.pubkey, msg.plaintext);
          break;

        case 'nip44Decrypt':
          await this.handleDecryptRequest(msg.id, msg.pubkey, msg.ciphertext);
          break;

        // SocialGraph responses
        case 'socialGraphReady':
        case 'followDistance':
        case 'isFollowingResult':
        case 'pubkeyList':
        case 'socialGraphSize':
          this.resolvePending(msg.id, msg);
          break;

        case 'socialGraphVersion':
          this.handleSocialGraphVersion(msg.version);
          break;

        // Blossom upload progress
        case 'blossomUploadProgress':
          this.handleBlossomProgress(msg.progress);
          break;

        // Background Blossom push progress
        case 'blossomPushProgress':
          this.blossomPushProgressCallback?.(msg.treeName, msg.current, msg.total);
          break;

        case 'blossomPushComplete':
          this.blossomPushCompleteCallback?.(msg.treeName, msg.pushed, msg.skipped, msg.failed);
          break;

        // Tree root updates from worker (Nostr subscriptions)
        case 'treeRootUpdate':
          this.handleTreeRootUpdate(msg as unknown as {
            npub: string;
            treeName: string;
            hash: Uint8Array;
            key?: Uint8Array;
            visibility: string;
            updatedAt: number;
            encryptedKey?: string;
            keyId?: string;
            selfEncryptedKey?: string;
            selfEncryptedLinkKey?: string;
          });
          break;

        case 'blossomPushResult':
        case 'republishResult':
          this.resolvePending(msg.id, msg);
          break;

        // WebRTC commands from worker - execute via proxy
        case 'rtc:createPeer':
        case 'rtc:closePeer':
        case 'rtc:createOffer':
        case 'rtc:createAnswer':
        case 'rtc:setLocalDescription':
        case 'rtc:setRemoteDescription':
        case 'rtc:addIceCandidate':
        case 'rtc:sendData':
          this.webrtcProxy?.handleCommand(msg as WebRTCCommand);
          break;

        default:
          console.warn('[WorkerAdapter] Unknown message type:', (msg as { type: string }).type);
      }
    };
  }

  private setupErrorHandler() {
    if (!this.worker) return;

    this.worker.onerror = (error) => {
      // Try to get more details from the error event
      const errorEvent = error as ErrorEvent;
      console.error('[WorkerAdapter] Worker crashed:', {
        message: errorEvent.message,
        filename: errorEvent.filename,
        lineno: errorEvent.lineno,
        colno: errorEvent.colno,
        error: errorEvent.error,
      });
      this.handleWorkerCrash();
    };
  }

  private startHeartbeat() {
    this.stopHeartbeat();
    this.lastPong = Date.now();

    this.heartbeatInterval = setInterval(() => {
      if (!this.ready || !this.worker) {
        return;
      }

      // Check if worker is unresponsive
      if (Date.now() - this.lastPong > this.HEARTBEAT_TIMEOUT_MS) {
        console.error('[WorkerAdapter] Worker unresponsive (no pong received), restarting...');
        this.handleWorkerCrash();
        return;
      }

      // Send ping
      this.worker.postMessage({ type: 'ping', id: generateRequestId() });
    }, this.HEARTBEAT_INTERVAL_MS);
  }

  private stopHeartbeat() {
    if (this.heartbeatInterval) {
      clearInterval(this.heartbeatInterval);
      this.heartbeatInterval = null;
    }
  }

  private async handleWorkerCrash() {
    this.stopHeartbeat();
    this.ready = false;
    this.worker?.terminate();
    this.worker = null;

    // Close WebRTC proxy - will be recreated on restart
    if (this.webrtcProxy) {
      this.webrtcProxy.close();
      this.webrtcProxy = null;
    }

    // Reject all pending requests
    for (const pending of this.pendingRequests.values()) {
      pending.reject(new Error('Worker crashed'));
    }
    this.pendingRequests.clear();

    // Attempt restart with exponential backoff
    if (this.restartAttempts < this.maxRestartAttempts) {
      this.restartAttempts++;
      const delay = Math.min(1000 * Math.pow(2, this.restartAttempts - 1), 30000);
      console.log(`[WorkerAdapter] Restarting worker in ${delay}ms (attempt ${this.restartAttempts})`);

      await new Promise((resolve) => setTimeout(resolve, delay));

      this.readyPromise = new Promise((resolve) => {
        this.readyResolve = resolve;
      });

      this.spawnWorker();
    } else {
      console.error('[WorkerAdapter] Max restart attempts exceeded');
    }
  }

  private flushMessageQueue() {
    while (this.messageQueue.length > 0) {
      const msg = this.messageQueue.shift()!;
      this.worker?.postMessage(msg);
    }
  }

  private initWebRTCProxy() {
    if (this.webrtcProxy) {
      this.webrtcProxy.close();
    }

    // Create proxy that forwards events to worker
    this.webrtcProxy = new WebRTCProxy((event) => {
      // Forward all WebRTC events to worker
      // Use transferable for data messages to avoid memory copy
      if (event.type === 'rtc:dataChannelMessage' && event.data?.buffer) {
        this.worker?.postMessage(event, [event.data.buffer]);
      } else {
        this.worker?.postMessage(event);
      }
    });

    console.log('[WorkerAdapter] WebRTC proxy initialized');
  }

  private postMessage(msg: WorkerRequest, transfer?: Transferable[]) {
    if (this.ready && this.worker) {
      if (transfer) {
        this.worker.postMessage(msg, transfer);
      } else {
        this.worker.postMessage(msg);
      }
    } else {
      this.messageQueue.push(msg);
    }
  }

  private resolvePending(id: string, value: unknown) {
    const pending = this.pendingRequests.get(id);
    if (pending) {
      this.pendingRequests.delete(id);
      if (pending.timeoutId) clearTimeout(pending.timeoutId);
      pending.resolve(value);
    }
  }

  private rejectPending(id: string, error: Error) {
    const pending = this.pendingRequests.get(id);
    if (pending) {
      this.pendingRequests.delete(id);
      if (pending.timeoutId) clearTimeout(pending.timeoutId);
      pending.reject(error);
    }
  }

  private request<T>(msg: WorkerRequest, transfer?: Transferable[]): Promise<T> {
    return new Promise((resolve, reject) => {
      const id = (msg as { id: string }).id;

      // Timeout after 120 seconds (increased for large tree operations)
      const timeoutId = setTimeout(() => {
        if (this.pendingRequests.has(id)) {
          this.pendingRequests.delete(id);
          reject(new Error('Request timeout'));
        }
      }, 120000);

      this.pendingRequests.set(id, {
        resolve: resolve as (value: unknown) => void,
        reject,
        timeoutId,
      });
      this.postMessage(msg, transfer);
    });
  }

  // ============================================================================
  // Stream Handling
  // ============================================================================

  private handleStreamChunk(id: string, chunk: Uint8Array, done: boolean) {
    const callback = this.streamCallbacks.get(id);
    if (callback) {
      callback(chunk, done);
      if (done) {
        this.streamCallbacks.delete(id);
      }
    }
  }

  // ============================================================================
  // Nostr Event Handling
  // ============================================================================

  private handleNostrEvent(subId: string, event: SignedEvent) {
    // Call global event callback (for subManager.dispatchEvent pattern)
    if (this.globalEventCallback) {
      this.globalEventCallback(event);
    }
    // Also call per-subscription callback if set
    const sub = this.subscriptions.get(subId);
    if (sub?.callback) {
      sub.callback(event);
    }
  }

  private handleEose(subId: string) {
    const sub = this.subscriptions.get(subId);
    if (sub?.eose) {
      sub.eose();
    }
  }

  // ============================================================================
  // NIP-07 Handlers (delegate to window.nostr)
  // ============================================================================

  private async handleSignRequest(id: string, event: UnsignedEvent) {
    try {
      const nostr = (window as unknown as { nostr?: { signEvent: (e: UnsignedEvent) => Promise<SignedEvent> } }).nostr;
      if (!nostr?.signEvent) {
        throw new Error('NIP-07 extension not available');
      }

      const signed = await nostr.signEvent(event);
      this.worker?.postMessage({ type: 'signed', id, event: signed } as WorkerRequest);
    } catch (err) {
      const error = getErrorMessage(err);
      this.worker?.postMessage({ type: 'signed', id, error } as WorkerRequest);
    }
  }

  private async handleEncryptRequest(id: string, pubkey: string, plaintext: string) {
    try {
      const nostr = (window as unknown as { nostr?: { nip44?: { encrypt: (pk: string, pt: string) => Promise<string> } } }).nostr;
      if (!nostr?.nip44?.encrypt) {
        throw new Error('NIP-44 encryption not available');
      }

      const ciphertext = await nostr.nip44.encrypt(pubkey, plaintext);
      this.worker?.postMessage({ type: 'encrypted', id, ciphertext } as WorkerRequest);
    } catch (err) {
      const error = getErrorMessage(err);
      this.worker?.postMessage({ type: 'encrypted', id, error } as WorkerRequest);
    }
  }

  private async handleDecryptRequest(id: string, pubkey: string, ciphertext: string) {
    try {
      const nostr = (window as unknown as { nostr?: { nip44?: { decrypt: (pk: string, ct: string) => Promise<string> } } }).nostr;
      if (!nostr?.nip44?.decrypt) {
        throw new Error('NIP-44 decryption not available');
      }

      const plaintext = await nostr.nip44.decrypt(pubkey, ciphertext);
      this.worker?.postMessage({ type: 'decrypted', id, plaintext } as WorkerRequest);
    } catch (err) {
      const error = getErrorMessage(err);
      this.worker?.postMessage({ type: 'decrypted', id, error } as WorkerRequest);
    }
  }

  // ============================================================================
  // SocialGraph Version Handler
  // ============================================================================

  private handleSocialGraphVersion(version: number) {
    this.socialGraphVersionCallback?.(version);
  }

  // ============================================================================
  // Blossom Progress Handler
  // ============================================================================

  private handleBlossomProgress(progress: BlossomUploadProgress) {
    this.blossomProgressCallback?.(progress);
  }

  // ============================================================================
  // Tree Root Update Handler
  // ============================================================================

  private handleTreeRootUpdate(msg: {
    npub: string;
    treeName: string;
    hash: Uint8Array;
    key?: Uint8Array;
    visibility: string;
    updatedAt: number;
    encryptedKey?: string;
    keyId?: string;
    selfEncryptedKey?: string;
    selfEncryptedLinkKey?: string;
  }) {
    for (const callback of this.treeRootUpdateCallbacks) {
      callback(msg.npub, msg.treeName, msg.hash, msg.updatedAt, {
        key: msg.key,
        visibility: msg.visibility,
        encryptedKey: msg.encryptedKey,
        keyId: msg.keyId,
        selfEncryptedKey: msg.selfEncryptedKey,
        selfEncryptedLinkKey: msg.selfEncryptedLinkKey,
      });
    }
  }

  // ============================================================================
  // Public API - Blossom Upload Sessions
  // ============================================================================

  /**
   * Set callback for blossom upload progress updates
   */
  onBlossomProgress(callback: (progress: BlossomUploadProgress) => void): void {
    this.blossomProgressCallback = callback;
  }

  /**
   * Set callback for background Blossom push progress (automatic pushes)
   */
  onBlossomPushProgress(callback: (treeName: string, current: number, total: number) => void): void {
    this.blossomPushProgressCallback = callback;
  }

  /**
   * Set callback for background Blossom push completion
   */
  onBlossomPushComplete(callback: (treeName: string, pushed: number, skipped: number, failed: number) => void): void {
    this.blossomPushCompleteCallback = callback;
  }

  /**
   * Subscribe to tree root updates from worker (Nostr subscription notifications).
   * Returns an unsubscribe function.
   */
  onTreeRootUpdate(
    callback: (npub: string, treeName: string, hash: Uint8Array, updatedAt: number, options: { key?: Uint8Array; visibility: string; encryptedKey?: string; keyId?: string; selfEncryptedKey?: string; selfEncryptedLinkKey?: string }) => void
  ): () => void {
    this.treeRootUpdateCallbacks.add(callback);
    return () => this.treeRootUpdateCallbacks.delete(callback);
  }

  /**
   * Start a blossom upload session for progress tracking
   * @param sessionId - Unique session identifier
   * @param totalChunks - Total number of chunks to upload
   */
  async startBlossomSession(sessionId: string, totalChunks: number): Promise<void> {
    const id = generateRequestId();
    await this.request<{ error?: string }>({
      type: 'startBlossomSession',
      id,
      sessionId,
      totalChunks,
    } as WorkerRequest);
  }

  /**
   * End the current blossom upload session
   */
  async endBlossomSession(): Promise<void> {
    const id = generateRequestId();
    await this.request<{ error?: string }>({
      type: 'endBlossomSession',
      id,
    } as WorkerRequest);
  }

  /**
   * Push a tree to blossom servers
   * @param cidHash - Hash of the root CID
   * @param cidKey - Optional encryption key
   * @param treeName - Optional tree name for progress tracking
   * @returns Push result with pushed/skipped/failed counts and error messages
   */
  async pushToBlossom(cidHash: Uint8Array, cidKey?: Uint8Array, treeName?: string): Promise<{ pushed: number; skipped: number; failed: number; errors?: string[] }> {
    const id = generateRequestId();
    const response = await this.request<{ pushed: number; skipped: number; failed: number; error?: string; errors?: string[] }>({
      type: 'pushToBlossom',
      id,
      cidHash,
      cidKey,
      treeName,
    } as WorkerRequest);
    if (response.error) throw new Error(response.error);
    return { pushed: response.pushed, skipped: response.skipped, failed: response.failed, errors: response.errors };
  }

  /**
   * Republish all cached tree events to relays
   * @param prefix - Optional URL-encoded prefix to filter trees by d-tag
   * @returns Result with count and any trees with encryption errors
   */
  async republishTrees(prefix?: string): Promise<{ count: number; encryptionErrors?: string[] }> {
    const id = generateRequestId();
    const response = await this.request<{ count: number; error?: string; encryptionErrors?: string[] }>({
      type: 'republishTrees',
      id,
      prefix,
    } as WorkerRequest);
    if (response.error) throw new Error(response.error);
    return { count: response.count, encryptionErrors: response.encryptionErrors };
  }

  /**
   * Republish a single tree's event to relays (preserves original event)
   * Works for any user's tree, not just own.
   * @param pubkey - Pubkey of the tree owner
   * @param treeName - Name of the tree (d-tag)
   * @returns true if republished successfully
   */
  async republishTree(pubkey: string, treeName: string): Promise<boolean> {
    const id = generateRequestId();
    const response = await this.request<{ value: boolean; error?: string }>({
      type: 'republishTree',
      id,
      pubkey,
      treeName,
    } as WorkerRequest);
    if (response.error) throw new Error(response.error);
    return response.value;
  }

  // ============================================================================
  // Public API - Store Operations
  // ============================================================================

  async get(hash: Uint8Array): Promise<Uint8Array | null> {
    const id = generateRequestId();
    const response = await this.request<{ data?: Uint8Array; error?: string }>({
      type: 'get',
      id,
      hash,
    });
    if (response.error) throw new Error(response.error);
    return response.data || null;
  }

  async put(hash: Uint8Array, data: Uint8Array): Promise<boolean> {
    const id = generateRequestId();
    const response = await this.request<{ value: boolean; error?: string }>(
      { type: 'put', id, hash, data },
      [data.buffer]  // Transfer ownership
    );
    if (response.error) throw new Error(response.error);
    return response.value;
  }

  async has(hash: Uint8Array): Promise<boolean> {
    const id = generateRequestId();
    const response = await this.request<{ value: boolean; error?: string }>({
      type: 'has',
      id,
      hash,
    });
    if (response.error) throw new Error(response.error);
    return response.value;
  }

  async delete(hash: Uint8Array): Promise<boolean> {
    const id = generateRequestId();
    const response = await this.request<{ value: boolean; error?: string }>({
      type: 'delete',
      id,
      hash,
    });
    if (response.error) throw new Error(response.error);
    return response.value;
  }

  // ============================================================================
  // Public API - Tree Operations
  // ============================================================================

  async readFile(cid: CID): Promise<Uint8Array | null> {
    const id = generateRequestId();
    const response = await this.request<{ data?: Uint8Array; error?: string }>({
      type: 'readFile',
      id,
      cid,
    });
    if (response.error) throw new Error(response.error);
    return response.data || null;
  }

  async readFileRange(cid: CID, start: number, end?: number): Promise<Uint8Array | null> {
    const id = generateRequestId();
    const response = await this.request<{ data?: Uint8Array; error?: string }>({
      type: 'readFileRange',
      id,
      cid,
      start,
      end,
    });
    if (response.error) throw new Error(response.error);
    return response.data || null;
  }

  async *readFileStream(cid: CID): AsyncGenerator<Uint8Array> {
    const id = generateRequestId();
    const chunks: Uint8Array[] = [];
    let done = false;
    let resolveNext: (() => void) | null = null;

    this.streamCallbacks.set(id, (chunk, isDone) => {
      if (chunk.length > 0) {
        chunks.push(chunk);
      }
      done = isDone;
      resolveNext?.();
    });

    this.postMessage({ type: 'readFileStream', id, cid });

    while (!done) {
      if (chunks.length > 0) {
        yield chunks.shift()!;
      } else {
        await new Promise<void>((resolve) => {
          resolveNext = resolve;
        });
      }
    }

    // Yield any remaining chunks
    while (chunks.length > 0) {
      yield chunks.shift()!;
    }
  }

  async writeFile(parentCid: CID | null, path: string, data: Uint8Array): Promise<CID> {
    const id = generateRequestId();
    const response = await this.request<{ cid?: CID; error?: string }>(
      { type: 'writeFile', id, parentCid, path, data },
      [data.buffer]
    );
    if (response.error) throw new Error(response.error);
    if (!response.cid) throw new Error('No CID returned');
    return response.cid;
  }

  async deleteFile(parentCid: CID, path: string): Promise<CID> {
    const id = generateRequestId();
    const response = await this.request<{ cid?: CID; error?: string }>({
      type: 'deleteFile',
      id,
      parentCid,
      path,
    });
    if (response.error) throw new Error(response.error);
    if (!response.cid) throw new Error('No CID returned');
    return response.cid;
  }

  async listDir(cid: CID): Promise<DirEntry[]> {
    const id = generateRequestId();
    const response = await this.request<{ entries?: DirEntry[]; error?: string }>({
      type: 'listDir',
      id,
      cid,
    });
    if (response.error) throw new Error(response.error);
    return response.entries || [];
  }

  async resolveRoot(npub: string, path?: string): Promise<CID | null> {
    const id = generateRequestId();
    const response = await this.request<{ cid?: CID; error?: string }>({
      type: 'resolveRoot',
      id,
      npub,
      path,
    });
    if (response.error) throw new Error(response.error);
    return response.cid || null;
  }

  // ============================================================================
  // Public API - Nostr
  // ============================================================================

  /**
   * Set global event callback - called for ALL events from ALL subscriptions.
   * Used with ndk.subManager.dispatchEvent pattern.
   */
  onEvent(callback: (event: SignedEvent) => void): void {
    this.globalEventCallback = callback;
  }

  /**
   * Subscribe to events matching filters.
   * If using global onEvent callback, no per-subscription callback needed.
   */
  subscribe(
    filters: NostrFilter[],
    callback?: SubscriptionCallback,
    eose?: EoseCallback
  ): string {
    const subId = generateRequestId();
    this.subscriptions.set(subId, { callback, eose });
    this.postMessage({ type: 'subscribe', id: subId, filters });
    return subId;
  }

  unsubscribe(subId: string): void {
    this.subscriptions.delete(subId);
    this.postMessage({ type: 'unsubscribe', id: generateRequestId(), subId });
  }

  async publish(event: SignedEvent): Promise<void> {
    const id = generateRequestId();
    const response = await this.request<{ error?: string }>({
      type: 'publish',
      id,
      event,
    });
    if (response.error) throw new Error(response.error);
  }

  // ============================================================================
  // Public API - Stats
  // ============================================================================

  async getPeerStats(): Promise<PeerStats[]> {
    const id = generateRequestId();
    const response = await this.request<{ stats: PeerStats[] }>({
      type: 'getPeerStats',
      id,
    });
    return response.stats;
  }

  async getRelayStats(): Promise<RelayStats[]> {
    const id = generateRequestId();
    const response = await this.request<{ stats: RelayStats[] }>({
      type: 'getRelayStats',
      id,
    });
    return response.stats;
  }

  async getStorageStats(): Promise<{ items: number; bytes: number }> {
    const id = generateRequestId();
    const response = await this.request<{ items: number; bytes: number }>({
      type: 'getStorageStats',
      id,
    });
    return { items: response.items, bytes: response.bytes };
  }

  /**
   * Block a peer by pubkey (disconnect and prevent reconnection)
   */
  async blockPeer(pubkey: string): Promise<void> {
    const id = generateRequestId();
    await this.request<{ error?: string }>({
      type: 'blockPeer',
      id,
      pubkey,
    } as WorkerRequest);
  }

  /**
   * Set WebRTC pool configuration
   */
  async setWebRTCPools(pools: { follows: { max: number; satisfied: number }; other: { max: number; satisfied: number } }): Promise<void> {
    const id = generateRequestId();
    await this.request<{ error?: string }>({
      type: 'setWebRTCPools',
      id,
      pools,
    } as WorkerRequest);
  }

  /**
   * Trigger a WebRTC hello broadcast for peer discovery.
   * Used after follow relationships change to force peer discovery.
   */
  async sendHello(): Promise<void> {
    const id = generateRequestId();
    await this.request<{ error?: string }>({
      type: 'sendWebRTCHello',
      id,
    } as WorkerRequest);
  }

  /**
   * Update the follows list in the worker.
   * Used for WebRTC peer classification (follows pool vs others pool).
   */
  async setFollows(follows: string[]): Promise<void> {
    const id = generateRequestId();
    await this.request<{ error?: string }>({
      type: 'setFollows',
      id,
      follows,
    } as WorkerRequest);
  }

  /**
   * Update blossom server configuration in the worker.
   * Call this when blossom settings change to sync with the worker.
   */
  async setBlossomServers(servers: BlossomServerConfig[]): Promise<void> {
    const id = generateRequestId();
    await this.request<{ error?: string }>({
      type: 'setBlossomServers',
      id,
      servers,
    } as WorkerRequest);
  }

  /**
   * Set storage limit for IndexedDB eviction.
   * When storage exceeds this limit, oldest entries will be evicted.
   */
  async setStorageMaxBytes(maxBytes: number): Promise<void> {
    const id = generateRequestId();
    await this.request<{ error?: string }>({
      type: 'setStorageMaxBytes',
      id,
      maxBytes,
    } as WorkerRequest);
  }

  /**
   * Update relay URLs dynamically.
   * Disconnects old relays and connects to new ones.
   */
  async setRelays(relays: string[]): Promise<void> {
    const id = generateRequestId();
    await this.request<{ error?: string }>({
      type: 'setRelays',
      id,
      relays,
    } as WorkerRequest);
  }

  /**
   * Update tree root cache in the worker.
   * Called when TreeRootRegistry updates to keep worker cache in sync.
   */
  async setTreeRootCache(
    npub: string,
    treeName: string,
    hash: Uint8Array,
    key?: Uint8Array,
    visibility: 'public' | 'link-visible' | 'private' = 'public'
  ): Promise<void> {
    const id = generateRequestId();
    await this.request<{ error?: string }>({
      type: 'setTreeRootCache',
      id,
      npub,
      treeName,
      hash,
      key,
      visibility,
    } as WorkerRequest);
  }

  async getTreeRootInfo(npub: string, treeName: string): Promise<TreeRootInfo | null> {
    const id = generateRequestId();
    const response = await this.request<{ record?: TreeRootInfo; error?: string }>({
      type: 'getTreeRootInfo',
      id,
      npub,
      treeName,
    } as WorkerRequest);
    if (response.error) throw new Error(response.error);
    return response.record ?? null;
  }

  async mergeTreeRootKey(npub: string, treeName: string, hash: Uint8Array, key: Uint8Array): Promise<boolean> {
    const id = generateRequestId();
    const response = await this.request<{ value?: boolean; error?: string }>({
      type: 'mergeTreeRootKey',
      id,
      npub,
      treeName,
      hash,
      key,
    } as WorkerRequest);
    if (response.error) throw new Error(response.error);
    return response.value ?? false;
  }

  async subscribeTreeRoots(pubkey: string): Promise<void> {
    const id = generateRequestId();
    await this.request<{ error?: string }>({
      type: 'subscribeTreeRoots',
      id,
      pubkey,
    } as WorkerRequest);
  }

  async unsubscribeTreeRoots(pubkey: string): Promise<void> {
    const id = generateRequestId();
    await this.request<{ error?: string }>({
      type: 'unsubscribeTreeRoots',
      id,
      pubkey,
    } as WorkerRequest);
  }

  // ============================================================================
  // Public API - Media Streaming
  // ============================================================================

  /**
   * Register a MessagePort from the service worker for media streaming
   */
  registerMediaPort(port: MessagePort): void {
    if (!this.worker) {
      console.warn('[WorkerAdapter] Cannot register media port - worker not ready');
      return;
    }
    this.worker.postMessage({ type: 'registerMediaPort', port } as WorkerRequest, [port]);
  }

  // ============================================================================
  // Public API - SocialGraph
  // ============================================================================

  /**
   * Set callback for social graph version updates
   */
  onSocialGraphVersion(callback: (version: number) => void): void {
    this.socialGraphVersionCallback = callback;
  }

  /**
   * Initialize the social graph with optional root pubkey
   */
  async initSocialGraph(rootPubkey?: string): Promise<{ version: number; size: number }> {
    const id = generateRequestId();
    const response = await this.request<{ version: number; size: number; error?: string }>({
      type: 'initSocialGraph',
      id,
      rootPubkey,
    } as WorkerRequest);
    if (response.error) throw new Error(response.error);
    return { version: response.version, size: response.size };
  }

  /**
   * Set the social graph root pubkey
   */
  async setSocialGraphRoot(pubkey: string): Promise<void> {
    const id = generateRequestId();
    const response = await this.request<{ error?: string }>({
      type: 'setSocialGraphRoot',
      id,
      pubkey,
    } as WorkerRequest);
    if (response.error) throw new Error(response.error);
  }

  /**
   * Handle social graph events (kind:3 contact lists)
   */
  handleSocialGraphEvents(events: SocialGraphEvent[]): void {
    if (events.length === 0) return;
    this.postMessage({
      type: 'handleSocialGraphEvents',
      id: generateRequestId(),
      events,
    } as WorkerRequest);
  }

  /**
   * Get follow distance for a pubkey
   */
  async getFollowDistance(pubkey: string): Promise<number> {
    const id = generateRequestId();
    const response = await this.request<{ distance: number; error?: string }>({
      type: 'getFollowDistance',
      id,
      pubkey,
    } as WorkerRequest);
    if (response.error) throw new Error(response.error);
    return response.distance;
  }

  /**
   * Check if follower follows followed
   */
  async isFollowing(follower: string, followed: string): Promise<boolean> {
    const id = generateRequestId();
    const response = await this.request<{ result: boolean; error?: string }>({
      type: 'isFollowing',
      id,
      follower,
      followed,
    } as WorkerRequest);
    if (response.error) throw new Error(response.error);
    return response.result;
  }

  /**
   * Get list of pubkeys a user follows
   */
  async getFollows(pubkey: string): Promise<string[]> {
    const id = generateRequestId();
    const response = await this.request<{ pubkeys: string[]; error?: string }>({
      type: 'getFollows',
      id,
      pubkey,
    } as WorkerRequest);
    if (response.error) throw new Error(response.error);
    return response.pubkeys;
  }

  /**
   * Get list of pubkeys following a user
   */
  async getFollowers(pubkey: string): Promise<string[]> {
    const id = generateRequestId();
    const response = await this.request<{ pubkeys: string[]; error?: string }>({
      type: 'getFollowers',
      id,
      pubkey,
    } as WorkerRequest);
    if (response.error) throw new Error(response.error);
    return response.pubkeys;
  }

  /**
   * Get pubkeys followed by friends of a user
   */
  async getFollowedByFriends(pubkey: string): Promise<string[]> {
    const id = generateRequestId();
    const response = await this.request<{ pubkeys: string[]; error?: string }>({
      type: 'getFollowedByFriends',
      id,
      pubkey,
    } as WorkerRequest);
    if (response.error) throw new Error(response.error);
    return response.pubkeys;
  }

  /**
   * Fetch a user's follow list when visiting their profile.
   * Only fetches if we don't already have their follow list.
   */
  fetchUserFollows(pubkey: string): void {
    const id = generateRequestId();
    // Fire and forget - don't await response
    this.request<{ error?: string }>({
      type: 'fetchUserFollows',
      id,
      pubkey,
    } as WorkerRequest).catch(() => {});
  }

  /**
   * Fetch followers of a user (who follows them) - for profile views
   */
  fetchUserFollowers(pubkey: string): void {
    const id = generateRequestId();
    // Fire and forget - don't await response
    this.request<{ error?: string }>({
      type: 'fetchUserFollowers',
      id,
      pubkey,
    } as WorkerRequest).catch(() => {});
  }

  /**
   * Get size of the social graph
   */
  async getSocialGraphSize(): Promise<number> {
    const id = generateRequestId();
    const response = await this.request<{ size: number; error?: string }>({
      type: 'getSocialGraphSize',
      id,
    } as WorkerRequest);
    if (response.error) throw new Error(response.error);
    return response.size;
  }

  /**
   * Get users by follow distance
   */
  async getUsersByDistance(distance: number): Promise<string[]> {
    const id = generateRequestId();
    const response = await this.request<{ pubkeys: string[]; error?: string }>({
      type: 'getUsersByDistance',
      id,
      distance,
    } as WorkerRequest);
    if (response.error) throw new Error(response.error);
    return response.pubkeys;
  }

  // ============================================================================
  // Identity Management
  // ============================================================================

  /**
   * Update worker's user identity (for account switching)
   */
  async setIdentity(pubkey: string, nsec?: string): Promise<void> {
    const id = generateRequestId();
    await this.request<{ error?: string }>({
      type: 'setIdentity',
      id,
      pubkey,
      nsec,
    } as WorkerRequest);
  }

  // ============================================================================
  // Cleanup
  // ============================================================================

  close(): void {
    this.stopHeartbeat();

    // Close WebRTC proxy
    if (this.webrtcProxy) {
      this.webrtcProxy.close();
      this.webrtcProxy = null;
    }

    if (this.worker) {
      this.postMessage({ type: 'close', id: generateRequestId() });
      this.worker.terminate();
      this.worker = null;
    }
    this.ready = false;
    this.pendingRequests.clear();
    this.subscriptions.clear();
    this.streamCallbacks.clear();
    this.messageQueue = [];
    this.socialGraphVersionCallback = null;
    this.treeRootUpdateCallbacks.clear();
  }
}

// ============================================================================
// Singleton Instance
// ============================================================================

let instance: WorkerAdapter | null = null;

// Expose on window for tests to reliably access (avoids Vite module duplication issues)
declare global {
  interface Window {
    __workerAdapter?: WorkerAdapter | null;
  }
}

export function getWorkerAdapter(): WorkerAdapter | null {
  return instance;
}

export async function initWorkerAdapter(
  workerFactory: WorkerConstructor,
  config: WorkerConfig
): Promise<WorkerAdapter> {
  if (instance) {
    return instance;
  }

  instance = new WorkerAdapter(workerFactory, config);
  await instance.init();

  // Expose on window for tests
  if (typeof window !== 'undefined') {
    window.__workerAdapter = instance;
  }

  return instance;
}

export function closeWorkerAdapter(): void {
  if (instance) {
    instance.close();
    instance = null;
  }
}

// Re-export types for consumers
export type { PeerStats, RelayStats };
