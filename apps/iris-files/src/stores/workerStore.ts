/**
 * WorkerStore - Store implementation that proxies to the hashtree worker
 *
 * This implements the Store interface, allowing HashTree to use worker-based
 * storage without knowing about the worker details.
 */

import type { Store, Hash } from 'hashtree';
import { getWorkerAdapter } from '../lib/workerInit';

/**
 * Store implementation that proxies all operations to the hashtree worker.
 * Falls back to returning null/false if worker is not ready.
 */
export class WorkerStore implements Store {
  async put(hash: Hash, data: Uint8Array): Promise<boolean> {
    const adapter = getWorkerAdapter();
    if (!adapter) {
      console.warn('[WorkerStore] Worker not ready, put failed');
      return false;
    }
    return adapter.put(hash, data);
  }

  async get(hash: Hash): Promise<Uint8Array | null> {
    const adapter = getWorkerAdapter();
    if (!adapter) {
      console.warn('[WorkerStore] Worker not ready, get failed');
      return null;
    }
    return adapter.get(hash);
  }

  async has(hash: Hash): Promise<boolean> {
    const adapter = getWorkerAdapter();
    if (!adapter) {
      return false;
    }
    return adapter.has(hash);
  }

  async delete(hash: Hash): Promise<boolean> {
    const adapter = getWorkerAdapter();
    if (!adapter) {
      return false;
    }
    return adapter.delete(hash);
  }
}

// Singleton instance
let instance: WorkerStore | null = null;

export function getWorkerStore(): WorkerStore {
  if (!instance) {
    instance = new WorkerStore();
  }
  return instance;
}
