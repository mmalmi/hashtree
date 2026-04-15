import {
  COLLECTION_MANIFEST_METADATA_FILE,
  CollectionSource,
  CollectionWriter,
  collectionManifestMetadataFromManifest,
  deserializeCid,
  serializeCid,
  type CollectionDefinition,
  type CollectionManifest,
  type CollectionManifestIndex,
  type CollectionPublishedSchema,
} from '@hashtree/collection';
import type { Store } from '@hashtree/core';
import {
  authorKindTimeKey,
  authorTimeKey,
  getDTag,
  isParameterizedReplaceableKind,
  isReplaceableKind,
  kindTimeKey,
  MANIFEST_BY_AUTHOR_KIND_TIME,
  MANIFEST_BY_AUTHOR_TIME,
  MANIFEST_BY_ID,
  MANIFEST_BY_KIND_TIME,
  MANIFEST_BY_KIND_TIME_AUTHOR,
  MANIFEST_BY_TAG,
  MANIFEST_BY_TIME,
  MANIFEST_PARAMETERIZED_REPLACEABLE,
  MANIFEST_REPLACEABLE,
  kindTimeAuthorKey,
  parameterizedReplaceableKey,
  replaceableKey,
  tagKeys,
  timeKey,
} from './eventKeys.js';
import { validateEventShape } from './eventValidation.js';
import type { NostrEventManifest, StoredNostrEvent } from './events.js';

export const DEFAULT_NOSTR_EVENT_COLLECTION_SOURCE_ID = 'nostr-events';
export const NOSTR_EVENT_ITEM_FORMAT = 'nostr/event@1';
export const NOSTR_EVENT_PROJECTION_FORMAT = 'hashtree/nostr-event-index@1';
export { COLLECTION_MANIFEST_METADATA_FILE };

const NOSTR_EVENT_PUBLISHED_SCHEMA: CollectionPublishedSchema = {
  itemFormat: NOSTR_EVENT_ITEM_FORMAT,
  projectionFormat: NOSTR_EVENT_PROJECTION_FORMAT,
};

export function createNostrEventCollectionDefinition(
  sourceId: string = DEFAULT_NOSTR_EVENT_COLLECTION_SOURCE_ID,
): CollectionDefinition<StoredNostrEvent> {
  return {
    sourceId,
    schema: {
      version: 1,
      validate: (event) => {
        validateEventShape(event);
      },
    },
    publishedSchema: NOSTR_EVENT_PUBLISHED_SCHEMA,
    getId: (event) => event.id,
    keyIndexes: [
      {
        name: MANIFEST_BY_AUTHOR_TIME,
        keys: (event) => [authorTimeKey(event)],
      },
      {
        name: MANIFEST_BY_AUTHOR_KIND_TIME,
        keys: (event) => [authorKindTimeKey(event)],
      },
      {
        name: MANIFEST_BY_KIND_TIME,
        keys: (event) => [kindTimeKey(event)],
      },
      {
        name: MANIFEST_BY_KIND_TIME_AUTHOR,
        keys: (event) => [kindTimeAuthorKey(event)],
      },
      {
        name: MANIFEST_BY_TIME,
        keys: (event) => [timeKey(event)],
      },
      {
        name: MANIFEST_BY_TAG,
        keys: (event) => tagKeys(event),
      },
      {
        name: MANIFEST_REPLACEABLE,
        keys: (event) => (
          isReplaceableKind(event.kind)
            ? [replaceableKey(event.pubkey, event.kind)]
            : []
        ),
      },
      {
        name: MANIFEST_PARAMETERIZED_REPLACEABLE,
        keys: (event) => (
          isParameterizedReplaceableKind(event.kind)
            ? [parameterizedReplaceableKey(event.pubkey, event.kind, getDTag(event) ?? '')]
            : []
        ),
      },
    ],
  };
}

export function nostrEventManifestToCollectionManifest(
  manifest: NostrEventManifest,
  sourceId: string = DEFAULT_NOSTR_EVENT_COLLECTION_SOURCE_ID,
  itemCount = 0,
): CollectionManifest {
  const indexes: Record<string, CollectionManifestIndex> = {
    [MANIFEST_BY_AUTHOR_TIME]: { kind: 'key', root: serializeCid(manifest.byAuthorTime) },
    [MANIFEST_BY_AUTHOR_KIND_TIME]: { kind: 'key', root: serializeCid(manifest.byAuthorKindTime) },
    [MANIFEST_BY_KIND_TIME]: { kind: 'key', root: serializeCid(manifest.byKindTime) },
    [MANIFEST_BY_KIND_TIME_AUTHOR]: { kind: 'key', root: serializeCid(manifest.byKindTimeAuthor) },
    [MANIFEST_BY_TIME]: { kind: 'key', root: serializeCid(manifest.byTime) },
    [MANIFEST_BY_TAG]: { kind: 'key', root: serializeCid(manifest.byTag) },
    [MANIFEST_REPLACEABLE]: { kind: 'key', root: serializeCid(manifest.replaceable) },
    [MANIFEST_PARAMETERIZED_REPLACEABLE]: {
      kind: 'key',
      root: serializeCid(manifest.parameterizedReplaceable),
    },
  };

  return {
    version: 1,
    sourceId,
    schemaVersion: 1,
    updatedAt: 0,
    itemCount,
    byIdRoot: serializeCid(manifest.byId),
    indexes,
    publishedSchema: NOSTR_EVENT_PUBLISHED_SCHEMA,
  };
}

export function collectionManifestToNostrEventManifest(manifest: CollectionManifest): NostrEventManifest {
  const byId = deserializeCid(manifest.byIdRoot);
  const keyIndexCid = (name: string) => {
    const index = manifest.indexes[name];
    if (!index?.root) {
      return null;
    }
    return deserializeCid(index.root);
  };

  return {
    byId,
    byAuthorTime: keyIndexCid(MANIFEST_BY_AUTHOR_TIME),
    byAuthorKindTime: keyIndexCid(MANIFEST_BY_AUTHOR_KIND_TIME),
    byKindTime: keyIndexCid(MANIFEST_BY_KIND_TIME),
    byKindTimeAuthor: keyIndexCid(MANIFEST_BY_KIND_TIME_AUTHOR),
    byTime: keyIndexCid(MANIFEST_BY_TIME),
    byTag: keyIndexCid(MANIFEST_BY_TAG),
    replaceable: keyIndexCid(MANIFEST_REPLACEABLE),
    parameterizedReplaceable: keyIndexCid(MANIFEST_PARAMETERIZED_REPLACEABLE),
  };
}

export async function createNostrEventCollectionSource(
  store: Store,
  manifest: NostrEventManifest,
  sourceId: string = DEFAULT_NOSTR_EVENT_COLLECTION_SOURCE_ID,
): Promise<CollectionSource> {
  const collectionManifest = nostrEventManifestToCollectionManifest(manifest, sourceId);
  const source = new CollectionSource(store, collectionManifest);
  return new CollectionSource(store, {
    ...collectionManifest,
    itemCount: await source.count(),
  });
}

export function createNostrEventCollectionWriter(
  store: Store,
  manifest: NostrEventManifest,
  sourceId: string = DEFAULT_NOSTR_EVENT_COLLECTION_SOURCE_ID,
): CollectionWriter<StoredNostrEvent> {
  return new CollectionWriter(
    store,
    createNostrEventCollectionDefinition(sourceId),
    nostrEventManifestToCollectionManifest(manifest, sourceId),
  );
}

export function nostrEventCollectionManifestMetadata(
  manifest: NostrEventManifest,
  sourceId: string = DEFAULT_NOSTR_EVENT_COLLECTION_SOURCE_ID,
) {
  return collectionManifestMetadataFromManifest(
    nostrEventManifestToCollectionManifest(manifest, sourceId),
  );
}

export const nostrEventCollectionRootMetadata = nostrEventCollectionManifestMetadata;
