import type {
  CollectionDefinition,
  CollectionManifest,
  CollectionManifestMetadata,
  CollectionManifestIndex,
  CollectionState,
  CID,
  Store,
} from './types.js';
import { deserializeCid, serializeCid } from './cid.js';
import { defaultSearchPrefix } from './helpers.js';
import { getSchemaVersion } from './schema.js';
import { HashTree } from '@hashtree/core';

export const COLLECTION_MANIFEST_METADATA_FILE = '.collection-manifest.json';

export function createEmptyCollectionState<T>(definition: CollectionDefinition<T>): CollectionState {
  return {
    byIdRoot: null,
    keyRoots: Object.fromEntries((definition.keyIndexes ?? []).map((index) => [index.name, null])),
    searchRoots: Object.fromEntries((definition.searchIndexes ?? []).map((index) => [index.name, null])),
    itemCount: 0,
    updatedAt: 0,
  };
}

export function collectionStateFromManifest<T>(
  definition: CollectionDefinition<T>,
  manifest: CollectionManifest | null | undefined,
): CollectionState {
  const empty = createEmptyCollectionState(definition);
  if (!manifest) {
    return empty;
  }

  const keyRoots = { ...empty.keyRoots };
  const searchRoots = { ...empty.searchRoots };

  for (const [name, index] of Object.entries(manifest.indexes ?? {})) {
    if (index.kind === 'key' && Object.hasOwn(keyRoots, name)) {
      keyRoots[name] = deserializeCid(index.root);
    }
    if (index.kind === 'search' && Object.hasOwn(searchRoots, name)) {
      searchRoots[name] = deserializeCid(index.root);
    }
  }

  return {
    byIdRoot: deserializeCid(manifest.byIdRoot),
    keyRoots,
    searchRoots,
    itemCount: 0,
    updatedAt: Number(manifest.updatedAt) || 0,
  };
}

export function collectionManifestFromState<T>(
  definition: CollectionDefinition<T>,
  state: CollectionState,
  metadata?: Record<string, unknown>,
): CollectionManifest {
  const indexes: Record<string, CollectionManifestIndex> = {};

  for (const definitionIndex of definition.keyIndexes ?? []) {
    indexes[definitionIndex.name] = {
      kind: 'key',
      root: serializeCid(state.keyRoots[definitionIndex.name] ?? null),
    };
  }

  for (const definitionIndex of definition.searchIndexes ?? []) {
    indexes[definitionIndex.name] = {
      kind: 'search',
      root: serializeCid(state.searchRoots[definitionIndex.name] ?? null),
      prefix: definitionIndex.prefix ?? defaultSearchPrefix(definitionIndex.name),
      options: definitionIndex.options,
    };
  }

  return {
    version: 1,
    sourceId: definition.sourceId,
    schemaVersion: getSchemaVersion(definition),
    updatedAt: state.updatedAt,
    byIdRoot: serializeCid(state.byIdRoot),
    indexes,
    publishedSchema: definition.publishedSchema,
    metadata,
  };
}

export function collectionRootMetadataFromManifest(
  manifest: CollectionManifest,
): CollectionManifestMetadata | null {
  if (manifest.schemaVersion === 1 && !manifest.publishedSchema) {
    return null;
  }

  return {
    version: 1,
    schemaVersion: manifest.schemaVersion,
    publishedSchema: manifest.publishedSchema,
  };
}

export const collectionManifestMetadataFromManifest = collectionRootMetadataFromManifest;

export function serializeCollectionManifestMetadata(metadata: CollectionManifestMetadata): Uint8Array {
  return new TextEncoder().encode(JSON.stringify(metadata));
}

export function parseCollectionManifestMetadata(bytes: Uint8Array): CollectionManifestMetadata {
  return JSON.parse(new TextDecoder().decode(bytes)) as CollectionManifestMetadata;
}

export async function loadCollectionManifestMetadata(
  store: Store,
  root: CID | null | undefined,
): Promise<CollectionManifestMetadata | null> {
  if (!root) {
    return null;
  }

  const tree = new HashTree({ store });
  const entry = (await tree.listDirectory(root)).find(
    (candidate) => candidate.name === COLLECTION_MANIFEST_METADATA_FILE,
  );
  if (!entry) {
    return null;
  }

  const bytes = await tree.readFile(entry.cid);
  if (!bytes) {
    return null;
  }

  return parseCollectionManifestMetadata(bytes);
}
