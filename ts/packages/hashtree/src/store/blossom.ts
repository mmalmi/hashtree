/**
 * Blossom content-addressed store
 * Uses Blossom protocol for remote blob storage
 */

import { StoreWithMeta, Hash, toHex } from '../types.js';
import { sha256 } from '../hash.js';

/**
 * Blossom server configuration
 */
export interface BlossomServer {
  url: string;
  /** Whether this server accepts reads (defaults to true) */
  read?: boolean;
  /** Whether this server accepts writes */
  write?: boolean;
  /** Prefer POST /blob/batch over raw /{hash}.bin reads for browser clients. */
  preferBatchReads?: boolean;
}

/**
 * Blossom auth event (NIP-98 style)
 */
export interface BlossomAuthEvent {
  kind: number;
  created_at: number;
  content: string;
  tags: string[][];
  pubkey: string;
  id: string;
  sig: string;
}

/**
 * Signer function for Blossom auth
 */
export type BlossomSigner = (event: {
  kind: 24242;
  created_at: number;
  content: string;
  tags: string[][];
}) => Promise<BlossomAuthEvent>;

/** Log entry for blossom operations */
export interface BlossomLogEntry {
  timestamp: number;
  operation: 'get' | 'put' | 'has' | 'delete';
  server: string;
  hash: string;
  success: boolean;
  error?: string;
  bytes?: number;
}

/** Logger callback for blossom operations */
export type BlossomLogger = (entry: BlossomLogEntry) => void;

/** Callback for upload progress per-server */
export type BlossomUploadCallback = (serverUrl: string, status: 'uploaded' | 'skipped' | 'failed') => void;

export interface BlossomStoreConfig {
  /** Blossom servers to use */
  servers: (string | BlossomServer)[];
  /** Signer for write operations */
  signer?: BlossomSigner;
  /** Optional logger for operations */
  logger?: BlossomLogger;
  /** Optional callback for upload progress (per-server, per-chunk) */
  onUploadProgress?: BlossomUploadCallback;
  /** Timeout for a single Blossom read request (defaults to 60 seconds) */
  getTimeoutMs?: number;
  /** Timeout for a single Blossom upload request (defaults to 120 seconds) */
  putTimeoutMs?: number;
  /** Maximum concurrent uploads across overlapping push operations (defaults to 4). */
  maxConcurrentWrites?: number;
  /** Skip pre-upload HEAD probes; useful when the write endpoint handles duplicates. */
  skipExistenceCheck?: boolean;
}

/** Server health tracking for backoff */
interface ServerHealth {
  lastErrorTime: number;
  consecutiveErrors: number;
}

interface ReadServerStats {
  requests: number;
  successes: number;
  misses: number;
  failures: number;
  timeouts: number;
  srttMs: number;
  rttvarMs: number;
  lastSuccessMs?: number;
  lastFailureMs?: number;
}

interface InFlightReadRequest {
  server: BlossomServer;
  settled: boolean;
  promise: Promise<{ serverUrl: string; data: Uint8Array | null; error?: Error }>;
}

/** Backoff config */
const BASE_BACKOFF_MS = 1000; // 1 second
const MAX_BACKOFF_MS = 60000; // 1 minute
const MAX_HASH_ATTEMPTS = 4; // Give up after this many attempts per hash

/** Size threshold for existence check before upload (256KB) */
const EXISTENCE_CHECK_THRESHOLD = 256 * 1024;

/** Timeout for HEAD requests (5 seconds) */
const HEAD_TIMEOUT_MS = 15_000;
const DEFAULT_GET_TIMEOUT_MS = 60_000;
const PUT_TIMEOUT_MS = 120_000;
const DEFAULT_MAX_CONCURRENT_WRITES = 4;
const MAX_WRITE_BACKOFF_WAIT_MS = 5_000;
const GET_HEDGE_INTERVAL_MS = 75;
const READ_SCORE_TIE_DELTA = 0.12;
const BLOB_BATCH_DOWNLOAD_MAGIC = new Uint8Array([72, 84, 66, 68, 86, 49, 0, 0]); // HTBDV1\0\0
const BLOB_BATCH_DOWNLOAD_CONTENT_TYPE = 'application/vnd.hashtree.blob-batch.v1+octet-stream';

/** Per-hash failure tracking */
interface HashAttempts {
  attempts: number;
  lastAttempt: number;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}

function normalizeMaxConcurrentWrites(value: number | undefined): number {
  const normalized = value ?? DEFAULT_MAX_CONCURRENT_WRITES;
  if (!Number.isSafeInteger(normalized) || normalized < 1 || normalized > 32) {
    throw new Error('maxConcurrentWrites must be an integer between 1 and 32');
  }
  return normalized;
}

function defaultReadStats(): ReadServerStats {
  return {
    requests: 0,
    successes: 0,
    misses: 0,
    failures: 0,
    timeouts: 0,
    srttMs: 0,
    rttvarMs: 0,
  };
}

export class BlossomStore implements StoreWithMeta {
  private servers: BlossomServer[];
  private signer?: BlossomSigner;
  private logger?: BlossomLogger;
  private onUploadProgress?: BlossomUploadCallback;
  private getTimeoutMs: number;
  private putTimeoutMs: number;
  private maxConcurrentWrites: number;
  private skipExistenceCheck: boolean;
  private serverHealth: Map<string, ServerHealth> = new Map();
  private readStats: Map<string, ReadServerStats> = new Map();
  private hashAttempts: Map<string, HashAttempts> = new Map();
  private activeWrites = 0;
  private readonly writeWaiters: Array<() => void> = [];

  constructor(config: BlossomStoreConfig) {
    this.servers = config.servers.map(s =>
      typeof s === 'string' ? { url: s, write: false } : s
    );
    this.signer = config.signer;
    this.logger = config.logger;
    this.onUploadProgress = config.onUploadProgress;
    this.getTimeoutMs = config.getTimeoutMs ?? DEFAULT_GET_TIMEOUT_MS;
    this.putTimeoutMs = config.putTimeoutMs ?? PUT_TIMEOUT_MS;
    this.maxConcurrentWrites = normalizeMaxConcurrentWrites(config.maxConcurrentWrites);
    this.skipExistenceCheck = config.skipExistenceCheck === true;
  }

  /** Get list of write-enabled server URLs */
  getWriteServers(): string[] {
    return this.servers.filter(s => s.write).map(s => s.url);
  }

  /** Get read-enabled servers in configured order. */
  getReadServers(): BlossomServer[] {
    return this.servers.filter((server) => server.read !== false);
  }

  /** Check if server is in backoff period */
  private isServerInBackoff(serverUrl: string): boolean {
    return this.serverBackoffRemainingMs(serverUrl) > 0;
  }

  private serverBackoffRemainingMs(serverUrl: string, now = Date.now()): number {
    const health = this.serverHealth.get(serverUrl);
    if (!health || health.consecutiveErrors === 0) return 0;

    const backoffMs = Math.min(
      BASE_BACKOFF_MS * Math.pow(2, health.consecutiveErrors - 1),
      MAX_BACKOFF_MS
    );
    return Math.max(0, backoffMs - (now - health.lastErrorTime));
  }

  /** Record server error */
  private recordError(serverUrl: string): void {
    const health = this.serverHealth.get(serverUrl) || { lastErrorTime: 0, consecutiveErrors: 0 };
    health.lastErrorTime = Date.now();
    health.consecutiveErrors++;
    this.serverHealth.set(serverUrl, health);
  }

  /** Record server success - reset backoff */
  private recordSuccess(serverUrl: string): void {
    this.serverHealth.delete(serverUrl);
  }

  private readStatsFor(serverUrl: string): ReadServerStats {
    const existing = this.readStats.get(serverUrl);
    if (existing) {
      return existing;
    }
    const created = defaultReadStats();
    this.readStats.set(serverUrl, created);
    return created;
  }

  private recordReadRequest(serverUrl: string): void {
    this.readStatsFor(serverUrl).requests += 1;
  }

  private recordReadMiss(serverUrl: string): void {
    this.readStatsFor(serverUrl).misses += 1;
  }

  private recordReadSuccess(serverUrl: string, elapsedMs: number): void {
    const stats = this.readStatsFor(serverUrl);
    const now = Date.now();
    stats.successes += 1;
    stats.lastSuccessMs = now;
    if (stats.srttMs === 0) {
      stats.srttMs = elapsedMs;
      stats.rttvarMs = elapsedMs / 2;
    } else {
      stats.rttvarMs = 0.75 * stats.rttvarMs + 0.25 * Math.abs(stats.srttMs - elapsedMs);
      stats.srttMs = 0.875 * stats.srttMs + 0.125 * elapsedMs;
    }
    this.recordSuccess(serverUrl);
  }

  private recordReadFailure(serverUrl: string): void {
    const stats = this.readStatsFor(serverUrl);
    stats.failures += 1;
    stats.lastFailureMs = Date.now();
    this.recordError(serverUrl);
  }

  private recordReadTimeout(serverUrl: string): void {
    const stats = this.readStatsFor(serverUrl);
    stats.timeouts += 1;
    stats.lastFailureMs = Date.now();
    this.recordError(serverUrl);
  }

  private readReliabilityScore(stats: ReadServerStats): number {
    return (stats.successes + 1) / (stats.requests + 2);
  }

  private readLatencyScore(stats: ReadServerStats): number {
    if (stats.srttMs <= 0) {
      return 0.5;
    }
    return Math.min(1, 500 / (stats.srttMs + 50));
  }

  private hasReadHistory(stats: ReadServerStats): boolean {
    return stats.requests > 0
      || stats.successes > 0
      || stats.misses > 0
      || stats.failures > 0
      || stats.timeouts > 0;
  }

  private defaultReadPreference(server: BlossomServer): number {
    let score = server.write ? 0 : 0.08;
    try {
      const hostname = new URL(server.url).hostname.toLowerCase();
      if (hostname.includes('cdn') || hostname.includes('cache') || hostname.includes('edge')) {
        score += 0.06;
      }
      if (hostname.includes('upload') || hostname.includes('origin')) {
        score -= 0.04;
      }
    } catch {
      // Ignore invalid URLs and use the basic read/write preference only.
    }
    return score;
  }

  private scoreReadServer(server: BlossomServer, now: number): number {
    if (this.isServerInBackoff(server.url)) {
      return Number.NEGATIVE_INFINITY;
    }
    const stats = this.readStatsFor(server.url);
    const missPenalty = stats.requests > 0 ? (stats.misses / stats.requests) * 0.2 : 0;
    const failurePenalty = stats.requests > 0 ? ((stats.failures + stats.timeouts) / stats.requests) * 0.35 : 0;
    const recencyBonus = stats.lastSuccessMs && now - stats.lastSuccessMs < 60_000 ? 0.1 : 0;
    return this.defaultReadPreference(server)
      + 0.55 * this.readReliabilityScore(stats)
      + 0.3 * this.readLatencyScore(stats)
      + recencyBonus
      - missPenalty
      - failurePenalty;
  }

  private orderedReadServers(readServers: BlossomServer[]): BlossomServer[] {
    const now = Date.now();
    return [...readServers].sort((left, right) => {
      const scoreDiff = this.scoreReadServer(right, now) - this.scoreReadServer(left, now);
      if (Math.abs(scoreDiff) > Number.EPSILON) {
        return scoreDiff;
      }
      const leftPreference = this.defaultReadPreference(left);
      const rightPreference = this.defaultReadPreference(right);
      if (leftPreference !== rightPreference) {
        return rightPreference - leftPreference;
      }
      return left.url.localeCompare(right.url);
    });
  }

  private shouldProbeMultipleReadServers(readServers: BlossomServer[]): boolean {
    if (readServers.length <= 1) {
      return false;
    }
    const [best, next] = readServers;
    if (!best || !next) {
      return false;
    }
    const bestStats = this.readStatsFor(best.url);
    const nextStats = this.readStatsFor(next.url);
    if (!this.hasReadHistory(bestStats) || !this.hasReadHistory(nextStats)) {
      return false;
    }
    const now = Date.now();
    return (this.scoreReadServer(best, now) - this.scoreReadServer(next, now)) < READ_SCORE_TIE_DELTA;
  }

  private isTimeoutError(error: unknown): boolean {
    if (error instanceof Error) {
      return error.name === 'AbortError'
        || error.name === 'TimeoutError'
        || /timed?\s*out/i.test(error.message);
    }
    return false;
  }

  private async getFromBatchEndpoint(
    server: BlossomServer,
    hashHex: string,
    signal: AbortSignal
  ): Promise<Uint8Array | null> {
    const response = await fetch(`${server.url}/blob/batch`, {
      method: 'POST',
      headers: {
        'Accept': BLOB_BATCH_DOWNLOAD_CONTENT_TYPE,
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({ hashes: [hashHex] }),
      signal,
    });
    if (!response.ok) {
      if (response.status === 404) return null;
      throw new Error(`Blossom batch endpoint ${server.url} returned HTTP ${response.status}`);
    }

    const bytes = new Uint8Array(await response.arrayBuffer());
    if (bytes.length < 12) {
      throw new Error(`Blossom batch endpoint ${server.url} returned a short response`);
    }
    for (let i = 0; i < BLOB_BATCH_DOWNLOAD_MAGIC.length; i += 1) {
      if (bytes[i] !== BLOB_BATCH_DOWNLOAD_MAGIC[i]) {
        throw new Error(`Blossom batch endpoint ${server.url} returned invalid framing`);
      }
    }

    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const count = view.getUint32(8, false);
    let offset = 12;
    for (let i = 0; i < count; i += 1) {
      if (bytes.length - offset < 40) {
        throw new Error(`Blossom batch endpoint ${server.url} returned a truncated entry`);
      }
      const entryHash = bytes.slice(offset, offset + 32);
      offset += 32;
      const len = Number(view.getBigUint64(offset, false));
      offset += 8;
      if (!Number.isSafeInteger(len) || bytes.length - offset < len) {
        throw new Error(`Blossom batch endpoint ${server.url} returned an invalid entry length`);
      }
      const data = bytes.slice(offset, offset + len);
      offset += len;
      if (toHex(entryHash) !== hashHex) {
        continue;
      }
      const computed = await sha256(data);
      if (toHex(computed) === hashHex) {
        return data;
      }
      throw new Error(`Blossom batch endpoint ${server.url} returned data with a hash mismatch`);
    }

    return null;
  }

  private createInFlightReadRequest(server: BlossomServer, hashHex: string): InFlightReadRequest {
    const startedAt = Date.now();
    this.recordReadRequest(server.url);
    const signal = AbortSignal.timeout(this.getTimeoutMs);
    let attemptedBatch = false;
    let batchError: Error | undefined;
    const tryBatchRead = async (): Promise<Uint8Array | null> => {
      attemptedBatch = true;
      try {
        return await this.getFromBatchEndpoint(server, hashHex, signal);
      } catch (error) {
        batchError = error instanceof Error ? error : new Error(String(error));
        return null;
      }
    };
    return {
      server,
      settled: false,
      promise: (server.preferBatchReads ? tryBatchRead() : Promise.resolve(null))
        .then(async (preferredBatchData) => {
          if (preferredBatchData) {
            this.log({ operation: 'get', server: server.url, hash: hashHex, success: true, bytes: preferredBatchData.length });
            this.recordReadSuccess(server.url, Math.max(1, Date.now() - startedAt));
            return { serverUrl: server.url, data: preferredBatchData };
          }

          return fetch(`${server.url}/${hashHex}.bin`, {
            signal,
          });
        })
        .then(async (responseOrResult) => {
          if ('serverUrl' in responseOrResult) {
            return responseOrResult;
          }
          const response = responseOrResult;
          const elapsedMs = Math.max(1, Date.now() - startedAt);
          if (response.ok) {
            const data = new Uint8Array(await response.arrayBuffer());
            const computed = await sha256(data);
            if (toHex(computed) === hashHex) {
              this.log({ operation: 'get', server: server.url, hash: hashHex, success: true, bytes: data.length });
              this.recordReadSuccess(server.url, elapsedMs);
              return { serverUrl: server.url, data };
            }
            this.log({ operation: 'get', server: server.url, hash: hashHex, success: false, error: 'Hash mismatch' });
            this.recordReadFailure(server.url);
            return {
              serverUrl: server.url,
              data: null,
              error: new Error(`Blossom server ${server.url} returned data with a hash mismatch`),
            };
          }

          const batchData = attemptedBatch ? null : await tryBatchRead();
          if (batchData) {
            this.log({ operation: 'get', server: server.url, hash: hashHex, success: true, bytes: batchData.length });
            this.recordReadSuccess(server.url, Math.max(1, Date.now() - startedAt));
            return { serverUrl: server.url, data: batchData };
          }

          if (response.status === 404) {
            this.log({ operation: 'get', server: server.url, hash: hashHex, success: false, error: '404' });
            this.recordReadMiss(server.url);
            return { serverUrl: server.url, data: null, error: batchError };
          }

          this.log({ operation: 'get', server: server.url, hash: hashHex, success: false, error: `${response.status}` });
          this.recordReadFailure(server.url);
          return {
            serverUrl: server.url,
            data: null,
            error: new Error(`Blossom server ${server.url} returned HTTP ${response.status}`),
          };
        })
        .catch(async (error) => {
          const batchData = attemptedBatch ? null : await tryBatchRead();
          if (batchData) {
            this.log({ operation: 'get', server: server.url, hash: hashHex, success: true, bytes: batchData.length });
            this.recordReadSuccess(server.url, Math.max(1, Date.now() - startedAt));
            return { serverUrl: server.url, data: batchData };
          }
          this.log({
            operation: 'get',
            server: server.url,
            hash: hashHex,
            success: false,
            error: error instanceof Error ? error.message : 'Network error',
          });
          if (this.isTimeoutError(error)) {
            this.recordReadTimeout(server.url);
          } else {
            this.recordReadFailure(server.url);
          }
          return {
            serverUrl: server.url,
            data: null,
            error: error instanceof Error ? error : new Error(String(error)),
          };
        }),
    };
  }

  private hasPendingReadRequests(requests: InFlightReadRequest[]): boolean {
    return requests.some((request) => !request.settled);
  }

  private async waitForNextReadResult(
    requests: InFlightReadRequest[],
    waitMs?: number,
  ): Promise<{
    request: InFlightReadRequest;
    serverUrl: string;
    data: Uint8Array | null;
    error?: Error;
  } | null> {
    const active = requests.filter((request) => !request.settled);
    if (active.length === 0) {
      return null;
    }
    const outcomes = active.map((request) => request.promise.then((value) => ({ request, ...value })));
    const result = waitMs === undefined
      ? await Promise.race(outcomes)
      : await Promise.race([
        new Promise<null>((resolve) => {
          setTimeout(() => resolve(null), waitMs);
        }),
        ...outcomes,
      ]);
    if (!result) {
      return null;
    }
    result.request.settled = true;
    return result;
  }

  /** Check if we should give up on a hash (too many failures) */
  private shouldGiveUpOnHash(hashHex: string): boolean {
    const attempts = this.hashAttempts.get(hashHex);
    if (!attempts) return false;
    return attempts.attempts >= MAX_HASH_ATTEMPTS;
  }

  /** Record a failed attempt for a hash */
  private recordHashFailure(hashHex: string): void {
    const existing = this.hashAttempts.get(hashHex) || { attempts: 0, lastAttempt: 0 };
    existing.attempts++;
    existing.lastAttempt = Date.now();
    this.hashAttempts.set(hashHex, existing);
  }

  /** Clear hash failure tracking on success */
  private clearHashFailure(hashHex: string): void {
    this.hashAttempts.delete(hashHex);
  }

  private log(entry: Omit<BlossomLogEntry, 'timestamp'>) {
    this.logger?.({ ...entry, timestamp: Date.now() });
  }

  /**
   * Create auth header for Blossom
   */
  private async createAuthHeader(
    method: string,
    hash: Hash,
    _contentType?: string
  ): Promise<string> {
    if (!this.signer) {
      throw new Error('Signer required for authenticated requests');
    }

    const hashHex = toHex(hash);
    const expiration = Math.floor(Date.now() / 1000) + 300; // 5 min

    const tags: string[][] = [
      ['t', method.toLowerCase()],
      ['x', hashHex],
      ['expiration', expiration.toString()],
    ];

    const event = await this.signer({
      kind: 24242,
      created_at: Math.floor(Date.now() / 1000),
      content: `${method} ${hashHex}`,
      tags,
    });

    return `Nostr ${btoa(JSON.stringify(event))}`;
  }

  async put(hash: Hash, data: Uint8Array, contentType?: string): Promise<boolean> {
    await this.acquireWriteSlot();
    try {
      return await this.doPut(hash, data, contentType);
    } finally {
      this.releaseWriteSlot();
    }
  }

  private async acquireWriteSlot(): Promise<void> {
    if (this.activeWrites < this.maxConcurrentWrites) {
      this.activeWrites += 1;
      return;
    }
    await new Promise<void>((resolve) => this.writeWaiters.push(resolve));
  }

  private releaseWriteSlot(): void {
    const next = this.writeWaiters.shift();
    if (next) {
      next();
      return;
    }
    this.activeWrites -= 1;
  }

  private async doPut(hash: Hash, data: Uint8Array, contentType?: string): Promise<boolean> {
    const hashHex = toHex(hash);

    // Check if we've given up on this hash
    if (this.shouldGiveUpOnHash(hashHex)) {
      // Silently return false - we've tried enough times
      return false;
    }

    // Verify hash matches data
    const computed = await sha256(data);
    if (toHex(computed) !== hashHex) {
      throw new Error('Hash does not match data');
    }

    const allWriteServers = this.servers.filter(s => s.write);
    if (allWriteServers.length === 0) {
      throw new Error('No write-enabled server configured');
    }

    // Filter to write-enabled servers not in backoff
    let writeServers = allWriteServers.filter(s => !this.isServerInBackoff(s.url));
    if (writeServers.length === 0) {
      const now = Date.now();
      const retryDelayMs = Math.min(
        ...allWriteServers.map(s => this.serverBackoffRemainingMs(s.url, now)).filter(ms => ms > 0)
      );
      if (Number.isFinite(retryDelayMs) && retryDelayMs > 0) {
        await sleep(Math.min(retryDelayMs, MAX_WRITE_BACKOFF_WAIT_MS));
        writeServers = allWriteServers.filter(s => !this.isServerInBackoff(s.url));
      }
    }
    if (writeServers.length === 0) {
      this.recordHashFailure(hashHex);
      throw new Error('All write servers are in backoff');
    }

    // For large blobs, check if they already exist on write servers before uploading
    // Only check write servers - we want to ensure data is on servers we control
    if (!this.skipExistenceCheck && data.length >= EXISTENCE_CHECK_THRESHOLD) {
      const existsOnWriteServer = await this.hasOnWriteServers(hash);
      if (existsOnWriteServer) {
        this.log({ operation: 'put', server: 'all', hash: hashHex, success: true, bytes: 0 });
        // Notify progress callback that all servers skipped (already exists)
        if (this.onUploadProgress) {
          for (const server of writeServers) {
            this.onUploadProgress(server.url, 'skipped');
          }
        }
        return false; // Already exists on write server, skip upload
      }
    }

    const authHeader = await this.createAuthHeader('upload', hash, contentType);

    // Upload to all available write-enabled servers in parallel, succeed if any succeeds
    const results = await Promise.allSettled(
      writeServers.map(async (server) => {
        try {
          const response = await fetch(`${server.url}/upload`, {
            method: 'PUT',
            signal: AbortSignal.timeout(this.putTimeoutMs),
            headers: {
              'Authorization': authHeader,
              'Content-Type': contentType || 'application/octet-stream',
              'X-SHA-256': hashHex,
            },
            body: new Blob([data as BlobPart]),
          });

          if (!response.ok && response.status !== 409) {
            const text = await response.text();
            const error = `${response.status} ${text}`;
            this.log({ operation: 'put', server: server.url, hash: hashHex, success: false, error });
            this.recordError(server.url);
            this.onUploadProgress?.(server.url, 'failed');
            throw new Error(`${server.url}: ${error}`);
          }

          // 200 (BUD-02) means already exists; 409 is accepted for older servers.
          const alreadyExisted = response.status === 200 || response.status === 409;

          // Verify blossom received the correct data by checking returned hash
          if (response.status !== 409) {
            try {
              const result = await response.json();
              if (result.sha256 && result.sha256 !== hashHex) {
                const error = `Hash mismatch: sent ${hashHex}, server got ${result.sha256}`;
                this.log({ operation: 'put', server: server.url, hash: hashHex, success: false, error });
                this.recordError(server.url);
                this.onUploadProgress?.(server.url, 'failed');
                throw new Error(`${server.url}: ${error}`);
              }
            } catch (e) {
              // JSON parse error is fine - some servers may not return JSON
              if (e instanceof SyntaxError) {
                // Ignore JSON parse errors
              } else {
                throw e;
              }
            }
          }

          this.log({ operation: 'put', server: server.url, hash: hashHex, success: true, bytes: data.length });
          this.recordSuccess(server.url);
          this.onUploadProgress?.(server.url, alreadyExisted ? 'skipped' : 'uploaded');
          return !alreadyExisted; // true if new, false if already existed
        } catch (e) {
          const error = e instanceof Error ? e.message : String(e);
          if (!error.includes(server.url)) { // Don't double-log
            this.log({ operation: 'put', server: server.url, hash: hashHex, success: false, error });
            this.recordError(server.url);
            this.onUploadProgress?.(server.url, 'failed');
          }
          throw e;
        }
      })
    );

    // Check if any succeeded
    const successes = results.filter(r => r.status === 'fulfilled');
    if (successes.length === 0) {
      // All failed - record hash failure and report first error
      this.recordHashFailure(hashHex);
      const firstError = results.find(r => r.status === 'rejected') as PromiseRejectedResult;
      throw new Error(`Blossom upload failed: ${firstError.reason}`);
    }

    // Success - clear any previous failure tracking for this hash
    this.clearHashFailure(hashHex);

    // Return true if any server stored it as new (not already existed)
    return successes.some(r => (r as PromiseFulfilledResult<boolean>).value);
  }

  async get(hash: Hash): Promise<Uint8Array | null> {
    return await this.getFromServers(hash, this.getReadServers().map((server) => server.url));
  }

  async getFromServers(hash: Hash, serverUrls: readonly string[]): Promise<Uint8Array | null> {
    const requested = new Set(serverUrls.map((serverUrl) => `${serverUrl}`.trim()).filter(Boolean));
    if (requested.size === 0) {
      return null;
    }

    const hashHex = toHex(hash);
    const readServers = this.getReadServers().filter((server) => {
      if (!requested.has(server.url)) return false;
      if (this.isServerInBackoff(server.url)) return false;
      return true;
    });

    if (readServers.length === 0) {
      return null;
    }

    const orderedServers = this.orderedReadServers(readServers);
    const requests: InFlightReadRequest[] = [];
    const errors: Error[] = [];
    let nextServerIndex = 0;

    const launchNext = (count: number): void => {
      for (let launched = 0; launched < count && nextServerIndex < orderedServers.length; launched += 1) {
        const server = orderedServers[nextServerIndex];
        nextServerIndex += 1;
        requests.push(this.createInFlightReadRequest(server, hashHex));
      }
    };

    launchNext(this.shouldProbeMultipleReadServers(orderedServers) ? Math.min(2, orderedServers.length) : 1);

    while (this.hasPendingReadRequests(requests) || nextServerIndex < orderedServers.length) {
      const result = await this.waitForNextReadResult(
        requests,
        nextServerIndex < orderedServers.length ? GET_HEDGE_INTERVAL_MS : undefined,
      );
      if (result?.data) {
        return result.data;
      }
      if (result?.error) {
        errors.push(result.error);
      }
      if (result === null && nextServerIndex < orderedServers.length) {
        launchNext(1);
        continue;
      }
      if (nextServerIndex < orderedServers.length && !this.hasPendingReadRequests(requests)) {
        launchNext(1);
      }
    }

    if (errors.length > 0) {
      throw new AggregateError(errors, `Blossom availability is uncertain: ${errors[0].message}`);
    }
    return null;
  }

  async has(hash: Hash): Promise<boolean> {
    const hashHex = toHex(hash);

    for (const server of this.servers) {
      // Skip write-only servers (read defaults to true if not specified)
      if (server.read === false) {
        continue;
      }
      // Skip servers in backoff
      if (this.isServerInBackoff(server.url)) {
        continue;
      }

      try {
        const response = await fetch(`${server.url}/${hashHex}.bin`, {
          method: 'HEAD',
          signal: AbortSignal.timeout(HEAD_TIMEOUT_MS),
        });
        if (response.ok) {
          this.log({ operation: 'has', server: server.url, hash: hashHex, success: true });
          this.recordSuccess(server.url);
          return true;
        }
        // 404 is expected, not an error - don't backoff
        // Other errors trigger backoff
        if (response.status !== 404 && response.status >= 500) {
          this.recordError(server.url);
        }
      } catch (e) {
        this.log({ operation: 'has', server: server.url, hash: hashHex, success: false, error: e instanceof Error ? e.message : 'Network error' });
        this.recordError(server.url);
        continue;
      }
    }

    return false;
  }

  /**
   * Check if hash exists on write-enabled servers only
   * Used before upload to avoid skipping uploads based on read-only server existence
   */
  private async hasOnWriteServers(hash: Hash): Promise<boolean> {
    const hashHex = toHex(hash);

    for (const server of this.servers) {
      // Only check write-enabled servers
      if (!server.write) {
        continue;
      }
      // Skip servers in backoff
      if (this.isServerInBackoff(server.url)) {
        continue;
      }

      try {
        const response = await fetch(`${server.url}/${hashHex}.bin`, {
          method: 'HEAD',
          signal: AbortSignal.timeout(HEAD_TIMEOUT_MS),
        });
        if (response.ok) {
          this.log({ operation: 'has', server: server.url, hash: hashHex, success: true });
          this.recordSuccess(server.url);
          return true;
        }
        // 404 is expected, not an error - don't backoff
        // Other errors trigger backoff
        if (response.status !== 404 && response.status >= 500) {
          this.recordError(server.url);
        }
      } catch (e) {
        this.log({ operation: 'has', server: server.url, hash: hashHex, success: false, error: e instanceof Error ? e.message : 'Network error' });
        this.recordError(server.url);
        continue;
      }
    }

    return false;
  }

  async delete(hash: Hash): Promise<boolean> {
    const writeServer = this.servers.find(s => s.write);
    if (!writeServer) {
      throw new Error('No write-enabled server configured');
    }

    const authHeader = await this.createAuthHeader('delete', hash);
    const hashHex = toHex(hash);

    const response = await fetch(`${writeServer.url}/${hashHex}.bin`, {
      method: 'DELETE',
      headers: {
        'Authorization': authHeader,
      },
    });

    if (!response.ok) {
      if (response.status === 404) {
        return false;
      }
      const text = await response.text();
      throw new Error(`Blossom delete failed: ${response.status} ${text}`);
    }

    return true;
  }
}
