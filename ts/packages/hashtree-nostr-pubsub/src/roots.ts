import type { CID, Store } from '@hashtree/core';
import { cloneNostrFilter, type NostrFilter } from './nostrTypes.js';
import type {
  HashtreeNostrRootEntry,
  HashtreeNostrRootProvider,
  HashtreeNostrRoots,
} from './types.js';
import { QueryCancellation } from './cancellation.js';

export interface RootReplicaSnapshot {
  partitionId: string;
  replicaId: string;
  root: CID | null;
  store: Store;
}

export function normalizeRootProvider(roots: HashtreeNostrRoots): HashtreeNostrRootProvider {
  if (isRootProvider(roots)) return roots;
  const entries: readonly HashtreeNostrRootEntry[] = Array.isArray(roots)
    ? roots
    : [{ partitionId: 'default', replicaId: 'default', root: roots as CID | null }];
  return { snapshot: () => entries };
}

export async function snapshotRoots(
  provider: HashtreeNostrRootProvider,
  defaultStore: Store,
  filters: readonly NostrFilter[],
  cancellation: QueryCancellation,
): Promise<RootReplicaSnapshot[]> {
  const provided = await cancellation.wait(Promise.resolve(provider.snapshot({
    filters: filters.map(cloneNostrFilter),
    signal: cancellation.signal,
    deadline: cancellation.deadline,
  })));
  cancellation.throwIfCancelled();

  if (!Array.isArray(provided)) {
    throw new TypeError('Hashtree Nostr root provider must return an array');
  }

  const seen = new Set<string>();
  return provided.map((entry, index) => {
    const partitionId = nonEmptyId(entry.partitionId, 'partitionId');
    const replicaId = entry.replicaId === undefined
      ? `replica-${index}`
      : nonEmptyId(entry.replicaId, 'replicaId');
    const identity = `${partitionId}\u0000${replicaId}`;
    if (seen.has(identity)) {
      throw new TypeError(`Duplicate Hashtree Nostr replica identity ${partitionId}/${replicaId}`);
    }
    seen.add(identity);

    return {
      partitionId,
      replicaId,
      root: cloneCid(entry.root),
      store: entry.store ?? defaultStore,
    };
  });
}

function isRootProvider(roots: HashtreeNostrRoots): roots is HashtreeNostrRootProvider {
  return roots !== null
    && !Array.isArray(roots)
    && typeof roots === 'object'
    && 'snapshot' in roots
    && typeof roots.snapshot === 'function';
}

function cloneCid(root: CID | null): CID | null {
  if (root === null) return null;
  if (!(root.hash instanceof Uint8Array) || root.hash.length !== 32) {
    throw new TypeError('Hashtree Nostr root hash must be 32 bytes');
  }
  if (root.key !== undefined && !(root.key instanceof Uint8Array)) {
    throw new TypeError('Hashtree Nostr root key must be bytes');
  }
  return {
    hash: new Uint8Array(root.hash),
    key: root.key === undefined ? undefined : new Uint8Array(root.key),
  };
}

function nonEmptyId(value: string, label: string): string {
  if (typeof value !== 'string' || value.trim().length === 0) {
    throw new TypeError(`${label} must be a non-empty string`);
  }
  return value;
}
