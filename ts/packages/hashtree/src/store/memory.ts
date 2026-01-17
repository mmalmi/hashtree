/**
 * In-memory content-addressed store
 * Useful for testing and temporary data
 */

import { Store, Hash, toHex } from '../types.js';

export class MemoryStore implements Store {
  private data = new Map<string, Uint8Array>();

  async put(hash: Hash, data: Uint8Array): Promise<boolean> {
    const key = toHex(hash);
    if (this.data.has(key)) {
      return false;
    }
    // Store a copy to prevent external mutation
    this.data.set(key, new Uint8Array(data));
    return true;
  }

  async get(hash: Hash): Promise<Uint8Array | null> {
    const key = toHex(hash);
    const data = this.data.get(key);
    if (!data) return null;
    // Return a copy to prevent external mutation
    return new Uint8Array(data);
  }

  async has(hash: Hash): Promise<boolean> {
    return this.data.has(toHex(hash));
  }

  async delete(hash: Hash): Promise<boolean> {
    return this.data.delete(toHex(hash));
  }

  /**
   * Get number of stored items
   */
  get size(): number {
    return this.data.size;
  }

  /**
   * Get total bytes stored
   */
  get totalBytes(): number {
    let total = 0;
    for (const data of this.data.values()) {
      total += data.length;
    }
    return total;
  }

  /**
   * Clear all data
   */
  clear(): void {
    this.data.clear();
  }

  /**
   * List all hashes
   */
  keys(): Hash[] {
    return Array.from(this.data.keys()).map(hex => {
      const bytes = new Uint8Array(hex.length / 2);
      for (let i = 0; i < bytes.length; i++) {
        bytes[i] = parseInt(hex.substr(i * 2, 2), 16);
      }
      return bytes;
    });
  }
}
