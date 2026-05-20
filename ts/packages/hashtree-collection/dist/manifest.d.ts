import type { CollectionDefinition, CollectionManifest, CollectionManifestMetadata, CollectionState, CID, Store } from './types.js';
export declare const COLLECTION_MANIFEST_METADATA_FILE = ".collection-manifest.json";
export declare function createEmptyCollectionState<T>(definition: CollectionDefinition<T>): CollectionState;
export declare function collectionStateFromManifest<T>(definition: CollectionDefinition<T>, manifest: CollectionManifest | null | undefined): CollectionState;
export declare function collectionManifestFromState<T>(definition: CollectionDefinition<T>, state: CollectionState, metadata?: Record<string, unknown>): CollectionManifest;
export declare function collectionRootMetadataFromManifest(manifest: CollectionManifest): CollectionManifestMetadata | null;
export declare const collectionManifestMetadataFromManifest: typeof collectionRootMetadataFromManifest;
export declare function serializeCollectionManifestMetadata(metadata: CollectionManifestMetadata): Uint8Array;
export declare function parseCollectionManifestMetadata(bytes: Uint8Array): CollectionManifestMetadata;
export declare function loadCollectionManifestMetadata(store: Store, root: CID | null | undefined): Promise<CollectionManifestMetadata | null>;
//# sourceMappingURL=manifest.d.ts.map