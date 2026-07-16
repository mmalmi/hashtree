import {
  BlossomStore,
  type BlossomSigner,
  type BlossomUploadCallback,
  fromHex,
} from '@hashtree/core';
import { finalizeEvent, generateSecretKey } from 'nostr-tools/pure';
import type { BlossomServerConfig } from '../protocol.js';
import {
  BlossomBandwidthTracker,
  type BlossomBandwidthStats,
  type BlossomBandwidthUpdateHandler,
} from './blossomBandwidthTracker.js';

export const DEFAULT_BLOSSOM_SERVERS: BlossomServerConfig[] = [];

const MAX_CONCURRENT_READ_FETCHES = 32;
const MAX_CONCURRENT_HEAD_FETCHES = 64;
const DEFAULT_FETCH_TIMEOUT_MS = 6_000;

let activeReadFetches = 0;
const pendingReadFetchWaiters: Array<() => void> = [];
let activeHeadFetches = 0;
const pendingHeadFetchWaiters: Array<() => void> = [];

export type {
  BlossomBandwidthServerStats,
  BlossomBandwidthStats,
  BlossomBandwidthUpdateHandler,
} from './blossomBandwidthTracker.js';

function normalizeServerUrl(url: string): string {
  return url.replace(/\/+$/, '');
}

function normalizeServers(servers: BlossomServerConfig[] | undefined): BlossomServerConfig[] {
  const source = servers && servers.length > 0 ? servers : DEFAULT_BLOSSOM_SERVERS;
  const unique = new Map<string, BlossomServerConfig>();
  for (const server of source) {
    const url = normalizeServerUrl(server.url.trim());
    if (!url) continue;
    unique.set(url, {
      url,
      read: server.read ?? true,
      write: server.write ?? false,
    });
  }
  return Array.from(unique.values());
}

function createEphemeralSigner(): BlossomSigner {
  const secretKey = generateSecretKey();
  return async (template) => {
    const event = finalizeEvent({
      ...template,
      kind: template.kind as 24242,
      created_at: template.created_at,
      content: template.content,
      tags: template.tags,
    }, secretKey);
    return {
      kind: event.kind,
      created_at: event.created_at,
      content: event.content,
      tags: event.tags,
      pubkey: event.pubkey,
      id: event.id,
      sig: event.sig,
    };
  };
}

function releaseReadFetchSlot(): void {
  activeReadFetches = Math.max(0, activeReadFetches - 1);
  pendingReadFetchWaiters.shift()?.();
}

function withReadFetchSlot<T>(loader: () => Promise<T>): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const start = () => {
      activeReadFetches += 1;
      let pending: Promise<T>;
      try {
        pending = loader();
      } catch (error) {
        releaseReadFetchSlot();
        reject(error);
        return;
      }

      pending
        .then(resolve, reject)
        .finally(() => {
          releaseReadFetchSlot();
        });
    };

    if (activeReadFetches < MAX_CONCURRENT_READ_FETCHES) {
      start();
      return;
    }

    pendingReadFetchWaiters.push(start);
  });
}

function withHeadFetchSlot<T>(loader: () => Promise<T>): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const start = () => {
      activeHeadFetches += 1;
      Promise.resolve()
        .then(loader)
        .then(resolve, reject)
        .finally(() => {
          activeHeadFetches = Math.max(0, activeHeadFetches - 1);
          pendingHeadFetchWaiters.shift()?.();
        });
    };
    if (activeHeadFetches < MAX_CONCURRENT_HEAD_FETCHES) {
      start();
      return;
    }
    pendingHeadFetchWaiters.push(start);
  });
}

export class BlossomTransport {
  private servers: BlossomServerConfig[];
  private readonly signer: BlossomSigner;
  private readonly bandwidthTracker: BlossomBandwidthTracker;
  private readonly inflightFetches = new Map<string, Promise<Uint8Array | null>>();
  private readonly fetchTimeoutMs: number;
  private store: BlossomStore;

  constructor(
    servers?: BlossomServerConfig[],
    onBandwidthUpdate?: BlossomBandwidthUpdateHandler,
    fetchTimeoutMs = DEFAULT_FETCH_TIMEOUT_MS,
  ) {
    this.servers = normalizeServers(servers);
    this.signer = createEphemeralSigner();
    this.bandwidthTracker = new BlossomBandwidthTracker(onBandwidthUpdate);
    this.fetchTimeoutMs = fetchTimeoutMs;
    this.store = this.createStore(this.servers);
  }

  setServers(servers: BlossomServerConfig[]): void {
    this.servers = normalizeServers(servers);
    this.store = this.createStore(this.servers);
  }

  getServers(): BlossomServerConfig[] {
    return this.servers;
  }

  getReadServers(): BlossomServerConfig[] {
    return this.servers.filter((server) => server.read !== false);
  }

  getWriteServers(): BlossomServerConfig[] {
    return this.servers.filter(server => !!server.write);
  }

  getBandwidthStats(): BlossomBandwidthStats {
    return this.bandwidthTracker.getStats();
  }

  private createStore(servers: BlossomServerConfig[], onUploadProgress?: BlossomUploadCallback): BlossomStore {
    return new BlossomStore({
      servers,
      signer: this.signer,
      onUploadProgress,
      logger: (entry) => {
        this.bandwidthTracker.apply(entry);
      },
    });
  }

  createUploadStore(onUploadProgress?: BlossomUploadCallback): BlossomStore {
    return this.createStore(this.servers, onUploadProgress);
  }

  async upload(
    hashHex: string,
    data: Uint8Array,
    _mimeType?: string,
    onUploadProgress?: BlossomUploadCallback
  ): Promise<void> {
    if (!this.servers.some(server => server.write)) return;
    const uploadMimeType = 'application/octet-stream';
    if (onUploadProgress) {
      const store = this.createStore(this.servers, onUploadProgress);
      await store.put(fromHex(hashHex), data, uploadMimeType);
      return;
    }

    await this.store.put(fromHex(hashHex), data, uploadMimeType);
  }

  async fetch(hashHex: string): Promise<Uint8Array | null> {
    const inflight = this.inflightFetches.get(hashHex);
    if (inflight) {
      return inflight;
    }

    const pending = this.fetchInternal(hashHex, () => this.store.get(fromHex(hashHex)));
    this.inflightFetches.set(hashHex, pending);
    return await pending;
  }

  async fetchFromServer(hashHex: string, serverUrl: string): Promise<Uint8Array | null> {
    const normalizedServerUrl = normalizeServerUrl(serverUrl.trim());
    if (!normalizedServerUrl) {
      return null;
    }
    const key = `${normalizedServerUrl}::${hashHex}`;
    const inflight = this.inflightFetches.get(key);
    if (inflight) {
      return inflight;
    }

    const pending = this.fetchInternal(key, () => (
      this.store as BlossomStore & {
        getFromServers(hash: ReturnType<typeof fromHex>, serverUrls: readonly string[]): Promise<Uint8Array | null>;
      }
    ).getFromServers(fromHex(hashHex), [normalizedServerUrl]));
    this.inflightFetches.set(key, pending);
    return await pending;
  }

  async stat(hashHex: string): Promise<{ size: number | null } | null> {
    for (const server of this.getReadServers()) {
      try {
        const response = await withHeadFetchSlot(async () => await fetch(
          `${server.url}/${hashHex}.bin`,
          { method: 'HEAD', signal: AbortSignal.timeout(this.fetchTimeoutMs) },
        ));
        if (!response.ok) {
          continue;
        }
        const contentLength = Number(response.headers.get('content-length'));
        return {
          size: Number.isFinite(contentLength) && contentLength >= 0 ? contentLength : null,
        };
      } catch {
        continue;
      }
    }
    return null;
  }

  private fetchInternal(
    inflightKey: string,
    loader: () => Promise<Uint8Array | null>,
  ): Promise<Uint8Array | null> {
    return withReadFetchSlot(() => new Promise<Uint8Array | null>((resolve, reject) => {
      let settled = false;
      const timeoutId = setTimeout(() => {
        if (settled) return;
        settled = true;
        reject(new Error(`Blossom read timed out after ${this.fetchTimeoutMs}ms`));
      }, this.fetchTimeoutMs);

      loader()
        .then((data) => {
          if (settled) return;
          settled = true;
          clearTimeout(timeoutId);
          resolve(data);
        })
        .catch((error) => {
          if (settled) return;
          settled = true;
          clearTimeout(timeoutId);
          reject(error);
        });
    }))
      .finally(() => {
        this.inflightFetches.delete(inflightKey);
      });
  }
}
