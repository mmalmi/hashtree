import { generateRequestId, type CID } from '@hashtree/core';
import type {
  BlossomBandwidthState,
  BlossomServerConfig,
  BlobSource,
  ConnectivityState,
  RootResolveOptions,
  WorkerDiagnosticEvent,
  UploadProgressState,
  WorkerConfig,
  WorkerRequest,
  WorkerResponse,
} from './protocol.js';

type PendingRequest = {
  resolve: (value: WorkerResponse) => void;
  reject: (error: Error) => void;
  timeoutId: ReturnType<typeof setTimeout>;
};

export type WorkerFactory = URL | string | (new () => Worker);
export type P2PFetchHandler = (hashHex: string, peerId?: string) => Promise<Uint8Array | null>;
export type P2PPeerListHandler = () => string[] | Promise<string[]>;

const REQUEST_TIMEOUT_MS = 30_000;
const PUT_BLOB_TIMEOUT_MS = 15 * 60_000;
const STREAM_APPEND_TIMEOUT_MS = 60_000;
type WorkerRequestPayload = WorkerRequest extends infer T
  ? T extends { id: string }
    ? Omit<T, 'id'>
    : never
  : never;

export class HashtreeWorkerClient {
  private readonly workerFactory: WorkerFactory;
  private readonly config: WorkerConfig;
  private worker: Worker | null = null;
  private initPromise: Promise<void> | null = null;
  private pending = new Map<string, PendingRequest>();
  private connectivityListeners = new Set<(state: ConnectivityState) => void>();
  private uploadProgressListeners = new Set<(progress: UploadProgressState) => void>();
  private blossomBandwidthListeners = new Set<(stats: BlossomBandwidthState) => void>();
  private diagnosticListeners = new Set<(event: WorkerDiagnosticEvent) => void>();
  private rootWatchListeners = new Map<string, (cid: CID | null) => void>();
  private pendingRootWatchUpdates = new Map<string, CID | null>();
  private p2pFetchHandler: P2PFetchHandler | null = null;
  private p2pPeerListHandler: P2PPeerListHandler | null = null;

  constructor(workerFactory: WorkerFactory, config: WorkerConfig = {}) {
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

      const id = generateRequestId();
      const timeoutId = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error('Worker init timed out'));
      }, REQUEST_TIMEOUT_MS);

      this.pending.set(id, {
        resolve: (message) => {
          if (message.type === 'ready') {
            resolve();
            return;
          }
          reject(new Error('Unexpected init response'));
        },
        reject: (error) => reject(error),
        timeoutId,
      });

      this.worker.postMessage({
        type: 'init',
        id,
        config: this.config,
      } as WorkerRequest);
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

    this.worker.onmessage = (event: MessageEvent<WorkerResponse>) => {
      const message = event.data;

      if (message.type === 'connectivityUpdate') {
        this.connectivityListeners.forEach(listener => listener(message.state));
        return;
      }

      if (message.type === 'uploadProgress') {
        this.uploadProgressListeners.forEach(listener => listener(message.progress));
        return;
      }

      if (message.type === 'blossomBandwidth') {
        this.blossomBandwidthListeners.forEach(listener => listener(message.stats));
        return;
      }

      if (message.type === 'diagnostic') {
        this.diagnosticListeners.forEach(listener => listener(message.event));
        return;
      }

      if (message.type === 'rootUpdate') {
        const listener = this.rootWatchListeners.get(message.watchId);
        if (listener) {
          listener(message.cid ?? null);
        } else {
          this.pendingRootWatchUpdates.set(message.watchId, message.cid ?? null);
        }
        return;
      }

      if (message.type === 'p2pFetch') {
        void this.handleP2PFetch(message.requestId, message.hashHex, message.peerId);
        return;
      }

      if (message.type === 'p2pPeerList') {
        void this.handleP2PPeerList(message.requestId);
        return;
      }

      if (message.type === 'error' && message.id) {
        this.rejectPending(message.id, new Error(message.error));
        return;
      }

      if ('id' in message && typeof message.id === 'string') {
        this.resolvePending(message.id, message);
      }
    };

    this.worker.onerror = (event) => {
      const errorMessage = event instanceof ErrorEvent ? event.message : 'Worker error';
      this.rejectAllPending(new Error(errorMessage));
    };
  }

  private rejectAllPending(error: Error): void {
    for (const [id, pending] of this.pending.entries()) {
      clearTimeout(pending.timeoutId);
      pending.reject(error);
      this.pending.delete(id);
    }
  }

  private resolvePending(id: string, message: WorkerResponse): void {
    const pending = this.pending.get(id);
    if (!pending) return;
    clearTimeout(pending.timeoutId);
    pending.resolve(message);
    this.pending.delete(id);
  }

  private rejectPending(id: string, error: Error): void {
    const pending = this.pending.get(id);
    if (!pending) return;
    clearTimeout(pending.timeoutId);
    pending.reject(error);
    this.pending.delete(id);
  }

  private async handleP2PFetch(requestId: string, hashHex: string, peerId?: string): Promise<void> {
    if (!this.worker) return;
    const id = generateRequestId();

    if (!this.p2pFetchHandler) {
      this.worker.postMessage({
        type: 'p2pFetchResult',
        id,
        requestId,
      } as WorkerRequest);
      return;
    }

    try {
      const data = await this.p2pFetchHandler(hashHex, peerId);
      if (data && data.byteLength > 0) {
        const transferableData = data.slice();
        this.worker.postMessage(
          {
            type: 'p2pFetchResult',
            id,
            requestId,
            data: transferableData,
          } as WorkerRequest,
          [transferableData.buffer]
        );
        return;
      }

      this.worker.postMessage({
        type: 'p2pFetchResult',
        id,
        requestId,
      } as WorkerRequest);
    } catch (err) {
      const error = err instanceof Error ? err.message : String(err);
      this.worker.postMessage({
        type: 'p2pFetchResult',
        id,
        requestId,
        error,
      } as WorkerRequest);
    }
  }

  private async handleP2PPeerList(requestId: string): Promise<void> {
    if (!this.worker) return;
    const id = generateRequestId();

    if (!this.p2pPeerListHandler) {
      this.worker.postMessage({
        type: 'p2pPeerListResult',
        id,
        requestId,
        peerIds: [],
      } as WorkerRequest);
      return;
    }

    try {
      const peerIds = await this.p2pPeerListHandler();
      this.worker.postMessage({
        type: 'p2pPeerListResult',
        id,
        requestId,
        peerIds: Array.isArray(peerIds) ? peerIds : [],
      } as WorkerRequest);
    } catch (err) {
      const error = err instanceof Error ? err.message : String(err);
      this.worker.postMessage({
        type: 'p2pPeerListResult',
        id,
        requestId,
        error,
      } as WorkerRequest);
    }
  }

  private async request(
    payload: WorkerRequestPayload,
    timeoutMs = REQUEST_TIMEOUT_MS,
    transfer: Transferable[] = []
  ): Promise<WorkerResponse> {
    await this.initIfNeeded();
    if (!this.worker) {
      throw new Error('Worker not initialized');
    }

    const id = generateRequestId();
    const message = { ...payload, id } as WorkerRequest;

    return new Promise<WorkerResponse>((resolve, reject) => {
      const timeoutId = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`Worker request timed out: ${payload.type}`));
      }, timeoutMs);

      this.pending.set(id, { resolve, reject, timeoutId });
      this.worker?.postMessage(message, transfer);
    });
  }

  private async initIfNeeded(): Promise<void> {
    if (!this.initPromise) {
      await this.init();
      return;
    }
    await this.initPromise;
  }

  async putBlob(data: Uint8Array, mimeType?: string, upload = true): Promise<{ hashHex: string; nhash: string }> {
    const res = await this.request({ type: 'putBlob', data, mimeType, upload }, PUT_BLOB_TIMEOUT_MS);
    if (res.type !== 'blobStored') {
      throw new Error('Unexpected response for putBlob');
    }
    if (!res.hashHex || !res.nhash) {
      throw new Error('Failed to store blob');
    }
    return { hashHex: res.hashHex, nhash: res.nhash };
  }

  async beginPutBlobStream(mimeType?: string, upload = true): Promise<string> {
    const res = await this.request({ type: 'beginPutBlobStream', mimeType, upload });
    if (res.type !== 'blobStreamStarted') {
      throw new Error('Unexpected response for beginPutBlobStream');
    }
    if (!res.streamId) {
      throw new Error('Failed to start blob stream');
    }
    return res.streamId;
  }

  async appendPutBlobStream(streamId: string, chunk: Uint8Array): Promise<void> {
    const res = await this.request(
      { type: 'appendPutBlobStream', streamId, chunk },
      STREAM_APPEND_TIMEOUT_MS,
      [chunk.buffer]
    );
    if (res.type !== 'void') {
      throw new Error('Unexpected response for appendPutBlobStream');
    }
    if (res.error) {
      throw new Error(res.error);
    }
  }

  async finishPutBlobStream(streamId: string): Promise<{ hashHex: string; nhash: string }> {
    const res = await this.request({ type: 'finishPutBlobStream', streamId }, PUT_BLOB_TIMEOUT_MS);
    if (res.type !== 'blobStored') {
      throw new Error('Unexpected response for finishPutBlobStream');
    }
    if (!res.hashHex || !res.nhash) {
      throw new Error('Failed to finalize blob stream');
    }
    return { hashHex: res.hashHex, nhash: res.nhash };
  }

  async cancelPutBlobStream(streamId: string): Promise<void> {
    const res = await this.request({ type: 'cancelPutBlobStream', streamId });
    if (res.type !== 'void') {
      throw new Error('Unexpected response for cancelPutBlobStream');
    }
    if (res.error) {
      throw new Error(res.error);
    }
  }

  async getBlob(hashHex: string): Promise<{ data: Uint8Array; source: BlobSource }> {
    const res = await this.request({ type: 'getBlob', hashHex });
    if (res.type !== 'blob') {
      throw new Error('Unexpected response for getBlob');
    }
    if (res.error || !res.data || !res.source) {
      throw new Error(res.error || 'Blob not found');
    }
    return { data: res.data, source: res.source };
  }

  async getBlobForPeer(hashHex: string): Promise<Uint8Array | null> {
    const res = await this.request({ type: 'getBlob', hashHex, forPeer: true });
    if (res.type !== 'blob') {
      throw new Error('Unexpected response for getBlobForPeer');
    }
    if (res.error || !res.data) {
      return null;
    }
    return res.data;
  }

  async setBlossomServers(servers: BlossomServerConfig[]): Promise<void> {
    const res = await this.request({ type: 'setBlossomServers', servers });
    if (res.type !== 'void') {
      throw new Error('Unexpected response for setBlossomServers');
    }
    if (res.error) {
      throw new Error(res.error);
    }
  }

  async registerMediaPort(port: MessagePort): Promise<void> {
    const res = await this.request({ type: 'registerMediaPort', port }, REQUEST_TIMEOUT_MS, [port]);
    if (res.type !== 'void') {
      throw new Error('Unexpected response for registerMediaPort');
    }
    if (res.error) {
      throw new Error(res.error);
    }
  }

  async setStorageMaxBytes(maxBytes: number): Promise<void> {
    const res = await this.request({ type: 'setStorageMaxBytes', maxBytes });
    if (res.type !== 'void') {
      throw new Error('Unexpected response for setStorageMaxBytes');
    }
    if (res.error) {
      throw new Error(res.error);
    }
  }

  async getStorageStats(): Promise<{ items: number; bytes: number; maxBytes: number }> {
    const res = await this.request({ type: 'getStorageStats' });
    if (res.type !== 'storageStats') {
      throw new Error('Unexpected response for getStorageStats');
    }
    if (res.error) {
      throw new Error(res.error);
    }
    return {
      items: res.items,
      bytes: res.bytes,
      maxBytes: res.maxBytes,
    };
  }

  async probeConnectivity(): Promise<ConnectivityState> {
    const res = await this.request({ type: 'probeConnectivity' });
    if (res.type !== 'connectivity') {
      throw new Error('Unexpected response for probeConnectivity');
    }
    if (res.error || !res.state) {
      throw new Error(res.error || 'Connectivity probe failed');
    }
    return res.state;
  }

  async resolveRoot(npub: string, path?: string, options: RootResolveOptions = {}): Promise<CID | null> {
    const res = await this.request({
      type: 'resolveRoot',
      npub,
      path,
      timeoutMs: options.timeoutMs,
      settleMs: options.settleMs,
    });
    if (res.type !== 'cid') {
      throw new Error('Unexpected response for resolveRoot');
    }
    if (res.error) {
      throw new Error(res.error);
    }
    return res.cid ?? null;
  }

  async watchRoot(
    npub: string,
    path: string | undefined,
    listener: (cid: CID | null) => void,
    options: RootResolveOptions = {},
  ): Promise<() => Promise<void>> {
    const res = await this.request({
      type: 'watchRoot',
      npub,
      path,
      timeoutMs: options.timeoutMs,
      settleMs: options.settleMs,
    });
    if (res.type !== 'rootWatchStarted') {
      throw new Error('Unexpected response for watchRoot');
    }
    if (res.error) {
      throw new Error(res.error);
    }

    this.rootWatchListeners.set(res.watchId, listener);
    if ('cid' in res) {
      listener(res.cid ?? null);
    }
    if (this.pendingRootWatchUpdates.has(res.watchId)) {
      const pendingCid = this.pendingRootWatchUpdates.get(res.watchId) ?? null;
      this.pendingRootWatchUpdates.delete(res.watchId);
      listener(pendingCid);
    }

    return async () => {
      this.rootWatchListeners.delete(res.watchId);
      this.pendingRootWatchUpdates.delete(res.watchId);
      try {
        const stopRes = await this.request({ type: 'unwatchRoot', watchId: res.watchId });
        if (stopRes.type !== 'void') {
          throw new Error('Unexpected response for unwatchRoot');
        }
        if (stopRes.error) {
          throw new Error(stopRes.error);
        }
      } catch {
        // Ignore cleanup failures after local listener removal.
      }
    };
  }

  onConnectivityUpdate(listener: (state: ConnectivityState) => void): () => void {
    this.connectivityListeners.add(listener);
    return () => {
      this.connectivityListeners.delete(listener);
    };
  }

  onUploadProgress(listener: (progress: UploadProgressState) => void): () => void {
    this.uploadProgressListeners.add(listener);
    return () => {
      this.uploadProgressListeners.delete(listener);
    };
  }

  onBlossomBandwidth(listener: (stats: BlossomBandwidthState) => void): () => void {
    this.blossomBandwidthListeners.add(listener);
    return () => {
      this.blossomBandwidthListeners.delete(listener);
    };
  }

  onDiagnostic(listener: (event: WorkerDiagnosticEvent) => void): () => void {
    this.diagnosticListeners.add(listener);
    return () => {
      this.diagnosticListeners.delete(listener);
    };
  }

  setP2PFetchHandler(handler: P2PFetchHandler | null): void {
    this.p2pFetchHandler = handler;
  }

  setP2PPeerListHandler(handler: P2PPeerListHandler | null): void {
    this.p2pPeerListHandler = handler;
  }

  async close(): Promise<void> {
    try {
      await this.request({ type: 'close' });
    } catch {
      // Ignore close errors.
    }
    this.rootWatchListeners.clear();
    this.pendingRootWatchUpdates.clear();
    this.worker?.terminate();
    this.worker = null;
    this.initPromise = null;
    this.rejectAllPending(new Error('Worker closed'));
  }
}
