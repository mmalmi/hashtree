/**
 * Tauri Worker Adapter
 *
 * Native Rust backend adapter for Tauri desktop app.
 * Provides the same interface as WorkerAdapter but uses Tauri IPC
 * instead of web worker postMessage.
 *
 * Implements all phases:
 * - Phase 1: Store operations (get/put/has/delete)
 * - Phase 2: Tree operations (readFile/writeFile/deleteFile/listDir)
 * - Phase 3: Nostr operations (subscribe/unsubscribe/publish)
 * - Phase 4: Social graph (follows/followers/WoT distance)
 * - Phase 6: Blossom (upload/download/exists)
 */

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { hexEncode, hexDecode, base64Encode, base64Decode } from '../utils/encoding';
import type {
  WorkerConfig,
  WorkerNostrFilter as NostrFilter,
  WorkerSignedEvent as SignedEvent,
  WorkerPeerStats as PeerStats,
  WorkerRelayStats as RelayStats,
  WorkerDirEntry as DirEntry,
  WorkerBlossomUploadProgress as BlossomUploadProgress,
  WorkerBlossomServerConfig as BlossomServerConfig,
  CID,
} from 'hashtree';

// Worker request/response types matching Rust types
interface WorkerRequest {
  type: string;
  id: string;
  [key: string]: unknown;
}

interface WorkerResponse {
  type: string;
  id?: string;
  subId?: string;
  data?: string | null;
  value?: boolean;
  error?: string;
  cid?: { hash: string; key?: string } | null;
  entries?: Array<{ name: string; hash: string; size: number; linkType: number; key?: string }> | null;
  pubkeys?: string[];
  distance?: number | null;
  users?: Array<[string, number]>;
  event?: unknown;
}

interface PendingRequest {
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
  timeoutId: ReturnType<typeof setTimeout>;
}

// Subscription callback handlers
interface SubscriptionCallbacks {
  onEvent?: (event: SignedEvent) => void;
  onEose?: () => void;
}

export class TauriWorkerAdapter {
  private pendingRequests = new Map<string, PendingRequest>();
  private ready = false;
  private requestId = 0;
  private unlisten: UnlistenFn | null = null;
  private subscriptions = new Map<string, SubscriptionCallbacks>();
  private globalEventCallback: ((event: SignedEvent) => void) | null = null;

  // Streaming callbacks
  private streamCallbacks = new Map<string, (chunk: Uint8Array, done: boolean) => void>();

  // Social graph version callback
  private socialGraphVersionCallback: ((version: number) => void) | null = null;

  // Blossom progress callbacks
  private blossomProgressCallback: ((progress: BlossomUploadProgress) => void) | null = null;
  private blossomPushProgressCallback: ((treeName: string, current: number, total: number) => void) | null = null;
  private blossomPushCompleteCallback: ((treeName: string, pushed: number, skipped: number, failed: number) => void) | null = null;

  async init(config: WorkerConfig): Promise<void> {
    if (this.ready) return;

    // Listen for responses from Rust backend
    this.unlisten = await listen<WorkerResponse>('worker_response', (event) => {
      this.handleResponse(event.payload);
    });

    // Send init command
    await this.request({ type: 'init', id: this.nextId() });
    this.ready = true;
    console.log('[TauriWorkerAdapter] Ready');

    // Set identity if provided (needed for Blossom auth)
    if (config.pubkey) {
      await this.setIdentity(config.pubkey, config.nsec);
    }

    // Set relays if provided
    if (config.relays?.length) {
      await this.setRelays(config.relays);
    }

    // Set blossom servers if provided
    if (config.blossomServers?.length) {
      await this.setBlossomServers(config.blossomServers);
    }
  }

  private nextId(): string {
    return `tauri-${++this.requestId}`;
  }

  private handleResponse(response: WorkerResponse): void {
    // Handle subscription events (no request id)
    if (response.type === 'event' && response.subId) {
      const callbacks = this.subscriptions.get(response.subId);
      if (callbacks?.onEvent && response.event) {
        callbacks.onEvent(response.event as SignedEvent);
      }
      if (this.globalEventCallback && response.event) {
        this.globalEventCallback(response.event as SignedEvent);
      }
      return;
    }

    if (response.type === 'eose' && response.subId) {
      const callbacks = this.subscriptions.get(response.subId);
      if (callbacks?.onEose) {
        callbacks.onEose();
      }
      return;
    }

    // Handle streaming chunks
    if (response.type === 'streamChunk' && response.id) {
      const callback = this.streamCallbacks.get(response.id);
      if (callback) {
        const done = (response as { done?: boolean }).done ?? false;
        const data = (response as { data?: string }).data;
        if (data) {
          callback(base64Decode(data), done);
        } else if (done) {
          callback(new Uint8Array(0), true);
        }
        if (done) {
          this.streamCallbacks.delete(response.id);
        }
      }
      return;
    }

    // Handle push progress
    if (response.type === 'pushProgress') {
      const payload = response as { treeName?: string; current?: number; total?: number };
      if (this.blossomPushProgressCallback && payload.treeName !== undefined) {
        this.blossomPushProgressCallback(payload.treeName, payload.current ?? 0, payload.total ?? 0);
      }
      return;
    }

    // Handle social graph version
    if (response.type === 'socialGraphVersion') {
      const version = (response as { version?: number }).version ?? 0;
      if (this.socialGraphVersionCallback) {
        this.socialGraphVersionCallback(version);
      }
      return;
    }

    const id = response.id;
    if (!id) {
      return;
    }

    const pending = this.pendingRequests.get(id);
    if (!pending) return;

    clearTimeout(pending.timeoutId);
    this.pendingRequests.delete(id);

    if (response.type === 'error') {
      pending.reject(new Error(response.error || 'Unknown error'));
    } else {
      pending.resolve(response);
    }
  }

  private async request<T>(msg: WorkerRequest): Promise<T> {
    return new Promise((resolve, reject) => {
      const timeoutId = setTimeout(() => {
        this.pendingRequests.delete(msg.id);
        reject(new Error('Request timeout'));
      }, 30000);

      this.pendingRequests.set(msg.id, {
        resolve: resolve as (value: unknown) => void,
        reject,
        timeoutId,
      });

      invoke('worker_message', { message: msg }).catch((err) => {
        this.pendingRequests.delete(msg.id);
        clearTimeout(timeoutId);
        reject(new Error(String(err)));
      });
    });
  }

  // ============================================================================
  // Phase 1: Store Operations
  // ============================================================================

  async get(hash: Uint8Array): Promise<Uint8Array | null> {
    const res = await this.request<WorkerResponse>({
      type: 'get',
      id: this.nextId(),
      hash: hexEncode(hash),
    });
    return res.data ? base64Decode(res.data) : null;
  }

  async put(hash: Uint8Array, data: Uint8Array): Promise<boolean> {
    const res = await this.request<WorkerResponse>({
      type: 'put',
      id: this.nextId(),
      hash: hexEncode(hash),
      data: base64Encode(data),
    });
    return res.value ?? false;
  }

  async has(hash: Uint8Array): Promise<boolean> {
    const res = await this.request<WorkerResponse>({
      type: 'has',
      id: this.nextId(),
      hash: hexEncode(hash),
    });
    return res.value ?? false;
  }

  async delete(hash: Uint8Array): Promise<boolean> {
    const res = await this.request<WorkerResponse>({
      type: 'delete',
      id: this.nextId(),
      hash: hexEncode(hash),
    });
    return res.value ?? false;
  }

  // ============================================================================
  // Phase 2: Tree Operations
  // ============================================================================

  private cidToRust(cid: CID): { hash: string; key?: string } {
    return {
      hash: hexEncode(cid.hash),
      key: cid.key ? hexEncode(cid.key) : undefined,
    };
  }

  private rustToCid(rust: { hash: string; key?: string }): CID {
    return {
      hash: hexDecode(rust.hash),
      key: rust.key ? hexDecode(rust.key) : undefined,
    };
  }

  async readFile(cid: CID): Promise<Uint8Array | null> {
    const res = await this.request<WorkerResponse>({
      type: 'readFile',
      id: this.nextId(),
      cid: this.cidToRust(cid),
    });
    return res.data ? base64Decode(res.data) : null;
  }

  async readFileRange(cid: CID, start: number, end?: number): Promise<Uint8Array | null> {
    const res = await this.request<WorkerResponse>({
      type: 'readFileRange',
      id: this.nextId(),
      cid: this.cidToRust(cid),
      start,
      end: end ?? null,
    });
    return res.data ? base64Decode(res.data) : null;
  }

  async *readFileStream(cid: CID): AsyncGenerator<Uint8Array> {
    const id = this.nextId();
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

    // Fire off the stream request
    invoke('worker_message', {
      message: {
        type: 'readFileStream',
        id,
        cid: this.cidToRust(cid),
      },
    }).catch((err) => {
      console.error('[TauriWorkerAdapter] Stream error:', err);
      done = true;
      resolveNext?.();
    });

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

    this.streamCallbacks.delete(id);
  }

  async writeFile(parentCid: CID | null, path: string, data: Uint8Array): Promise<CID> {
    const res = await this.request<WorkerResponse>({
      type: 'writeFile',
      id: this.nextId(),
      parentCid: parentCid ? this.cidToRust(parentCid) : null,
      path,
      data: base64Encode(data),
    });
    if (!res.cid) {
      throw new Error('writeFile returned no CID');
    }
    return this.rustToCid(res.cid);
  }

  async deleteFile(parentCid: CID, path: string): Promise<CID> {
    const res = await this.request<WorkerResponse>({
      type: 'deleteFile',
      id: this.nextId(),
      parentCid: this.cidToRust(parentCid),
      path,
    });
    if (!res.cid) {
      throw new Error('deleteFile returned no CID');
    }
    return this.rustToCid(res.cid);
  }

  async listDir(cid: CID): Promise<DirEntry[]> {
    const res = await this.request<WorkerResponse>({
      type: 'listDir',
      id: this.nextId(),
      cid: this.cidToRust(cid),
    });
    if (!res.entries) {
      return [];
    }
    return res.entries.map((e) => ({
      name: e.name,
      hash: hexDecode(e.hash),
      size: e.size,
      linkType: e.linkType,
      key: e.key ? hexDecode(e.key) : undefined,
    }));
  }

  async resolveRoot(npub: string, path?: string): Promise<CID | null> {
    const res = await this.request<WorkerResponse>({
      type: 'resolveRoot',
      id: this.nextId(),
      npub,
      path,
    });
    if (!res.cid) {
      return null;
    }
    return {
      hash: hexDecode(res.cid.hash),
      key: res.cid.key ? hexDecode(res.cid.key) : undefined,
    };
  }

  // ============================================================================
  // Phase 3: Nostr Operations
  // ============================================================================

  onEvent(callback: (event: SignedEvent) => void): void {
    this.globalEventCallback = callback;
  }

  subscribe(
    filters: NostrFilter[],
    callback?: (event: SignedEvent) => void,
    eose?: () => void
  ): string {
    const subId = this.nextId();

    // Store callbacks
    this.subscriptions.set(subId, {
      onEvent: callback,
      onEose: eose,
    });

    // Send subscribe request (fire and forget - events come via listener)
    this.request<WorkerResponse>({
      type: 'subscribe',
      id: subId,
      filters: filters,
    }).catch((err) => {
      console.error('[TauriWorkerAdapter] Subscribe error:', err);
      this.subscriptions.delete(subId);
    });

    return subId;
  }

  unsubscribe(subId: string): void {
    this.subscriptions.delete(subId);

    // Send unsubscribe request
    this.request<WorkerResponse>({
      type: 'unsubscribe',
      id: this.nextId(),
      subId,
    }).catch((err) => {
      console.error('[TauriWorkerAdapter] Unsubscribe error:', err);
    });
  }

  async publish(event: SignedEvent): Promise<void> {
    await this.request<WorkerResponse>({
      type: 'publish',
      id: this.nextId(),
      event,
    });
  }

  // ============================================================================
  // Phase 4: Social Graph Operations
  // ============================================================================

  async setIdentity(pubkey: string, nsec?: string): Promise<void> {
    await this.request<WorkerResponse>({
      type: 'setIdentity',
      id: this.nextId(),
      pubkey,
      nsec,
    });
  }

  onSocialGraphVersion(callback: (version: number) => void): void {
    this.socialGraphVersionCallback = callback;
  }

  async initSocialGraph(_rootPubkey?: string): Promise<{ version: number; size: number }> {
    // Social graph is initialized automatically in Rust backend
    return { version: 0, size: 0 };
  }

  async setSocialGraphRoot(pubkey: string): Promise<void> {
    // Set via setIdentity
    await this.setIdentity(pubkey);
  }

  handleSocialGraphEvents(events: unknown[]): void {
    // Process follow events and update social graph
    for (const event of events) {
      const e = event as { kind?: number; pubkey?: string; tags?: string[][] };
      if (e.kind === 3 && e.pubkey && e.tags) {
        // Kind 3 = follow list
        const follows = e.tags
          .filter((t) => t[0] === 'p' && t[1])
          .map((t) => t[1]);

        this.request<WorkerResponse>({
          type: 'updateFollows',
          id: this.nextId(),
          pubkey: e.pubkey,
          follows,
        }).catch((err) => {
          console.error('[TauriWorkerAdapter] updateFollows error:', err);
        });
      }
    }
  }

  async getFollowDistance(pubkey: string): Promise<number> {
    const res = await this.request<WorkerResponse>({
      type: 'getWotDistance',
      id: this.nextId(),
      target: pubkey,
    });
    return res.distance ?? 1000; // 1000 = unknown
  }

  async isFollowing(follower: string, followed: string): Promise<boolean> {
    const follows = await this.getFollows(follower);
    return follows.includes(followed);
  }

  async getFollows(pubkey: string): Promise<string[]> {
    const res = await this.request<WorkerResponse>({
      type: 'getFollows',
      id: this.nextId(),
      pubkey,
    });
    return res.pubkeys ?? [];
  }

  async getFollowers(pubkey: string): Promise<string[]> {
    const res = await this.request<WorkerResponse>({
      type: 'getFollowers',
      id: this.nextId(),
      pubkey,
    });
    return res.pubkeys ?? [];
  }

  async getFollowedByFriends(pubkey: string): Promise<string[]> {
    // Get friends who follow this pubkey
    // This is computed client-side using existing getFollows calls
    const ourFollows = await this.getFollows(pubkey);
    if (ourFollows.length === 0) return [];

    const results: string[] = [];
    // Check which of our follows also follow this pubkey
    for (const friend of ourFollows) {
      const theirFollows = await this.getFollows(friend);
      if (theirFollows.includes(pubkey)) {
        results.push(friend);
      }
    }
    return results;
  }

  fetchUserFollows(_pubkey: string): void {
    // In Tauri mode, follows are fetched via Nostr subscriptions
  }

  fetchUserFollowers(_pubkey: string): void {
    // In Tauri mode, followers are fetched via Nostr subscriptions
  }

  async getSocialGraphSize(): Promise<number> {
    const res = await this.request<WorkerResponse & { size?: number }>({
      type: 'getSocialGraphSize',
      id: this.nextId(),
    });
    return res.size ?? 0;
  }

  async getUsersByDistance(distance: number): Promise<string[]> {
    const res = await this.request<WorkerResponse>({
      type: 'getUsersWithinDistance',
      id: this.nextId(),
      maxDistance: distance,
    });
    // Filter to exact distance
    return (res.users ?? [])
      .filter(([_, d]) => d === distance)
      .map(([pubkey]) => pubkey);
  }

  // ============================================================================
  // Phase 6: Blossom Operations
  // ============================================================================

  onBlossomProgress(callback: (progress: BlossomUploadProgress) => void): void {
    this.blossomProgressCallback = callback;
  }

  onBlossomPushProgress(callback: (treeName: string, current: number, total: number) => void): void {
    this.blossomPushProgressCallback = callback;
  }

  onBlossomPushComplete(
    callback: (treeName: string, pushed: number, skipped: number, failed: number) => void
  ): void {
    this.blossomPushCompleteCallback = callback;
  }

  async startBlossomSession(_sessionId: string, _totalChunks: number): Promise<void> {
    // Blossom sessions not needed in Rust - uploads are atomic
  }

  async endBlossomSession(): Promise<void> {
    // Blossom sessions not needed in Rust
  }

  async blossomUpload(data: Uint8Array): Promise<string> {
    const res = await this.request<WorkerResponse>({
      type: 'blossomUpload',
      id: this.nextId(),
      data: base64Encode(data),
    });
    if (!res.data) {
      throw new Error('blossomUpload returned no hash');
    }
    return res.data;
  }

  async blossomDownload(hash: string): Promise<Uint8Array> {
    const res = await this.request<WorkerResponse>({
      type: 'blossomDownload',
      id: this.nextId(),
      hash,
    });
    if (!res.data) {
      throw new Error('blossomDownload returned no data');
    }
    return base64Decode(res.data);
  }

  async blossomExists(hash: string): Promise<boolean> {
    const res = await this.request<WorkerResponse>({
      type: 'blossomExists',
      id: this.nextId(),
      hash,
    });
    return res.value ?? false;
  }

  async pushToBlossom(
    cidHash: Uint8Array,
    cidKey?: Uint8Array,
    treeName?: string
  ): Promise<{ pushed: number; skipped: number; failed: number; errors?: string[] }> {
    const res = await this.request<{
      pushed: number;
      skipped: number;
      failed: number;
      errors?: string[];
    }>({
      type: 'pushToBlossom',
      id: this.nextId(),
      cid: {
        hash: hexEncode(cidHash),
        key: cidKey ? hexEncode(cidKey) : undefined,
      },
      treeName,
    });
    return {
      pushed: res.pushed ?? 0,
      skipped: res.skipped ?? 0,
      failed: res.failed ?? 0,
      errors: res.errors,
    };
  }

  async republishTrees(prefix?: string): Promise<{ count: number; encryptionErrors?: string[] }> {
    const res = await this.request<{ count?: number; encryptionErrors?: string[] }>({
      type: 'republishTrees',
      id: this.nextId(),
      pubkeyPrefix: prefix,
    });
    return {
      count: res.count ?? 0,
      encryptionErrors: res.encryptionErrors,
    };
  }

  async republishTree(pubkey: string, treeName: string): Promise<boolean> {
    const res = await this.request<{ value: boolean }>({
      type: 'republishTree',
      id: this.nextId(),
      pubkey,
      treeName,
    });
    return res.value ?? false;
  }

  // ============================================================================
  // Stats Operations
  // ============================================================================

  async getPeerStats(): Promise<PeerStats[]> {
    const res = await this.request<
      WorkerResponse & {
        peers?: Array<{ peerId: string; connected: boolean; pool: string }>;
      }
    >({
      type: 'getPeerStats',
      id: this.nextId(),
    });
    return (res.peers ?? []).map((p) => ({
      peerId: p.peerId,
      connected: p.connected,
      pool: p.pool as 'follows' | 'other',
    }));
  }

  async getRelayStats(): Promise<RelayStats[]> {
    const res = await this.request<
      WorkerResponse & {
        relays?: Array<{ url: string; connected: boolean; connecting: boolean }>;
      }
    >({
      type: 'getRelayStats',
      id: this.nextId(),
    });
    return (res.relays ?? []).map((r) => ({
      url: r.url,
      connected: r.connected,
      connecting: r.connecting,
    }));
  }

  async getStorageStats(): Promise<{
    items: number;
    bytes: number;
    pinnedItems: number;
    pinnedBytes: number;
    maxBytes: number;
  }> {
    const res = await this.request<
      WorkerResponse & {
        items?: number;
        bytes?: number;
        pinnedItems?: number;
        pinnedBytes?: number;
        maxBytes?: number;
      }
    >({
      type: 'getStorageStats',
      id: this.nextId(),
    });
    return {
      items: res.items ?? 0,
      bytes: res.bytes ?? 0,
      pinnedItems: res.pinnedItems ?? 0,
      pinnedBytes: res.pinnedBytes ?? 0,
      maxBytes: res.maxBytes ?? 0,
    };
  }

  async setStorageMaxBytes(maxBytes: number): Promise<void> {
    await this.request<WorkerResponse>({
      type: 'setStorageMaxBytes',
      id: this.nextId(),
      maxBytes,
    });
  }

  async runEviction(): Promise<number> {
    const res = await this.request<WorkerResponse & { bytesFreed?: number }>({
      type: 'runEviction',
      id: this.nextId(),
    });
    return res.bytesFreed ?? 0;
  }

  async blockPeer(_pubkey: string): Promise<void> {
    // WebRTC peer blocking not applicable for Tauri native backend
    // Tauri uses native networking, not browser WebRTC
  }

  async setWebRTCPools(
    pools: { follows: { max: number; satisfied: number }; other: { max: number; satisfied: number } }
  ): Promise<void> {
    await this.request<WorkerResponse>({
      type: 'setWebRTCPools',
      id: this.nextId(),
      followsMax: pools.follows.max,
      followsSatisfied: pools.follows.satisfied,
      otherMax: pools.other.max,
      otherSatisfied: pools.other.satisfied,
    });
  }

  async sendHello(): Promise<void> {
    await this.request<WorkerResponse>({
      type: 'sendHello',
      id: this.nextId(),
      roots: null,
    });
  }

  async setFollows(follows: string[]): Promise<void> {
    // Update follows via social graph
    // This requires knowing our pubkey, which is set via setIdentity
    // For now, no-op - follows are set via handleSocialGraphEvents
    console.log('[TauriWorkerAdapter] setFollows called with', follows.length, 'follows');
  }

  async setBlossomServers(servers: BlossomServerConfig[]): Promise<void> {
    // Convert from the config format to separate read/write lists
    const readServers = servers.filter((s) => s.read !== false).map((s) => s.url);
    const writeServers = servers.filter((s) => s.write !== false).map((s) => s.url);

    await this.request<WorkerResponse>({
      type: 'setBlossomServers',
      id: this.nextId(),
      readServers,
      writeServers,
    });
  }

  async getBlossomServers(): Promise<{ readServers: string[]; writeServers: string[] }> {
    const res = await this.request<
      WorkerResponse & { readServers?: string[]; writeServers?: string[] }
    >({
      type: 'getBlossomServers',
      id: this.nextId(),
    });
    return {
      readServers: res.readServers ?? [],
      writeServers: res.writeServers ?? [],
    };
  }

  async setRelays(relays: string[]): Promise<void> {
    await this.request<WorkerResponse>({
      type: 'setRelays',
      id: this.nextId(),
      relays,
    });
  }

  async getRelays(): Promise<string[]> {
    const res = await this.request<WorkerResponse & { relays?: string[] }>({
      type: 'getRelays',
      id: this.nextId(),
    });
    return res.relays ?? [];
  }

  // Media
  registerMediaPort(_port: MessagePort): void {
    // Tauri uses htree HTTP server for media
  }

  // Cleanup
  close(): void {
    if (this.unlisten) {
      this.unlisten();
      this.unlisten = null;
    }

    // Clear subscriptions
    this.subscriptions.clear();
    this.globalEventCallback = null;

    // Clear pending requests
    for (const pending of this.pendingRequests.values()) {
      clearTimeout(pending.timeoutId);
      pending.reject(new Error('Adapter closed'));
    }
    this.pendingRequests.clear();
    this.ready = false;
  }
}

// Singleton instance
let tauriAdapter: TauriWorkerAdapter | null = null;

export function getTauriWorkerAdapter(): TauriWorkerAdapter | null {
  return tauriAdapter;
}

export async function initTauriWorkerAdapter(config: WorkerConfig): Promise<TauriWorkerAdapter> {
  if (tauriAdapter) {
    return tauriAdapter;
  }

  tauriAdapter = new TauriWorkerAdapter();
  await tauriAdapter.init(config);
  return tauriAdapter;
}

export function closeTauriWorkerAdapter(): void {
  if (tauriAdapter) {
    tauriAdapter.close();
    tauriAdapter = null;
  }
}
