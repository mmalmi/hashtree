import { type SearchLinkResult } from '@hashtree/index';
import type { CID, CollectionIndexLinkResult, CollectionManifest, CollectionSourceQueryDefinition, CollectionSearchManifestIndex, SearchOptions, Store } from './types.js';
type QueryOptions = {
    prefix?: string;
    limit?: number;
};
export declare class CollectionSource {
    readonly manifest: CollectionManifest;
    private readonly linkIndex;
    private readonly byIdRoot;
    private readonly searchIndexes;
    private readonly searchDefinitions;
    constructor(store: Store, manifest: CollectionManifest, definition?: CollectionSourceQueryDefinition | null);
    get(id: string): Promise<CID | null>;
    getIndexLink(indexName: string, key: string): Promise<CID | null>;
    count(): Promise<number>;
    countReported(): Promise<number | null>;
    exactCount(): Promise<number>;
    sampleById(limit: number, random?: () => number): Promise<CollectionIndexLinkResult[]>;
    queryById(options?: QueryOptions): Promise<CollectionIndexLinkResult[]>;
    streamQueryById(options?: QueryOptions): AsyncGenerator<CollectionIndexLinkResult>;
    search(indexName: string, query: string, options?: SearchOptions): Promise<SearchLinkResult[]>;
    searchTerms(indexName: string, terms: Iterable<string>, options?: SearchOptions): Promise<SearchLinkResult[]>;
    parseSearchTerms(indexName: string, query: string): string[];
    queryIndex(indexName: string, options?: QueryOptions): Promise<CollectionIndexLinkResult[]>;
    streamQueryIndex(indexName: string, options?: QueryOptions): AsyncGenerator<CollectionIndexLinkResult>;
    private streamLinks;
    getSearchManifestIndex(indexName: string): CollectionSearchManifestIndex | null;
}
export {};
//# sourceMappingURL=source.d.ts.map