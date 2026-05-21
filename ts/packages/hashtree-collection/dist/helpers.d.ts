import { SearchIndex } from '@hashtree/index';
import type { CollectionEntryContext, CollectionDefinition, CollectionKeyIndexDefinition, CollectionSearchEntry, CollectionSearchIndexDefinition, CollectionSearchIndexOptions, Store } from './types.js';
export interface MaterializedCollectionSearchEntry {
    text: string;
    id?: string;
    cid?: CollectionSearchEntry['cid'];
    prefix?: string;
}
export declare function getSchemaVersion<T>(definition: CollectionDefinition<T>): number;
export declare function defaultSearchPrefix(name: string): string;
export declare function createSearchIndex(store: Store, options?: CollectionSearchIndexOptions): SearchIndex;
export declare function materializeSearchText<T>(definition: CollectionSearchIndexDefinition<T>, item: T): string;
export declare function materializeSearchTerms<T>(definition: CollectionSearchIndexDefinition<T>, searchIndex: SearchIndex, text: string): string[];
export declare function materializeSearchEntries<T>(definition: CollectionSearchIndexDefinition<T>, item: T, context: CollectionEntryContext): MaterializedCollectionSearchEntry[];
export declare function materializeKeyValues<T>(definition: CollectionKeyIndexDefinition<T>, item: T): string[];
export declare function readStringInput(value: Iterable<string> | string): string[];
export declare function normalizeStringInput(value: Iterable<string> | string): string;
export declare function uniqueStrings(values: string[]): string[];
//# sourceMappingURL=helpers.d.ts.map