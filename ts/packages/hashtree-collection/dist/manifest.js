import { deserializeCid, serializeCid } from './cid.js';
import { defaultSearchPrefix } from './helpers.js';
import { getSchemaVersion } from './schema.js';
import { HashTree } from '@hashtree/core';
export const COLLECTION_MANIFEST_METADATA_FILE = '.collection-manifest.json';
export function createEmptyCollectionState(definition) {
    return {
        byIdRoot: null,
        keyRoots: Object.fromEntries((definition.keyIndexes ?? []).map((index) => [index.name, null])),
        searchRoots: Object.fromEntries((definition.searchIndexes ?? []).map((index) => [index.name, null])),
        itemCount: 0,
        updatedAt: 0,
    };
}
export function collectionStateFromManifest(definition, manifest) {
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
export function collectionManifestFromState(definition, state, metadata) {
    const indexes = {};
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
export function collectionRootMetadataFromManifest(manifest) {
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
export function serializeCollectionManifestMetadata(metadata) {
    return new TextEncoder().encode(JSON.stringify(metadata));
}
export function parseCollectionManifestMetadata(bytes) {
    return JSON.parse(new TextDecoder().decode(bytes));
}
export async function loadCollectionManifestMetadata(store, root) {
    if (!root) {
        return null;
    }
    const tree = new HashTree({ store });
    const entry = (await tree.listDirectory(root)).find((candidate) => candidate.name === COLLECTION_MANIFEST_METADATA_FILE);
    if (!entry) {
        return null;
    }
    const bytes = await tree.readFile(entry.cid);
    if (!bytes) {
        return null;
    }
    return parseCollectionManifestMetadata(bytes);
}
//# sourceMappingURL=manifest.js.map