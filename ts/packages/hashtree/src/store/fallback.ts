/**
 * FallbackStore - Store with cascading fallback sources
 *
 * Tries stores in order until data is found.
 * Caches successful remote fetches to primary store.
 *
 * Example: local (Dexie) -> WebRTC -> Blossom
 */

import type { Store, Hash } from '../types.js';

/** Minimal interface for read-only fallback sources */
export interface ReadableStore {
  get(hash: Hash): Promise<Uint8Array | null>;
}

/** Minimal interface for writable stores (optional put for fire-and-forget writes) */
export interface WritableStore extends ReadableStore {
  put?(hash: Hash, data: Uint8Array): Promise<boolean>;
}

export interface FallbackStoreConfig {
  /** Primary local store (reads/writes) */
  primary: Store;
  /** Fallback stores to try in order (read-only, writes are fire-and-forget) */
  fallbacks: WritableStore[];
  /** Timeout for fallback stores (ms) */
  timeout?: number;
}

export class FallbackStore implements Store {
  private primary: Store;
  private fallbacks: WritableStore[];
  private timeout: number;

  constructor(config: FallbackStoreConfig) {
    this.primary = config.primary;
    this.fallbacks = config.fallbacks;
    this.timeout = config.timeout ?? 5000;
  }

  async put(hash: Hash, data: Uint8Array): Promise<boolean> {
    // Write to primary
    const success = await this.primary.put(hash, data);

    // Fire-and-forget writes to fallback stores (if they support put)
    for (const store of this.fallbacks) {
      if (store.put) {
        store.put(hash, data).catch(() => {
          // Silently ignore - fallbacks are best-effort
        });
      }
    }

    return success;
  }

  async get(hash: Hash): Promise<Uint8Array | null> {
    // Try primary first
    let data = await this.primary.get(hash);
    if (data) return data;

    // Try fallbacks with timeout
    for (const store of this.fallbacks) {
      try {
        const fetchPromise = store.get(hash);
        const timeoutPromise = new Promise<null>(resolve =>
          setTimeout(() => resolve(null), this.timeout)
        );

        data = await Promise.race([fetchPromise, timeoutPromise]);

        if (data) {
          // Cache to primary
          await this.primary.put(hash, data);
          return data;
        }
      } catch {
        // Continue to next fallback
      }
    }

    return null;
  }

  async has(hash: Hash): Promise<boolean> {
    return this.primary.has(hash);
  }

  async delete(hash: Hash): Promise<boolean> {
    return this.primary.delete(hash);
  }

  /** Add a fallback store dynamically */
  addFallback(store: WritableStore): void {
    this.fallbacks.push(store);
  }

  /** Remove a fallback store */
  removeFallback(store: WritableStore): void {
    const idx = this.fallbacks.indexOf(store);
    if (idx >= 0) this.fallbacks.splice(idx, 1);
  }
}
