import type { CID, Store } from '@hashtree/core';
import type { SearchOptions } from '@hashtree/index';
export type { CID, Store, SearchOptions };
export interface SerializedCid {
    hash: string;
    key?: string;
}
export interface CollectionSearchIndexOptions {
    order?: number;
    stopWords?: string[];
    minKeywordLength?: number;
}
export type CollectionWriteContext = Record<string, unknown>;
export interface CollectionEntryContext {
    id: string;
    cid: CID | null;
    writeContext?: CollectionWriteContext;
}
export interface CollectionKeyIndexDefinition<T> {
    name: string;
    keys: (item: T) => Iterable<string> | string;
}
export interface CollectionSearchEntry {
    text: Iterable<string> | string;
    id?: string;
    cid?: CID | null;
    prefix?: string;
}
export interface CollectionSearchTermContext {
    parseKeywords: (text: string) => string[];
}
export interface CollectionSearchIndexDefinition<T> {
    name: string;
    rootName?: string;
    prefix?: string;
    text?: (item: T) => Iterable<string> | string;
    entries?: (item: T, context: CollectionEntryContext) => Iterable<CollectionSearchEntry> | CollectionSearchEntry;
    terms?: (text: string, context: CollectionSearchTermContext) => Iterable<string> | string;
    options?: CollectionSearchIndexOptions;
}
export interface CollectionSourceQueryIndexDefinition {
    name: string;
    terms?: (text: string, context: CollectionSearchTermContext) => Iterable<string> | string;
}
export interface CollectionSourceQueryDefinition {
    searchIndexes?: CollectionSourceQueryIndexDefinition[];
}
export interface CollectionSchema<T> {
    version: number;
    defaults?: Partial<T> | (() => Partial<T>);
    migrate?: (value: unknown, fromVersion: number) => T;
    normalize?: (value: T) => T;
    validate?: (value: T) => void;
}
export interface CollectionPublishedSchema {
    itemFormat?: string;
    projectionFormat?: string;
    schemaRef?: SerializedCid | null;
}
export interface CollectionDefinition<T> {
    sourceId: string;
    schemaVersion?: number;
    schema?: CollectionSchema<T>;
    publishedSchema?: CollectionPublishedSchema;
    getId: (item: T) => string;
    keyIndexes?: CollectionKeyIndexDefinition<T>[];
    searchIndexes?: CollectionSearchIndexDefinition<T>[];
}
export interface CollectionState {
    byIdRoot: CID | null;
    keyRoots: Record<string, CID | null>;
    searchRoots: Record<string, CID | null>;
    itemCount: number;
    updatedAt: number;
}
export interface CollectionKeyManifestIndex {
    kind: 'key';
    root: SerializedCid | null;
}
export interface CollectionSearchManifestIndex {
    kind: 'search';
    root: SerializedCid | null;
    prefix: string;
    options?: CollectionSearchIndexOptions;
}
export type CollectionManifestIndex = CollectionKeyManifestIndex | CollectionSearchManifestIndex;
export interface CollectionManifestMetadata {
    version: 1;
    schemaVersion: number;
    publishedSchema?: CollectionPublishedSchema;
}
export type CollectionRootMetadata = CollectionManifestMetadata;
export interface CollectionManifest {
    version: 1;
    sourceId: string;
    schemaVersion: number;
    updatedAt: number;
    itemCount: number;
    byIdRoot: SerializedCid | null;
    indexes: Record<string, CollectionManifestIndex>;
    publishedSchema?: CollectionPublishedSchema;
    metadata?: Record<string, unknown>;
}
export interface CollectionPutMutation<T> {
    type: 'put';
    item: T;
    cid: CID;
    previous?: T;
    context?: CollectionWriteContext;
    previousContext?: CollectionWriteContext;
}
export interface CollectionDeleteMutation<T> {
    type: 'delete';
    item: T;
    context?: CollectionWriteContext;
}
export type CollectionMutation<T> = CollectionPutMutation<T> | CollectionDeleteMutation<T>;
export interface CollectionReindexEntry<T> {
    item: T;
    cid: CID;
    context?: CollectionWriteContext;
}
export type CollectionReindexEntries<T> = Iterable<CollectionReindexEntry<T>> | AsyncIterable<CollectionReindexEntry<T>>;
export interface CollectionIndexLinkResult {
    key: string;
    cid: CID;
}
export interface FederatedCollectionSource {
    manifest: CollectionManifest;
    boost?: number;
}
export interface FederatedSearchSourceHit {
    sourceId: string;
    cid: CID;
    score: number;
    boost: number;
}
export interface FederatedSearchHit {
    id: string;
    cid: CID;
    score: number;
    bestSourceId: string;
    sourceIds: string[];
    hits: FederatedSearchSourceHit[];
}
export interface FederatedSearchOptions extends SearchOptions {
    perSourceLimit?: number;
}
//# sourceMappingURL=types.d.ts.map