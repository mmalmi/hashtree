import { toHex, type Hash, type Store } from '@hashtree/core';
import { QueryCancellation } from './cancellation.js';
import { HashtreeNostrReplicaUnavailableError } from './types.js';

/** Bounds reads and turns an absent content-addressed block into replica failure. */
export class GuardedReplicaStore implements Store {
  constructor(
    private readonly store: Store,
    private readonly cancellation: QueryCancellation,
  ) {}

  async get(hash: Hash): Promise<Uint8Array> {
    const value = await this.run(() => this.store.get(hash), 'read');
    if (!value) {
      throw new HashtreeNostrReplicaUnavailableError(
        `Hashtree replica is missing block ${toHex(hash)}`,
      );
    }
    return value;
  }

  async has(hash: Hash): Promise<boolean> {
    return await this.run(() => this.store.has(hash), 'check');
  }

  async put(hash: Hash, data: Uint8Array): Promise<boolean> {
    return await this.run(() => this.store.put(hash, data), 'write');
  }

  async delete(hash: Hash): Promise<boolean> {
    return await this.run(() => this.store.delete(hash), 'delete');
  }

  private async run<T>(operation: () => Promise<T>, verb: string): Promise<T> {
    try {
      return await this.cancellation.wait(operation());
    } catch (error) {
      this.cancellation.throwIfCancelled();
      if (error instanceof HashtreeNostrReplicaUnavailableError) {
        throw error;
      }
      throw new HashtreeNostrReplicaUnavailableError(
        `Unable to ${verb} a Hashtree replica block`,
        { cause: error },
      );
    }
  }
}
