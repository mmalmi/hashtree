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
}

/** Server health tracking for backoff */
interface ServerHealth {
  lastErrorTime: number;
  consecutiveErrors: number;
}

/** Backoff config */
const BASE_BACKOFF_MS = 1000; // 1 second
const MAX_BACKOFF_MS = 60000; // 1 minute
const MAX_HASH_ATTEMPTS = 4; // Give up after this many attempts per hash

/** Size threshold for existence check before upload (256KB) */
const EXISTENCE_CHECK_THRESHOLD = 256 * 1024;

/** Timeout for HEAD requests (5 seconds) */
const HEAD_TIMEOUT_MS = 5000;

/** Per-hash failure tracking */
interface HashAttempts {
  attempts: number;
  lastAttempt: number;
}

export class BlossomStore implements StoreWithMeta {
  private servers: BlossomServer[];
  private signer?: BlossomSigner;
  private logger?: BlossomLogger;
  private onUploadProgress?: BlossomUploadCallback;
  private serverHealth: Map<string, ServerHealth> = new Map();
  private hashAttempts: Map<string, HashAttempts> = new Map();
  private writeQueue: Promise<boolean> = Promise.resolve(true);

  constructor(config: BlossomStoreConfig) {
    this.servers = config.servers.map(s =>
      typeof s === 'string' ? { url: s, write: false } : s
    );
    this.signer = config.signer;
    this.logger = config.logger;
    this.onUploadProgress = config.onUploadProgress;
  }

  /** Get list of write-enabled server URLs */
  getWriteServers(): string[] {
    return this.servers.filter(s => s.write).map(s => s.url);
  }

  /** Check if server is in backoff period */
  private isServerInBackoff(serverUrl: string): boolean {
    const health = this.serverHealth.get(serverUrl);
    if (!health || health.consecutiveErrors === 0) return false;

    const backoffMs = Math.min(
      BASE_BACKOFF_MS * Math.pow(2, health.consecutiveErrors - 1),
      MAX_BACKOFF_MS
    );
    return Date.now() - health.lastErrorTime < backoffMs;
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
    // Queue writes sequentially to avoid overwhelming servers
    const result = this.writeQueue.then(
      () => this.doPut(hash, data, contentType),
      () => this.doPut(hash, data, contentType) // Continue even if previous failed
    );
    this.writeQueue = result.then(() => true, () => true); // Keep queue going
    return result;
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

    // Filter to write-enabled servers not in backoff
    const writeServers = this.servers.filter(s => s.write && !this.isServerInBackoff(s.url));
    if (writeServers.length === 0) {
      // Check if we have any write servers at all
      const anyWriteServers = this.servers.filter(s => s.write);
      if (anyWriteServers.length === 0) {
        throw new Error('No write-enabled server configured');
      }
      // All servers in backoff - count as an attempt
      this.recordHashFailure(hashHex);
      throw new Error('All write servers are in backoff');
    }

    // For large blobs, check if they already exist on write servers before uploading
    // Only check write servers - we want to ensure data is on servers we control
    if (data.length >= EXISTENCE_CHECK_THRESHOLD) {
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

          // 409 means already exists - count as skipped
          const alreadyExisted = response.status === 409;

          // Verify blossom received the correct data by checking returned hash
          if (!alreadyExisted) {
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
    const hashHex = toHex(hash);

    // Try each read-enabled server until success, respecting backoff
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
        const response = await fetch(`${server.url}/${hashHex}.bin`);
        if (response.ok) {
          const data = new Uint8Array(await response.arrayBuffer());
          // Verify hash
          const computed = await sha256(data);
          if (toHex(computed) === hashHex) {
            this.log({ operation: 'get', server: server.url, hash: hashHex, success: true, bytes: data.length });
            this.recordSuccess(server.url);
            return data;
          }
          this.log({ operation: 'get', server: server.url, hash: hashHex, success: false, error: 'Hash mismatch' });
          this.recordError(server.url);
        } else if (response.status === 404) {
          // 404 is not an error - blob just doesn't exist on this server
          this.log({ operation: 'get', server: server.url, hash: hashHex, success: false, error: '404' });
        } else {
          // Other errors trigger backoff
          this.log({ operation: 'get', server: server.url, hash: hashHex, success: false, error: `${response.status}` });
          this.recordError(server.url);
        }
      } catch (e) {
        this.log({ operation: 'get', server: server.url, hash: hashHex, success: false, error: e instanceof Error ? e.message : 'Network error' });
        this.recordError(server.url);
        continue;
      }
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
