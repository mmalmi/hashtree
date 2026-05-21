import {
  MemoryStore,
  sha256,
  toHex,
  type Hash,
  type Store,
} from '@hashtree/core';
import {
  createRequest,
  createResponse,
  createFragmentResponse,
  encodeRequest,
  encodeResponse,
  hashToKey,
  isFragmented,
  MAX_HTL,
  MSG_TYPE_REQUEST,
  MSG_TYPE_RESPONSE,
  parseMessage,
  verifyHash,
  type DataRequest,
  type DataResponse,
} from '@hashtree/mesh';

export const DEFAULT_FIPS_DISCOVERY_APP = 'hashtree-v1';
export const DEFAULT_FIPS_REQUEST_TIMEOUT_MS = 5_500;
export const FIPS_RESPONSE_FRAGMENT_SIZE = 32 * 1024;
const DYNAMIC_PEER_POLL_INTERVAL_MS = 100;
const MAX_RESPONSE_FRAGMENTS = 16_384;

export interface FipsEndpointMessage {
  peerId: string;
  data: Uint8Array;
}

export interface FipsEndpoint {
  send(peerId: string, data: Uint8Array): Promise<void>;
  onMessage(handler: (message: FipsEndpointMessage) => void | Promise<void>): () => void;
  listPeerIds?(): readonly string[] | Promise<readonly string[]>;
  localPeerId?(): string;
  close?(): void;
}

export interface FipsNodeEndpointData {
  src: string;
  dst: string;
  payload: Uint8Array;
}

export interface FipsNodePeerEvent {
  remotePubkey: string;
  state: 'connected' | 'disconnected';
}

export interface FipsNodeLike {
  identity?: {
    publicKey?: Uint8Array;
  };
  sendEndpointData(args: {
    dst: string;
    payload: Uint8Array;
  }): Promise<void>;
  on(event: 'endpointData', handler: (event: FipsNodeEndpointData) => void): () => void;
  on(event: 'peer', handler: (event: FipsNodePeerEvent) => void): () => void;
}

export interface FipsNodeEndpointOptions {
  initialPeers?: readonly string[];
}

export type FipsPeerSource = readonly string[] | (() => readonly string[] | Promise<readonly string[]>);

export interface HashtreeFipsTransportOptions {
  endpoint: FipsEndpoint;
  localStore?: Store;
  peers?: FipsPeerSource;
  requestTimeoutMs?: number;
  requestHtl?: number;
  cacheResponses?: boolean;
}

export interface FipsReadSource {
  id: string;
  get(hash: Hash): Promise<Uint8Array | null>;
  isAvailable?: () => boolean;
}

interface PendingBlobRequest {
  hash: Hash;
  resolve: (data: Uint8Array | null) => void;
  timer: ReturnType<typeof setTimeout>;
}

interface PendingBlobRequestHandle {
  promise: Promise<Uint8Array | null>;
  isSettled: () => boolean;
}

interface ResponseReassembly {
  total: number;
  fragments: Map<number, Uint8Array>;
  receivedBytes: number;
}

function copyBytes(data: Uint8Array): Uint8Array {
  return data.slice();
}

function bytesToHex(data: Uint8Array): string {
  let out = '';
  for (const byte of data) {
    out += byte.toString(16).padStart(2, '0');
  }
  return out;
}

function bytesEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i += 1) {
    if (a[i] !== b[i]) return false;
  }
  return true;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, Math.max(0, ms)));
}

function normalizePeers(peers: readonly string[], localPeerId?: string): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const peer of peers) {
    const id = `${peer}`.trim();
    if (!id || id === localPeerId || seen.has(id)) continue;
    seen.add(id);
    out.push(id);
  }
  return out;
}

async function resolvePeerSource(
  endpoint: FipsEndpoint,
  peers?: FipsPeerSource,
): Promise<string[]> {
  const resolved = typeof peers === 'function'
    ? await peers()
    : peers ?? await endpoint.listPeerIds?.() ?? [];
  return normalizePeers(resolved, endpoint.localPeerId?.());
}

async function verifiedLocalGet(store: Store, hash: Hash): Promise<Uint8Array | null> {
  const data = await store.get(hash);
  if (!data) return null;
  return await verifyHash(data, hash) ? data : null;
}

export function createFipsNodeEndpoint(
  node: FipsNodeLike,
  options: FipsNodeEndpointOptions = {},
): FipsEndpoint {
  const peers = new Set<string>();
  const dataUnsubs = new Set<() => void>();
  for (const peer of options.initialPeers ?? []) {
    const id = `${peer}`.trim();
    if (id) peers.add(id);
  }
  const localPeerId = node.identity?.publicKey ? bytesToHex(node.identity.publicKey) : undefined;

  const peerUnsub = node.on('peer', (event) => {
    if (event.state === 'connected') {
      peers.add(event.remotePubkey);
    } else {
      peers.delete(event.remotePubkey);
    }
  });

  return {
    localPeerId: () => localPeerId ?? '',
    listPeerIds: () => normalizePeers(Array.from(peers), localPeerId),
    send: (peerId, data) => node.sendEndpointData({
      dst: peerId,
      payload: copyBytes(data),
    }),
    onMessage: (handler) => {
      const dataUnsub = node.on('endpointData', (event) => {
        peers.add(event.src);
        void Promise.resolve(handler({
          peerId: event.src,
          data: copyBytes(event.payload),
        })).catch(() => undefined);
      });
      dataUnsubs.add(dataUnsub);
      return () => {
        dataUnsub();
        dataUnsubs.delete(dataUnsub);
      };
    },
    close: () => {
      peerUnsub();
      for (const unsubscribe of dataUnsubs) unsubscribe();
      dataUnsubs.clear();
    },
  };
}

export class HashtreeFipsTransport {
  private readonly endpoint: FipsEndpoint;
  private readonly localStore: Store;
  private peers?: FipsPeerSource;
  private readonly requestTimeoutMs: number;
  private readonly requestHtl: number;
  private readonly cacheResponses: boolean;
  private readonly pending = new Map<string, PendingBlobRequest[]>();
  private readonly responseFragments = new Map<string, ResponseReassembly>();
  private unsubscribe: (() => void) | null = null;

  constructor(options: HashtreeFipsTransportOptions) {
    this.endpoint = options.endpoint;
    this.localStore = options.localStore ?? new MemoryStore();
    this.peers = options.peers;
    this.requestTimeoutMs = options.requestTimeoutMs ?? DEFAULT_FIPS_REQUEST_TIMEOUT_MS;
    this.requestHtl = options.requestHtl ?? MAX_HTL;
    this.cacheResponses = options.cacheResponses ?? true;
    this.unsubscribe = this.endpoint.onMessage((message) => {
      void this.handleMessage(message).catch(() => undefined);
    });
  }

  close(): void {
    this.unsubscribe?.();
    this.unsubscribe = null;
    this.endpoint.close?.();
    for (const pending of this.pending.values()) {
      for (const request of pending) {
        clearTimeout(request.timer);
        request.resolve(null);
      }
    }
    this.pending.clear();
    this.responseFragments.clear();
  }

  setPeers(peers: FipsPeerSource): void {
    this.peers = peers;
  }

  async get(hash: Hash, peers?: FipsPeerSource): Promise<Uint8Array | null> {
    const local = await verifiedLocalGet(this.localStore, hash);
    if (local) return local;

    const peerSource = peers ?? this.peers;
    if (typeof peerSource === 'function') {
      return this.requestFromDynamicPeers(hash, peerSource);
    }

    const peerIds = await resolvePeerSource(this.endpoint, peerSource);
    if (peerIds.length === 0) return null;

    return this.requestFromPeers(hash, peerIds);
  }

  async put(hash: Hash, data: Uint8Array): Promise<boolean> {
    const computed = await sha256(data);
    if (!bytesEqual(computed, hash)) {
      throw new Error(`hashtree fips transport put hash mismatch: ${toHex(hash)}`);
    }
    return this.localStore.put(hash, copyBytes(data));
  }

  async handleMessage(message: FipsEndpointMessage): Promise<void> {
    const parsed = parseMessage(message.data);
    if (!parsed) return;

    if (parsed.type === MSG_TYPE_REQUEST) {
      await this.handleRequest(message.peerId, parsed.body);
      return;
    }

    if (parsed.type === MSG_TYPE_RESPONSE) {
      await this.handleResponse(parsed.body);
    }
  }

  createReadSource(id = 'fips'): FipsReadSource {
    return {
      id,
      get: (hash) => this.get(hash),
      isAvailable: () => true,
    };
  }

  private async handleRequest(peerId: string, req: DataRequest): Promise<void> {
    const data = await verifiedLocalGet(this.localStore, req.h);
    if (!data) return;
    await this.sendResponse(peerId, req.h, data);
  }

  private async handleResponse(resp: DataResponse): Promise<void> {
    const hashKey = hashToKey(resp.h);
    const pending = this.pending.get(hashKey);
    if (!pending) return;

    const data = isFragmented(resp)
      ? this.handleResponseFragment(hashKey, resp)
      : copyBytes(resp.d);
    if (!data) return;

    if (!await verifyHash(data, resp.h)) return;

    this.pending.delete(hashKey);
    this.responseFragments.delete(hashKey);
    if (this.cacheResponses) {
      await this.localStore.put(resp.h, copyBytes(data)).catch(() => false);
    }
    for (const request of pending) {
      clearTimeout(request.timer);
      request.resolve(copyBytes(data));
    }
  }

  private async sendResponse(peerId: string, hash: Hash, data: Uint8Array): Promise<void> {
    if (data.byteLength <= FIPS_RESPONSE_FRAGMENT_SIZE) {
      await this.endpoint.send(peerId, new Uint8Array(encodeResponse(createResponse(hash, data))));
      return;
    }

    const total = Math.ceil(data.byteLength / FIPS_RESPONSE_FRAGMENT_SIZE);
    for (let index = 0; index < total; index += 1) {
      const start = index * FIPS_RESPONSE_FRAGMENT_SIZE;
      const end = Math.min(start + FIPS_RESPONSE_FRAGMENT_SIZE, data.byteLength);
      const fragment = data.slice(start, end);
      const response = createFragmentResponse(hash, fragment, index, total);
      await this.endpoint.send(peerId, new Uint8Array(encodeResponse(response)));
    }
  }

  private handleResponseFragment(hashKey: string, resp: DataResponse): Uint8Array | null {
    const index = resp.i ?? -1;
    const total = resp.n ?? 0;
    if (
      !Number.isInteger(index)
      || !Number.isInteger(total)
      || index < 0
      || total <= 0
      || total > MAX_RESPONSE_FRAGMENTS
      || index >= total
    ) {
      return null;
    }

    let reassembly = this.responseFragments.get(hashKey);
    if (!reassembly || reassembly.total !== total) {
      reassembly = {
        total,
        fragments: new Map(),
        receivedBytes: 0,
      };
      this.responseFragments.set(hashKey, reassembly);
    }

    if (!reassembly.fragments.has(index)) {
      const fragment = copyBytes(resp.d);
      reassembly.fragments.set(index, fragment);
      reassembly.receivedBytes += fragment.byteLength;
    }

    if (reassembly.fragments.size !== reassembly.total) {
      return null;
    }

    const assembled = new Uint8Array(reassembly.receivedBytes);
    let offset = 0;
    for (let fragmentIndex = 0; fragmentIndex < reassembly.total; fragmentIndex += 1) {
      const fragment = reassembly.fragments.get(fragmentIndex);
      if (!fragment) return null;
      assembled.set(fragment, offset);
      offset += fragment.byteLength;
    }
    return assembled;
  }

  private async requestFromPeers(hash: Hash, peers: readonly string[]): Promise<Uint8Array | null> {
    const hashKey = hashToKey(hash);
    const pending = this.createPendingRequest(hashKey, hash);
    const payload = new Uint8Array(encodeRequest(createRequest(hash, this.requestHtl)));
    const sent = await this.sendRequestToPeers(payload, peers);
    if (sent === 0) {
      this.resolvePendingMiss(hashKey);
    }

    return pending.promise;
  }

  private async requestFromDynamicPeers(hash: Hash, peers: () => readonly string[] | Promise<readonly string[]>): Promise<Uint8Array | null> {
    const hashKey = hashToKey(hash);
    const pending = this.createPendingRequest(hashKey, hash);
    const payload = new Uint8Array(encodeRequest(createRequest(hash, this.requestHtl)));
    const attempted = new Set<string>();
    const deadline = Date.now() + this.requestTimeoutMs;

    while (!pending.isSettled()) {
      const peerIds = await resolvePeerSource(this.endpoint, peers).catch(() => []);
      const newPeers = peerIds.filter((peerId) => !attempted.has(peerId));
      for (const peerId of newPeers) {
        attempted.add(peerId);
      }
      if (newPeers.length > 0) {
        void this.sendRequestToPeers(payload, newPeers);
      }

      const remainingMs = deadline - Date.now();
      if (remainingMs <= 0) {
        break;
      }
      await Promise.race([
        pending.promise.then(() => undefined),
        sleep(Math.min(DYNAMIC_PEER_POLL_INTERVAL_MS, remainingMs)),
      ]);
    }

    return pending.promise;
  }

  private createPendingRequest(hashKey: string, hash: Hash): PendingBlobRequestHandle {
    let settled = false;
    const promise = new Promise<Uint8Array | null>((resolve) => {
      const finish = (data: Uint8Array | null): void => {
        if (settled) return;
        settled = true;
        resolve(data);
      };
      const timer = setTimeout(() => {
        this.removePendingRequest(hashKey, finish);
        finish(null);
      }, this.requestTimeoutMs);
      const pending = this.pending.get(hashKey) ?? [];
      pending.push({ hash, resolve: finish, timer });
      this.pending.set(hashKey, pending);
    });
    return {
      promise,
      isSettled: () => settled,
    };
  }

  private removePendingRequest(hashKey: string, resolve: (data: Uint8Array | null) => void): void {
    const pending = this.pending.get(hashKey) ?? [];
    const remaining = pending.filter((request) => request.resolve !== resolve);
    if (remaining.length > 0) {
      this.pending.set(hashKey, remaining);
    } else {
      this.pending.delete(hashKey);
      this.responseFragments.delete(hashKey);
    }
  }

  private async sendRequestToPeers(payload: Uint8Array, peers: readonly string[]): Promise<number> {
    const sends = await Promise.allSettled(
      peers.map((peerId) => this.endpoint.send(peerId, copyBytes(payload))),
    );
    return sends.filter((result) => result.status === 'fulfilled').length;
  }

  private resolvePendingMiss(hashKey: string): void {
    const pending = this.pending.get(hashKey);
    if (!pending) return;
    this.pending.delete(hashKey);
    this.responseFragments.delete(hashKey);
    for (const request of pending) {
      clearTimeout(request.timer);
      request.resolve(null);
    }
  }
}

export interface FipsTransportStoreOptions extends HashtreeFipsTransportOptions {
  localStore: Store;
}

export class FipsTransportStore implements Store {
  readonly transport: HashtreeFipsTransport;
  private readonly localStore: Store;

  constructor(options: FipsTransportStoreOptions) {
    this.localStore = options.localStore;
    this.transport = new HashtreeFipsTransport(options);
  }

  close(): void {
    this.transport.close();
  }

  put(hash: Hash, data: Uint8Array): Promise<boolean> {
    return this.transport.put(hash, data);
  }

  async get(hash: Hash): Promise<Uint8Array | null> {
    const local = await verifiedLocalGet(this.localStore, hash);
    if (local) return local;
    return this.transport.get(hash);
  }

  has(hash: Hash): Promise<boolean> {
    return this.localStore.has(hash);
  }

  delete(hash: Hash): Promise<boolean> {
    return this.localStore.delete(hash);
  }

  watch(hash: Hash, callback: (data: Uint8Array) => void): () => void {
    return this.localStore.watch?.(hash, callback) ?? (() => {});
  }
}
