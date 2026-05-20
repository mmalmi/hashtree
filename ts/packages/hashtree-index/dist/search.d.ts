import type { CID, Store } from '@hashtree/core';
export interface SearchIndexOptions {
    /** Max entries per B-Tree node. Default: 64 */
    order?: number;
    /** Words to exclude from indexing. Default: common English stop words */
    stopWords?: Set<string>;
    /** Minimum keyword length. Default: 2 */
    minKeywordLength?: number;
}
export interface SearchResult {
    id: string;
    value: string;
    score: number;
    exactMatches?: number;
    prefixDistance?: number;
}
export interface SearchLinkResult {
    id: string;
    cid: CID;
    score: number;
    exactMatches?: number;
    prefixDistance?: number;
}
export interface SearchOptions {
    /** Max results to return. Default: 20 */
    limit?: number;
    /** Require full keyword match vs prefix match. Default: false (prefix) */
    fullMatch?: boolean;
    /** Max prefix-scan results to inspect per query term. Default: limit * 2 */
    scanLimit?: number;
}
export declare class SearchIndex {
    private btree;
    private stopWords;
    private minKeywordLength;
    constructor(store: Store, options?: SearchIndexOptions);
    /**
     * Parse text into searchable keywords.
     * Filters stop words, short words, and pure numbers (except 4-digit years).
     */
    parseKeywords(text: string): string[];
    private expandKeywordVariants;
    /**
     * Check if word is a pure number (excluding 4-digit years 1900-2099)
     */
    private isPureNumber;
    /**
     * Index an item under multiple terms.
     *
     * @param root Current index root (null for new index)
     * @param prefix Namespace prefix (e.g., "v:" for videos, "u:" for users)
     * @param terms Search terms to index under
     * @param id Unique identifier for deduplication
     * @param value JSON-serialized value to store
     * @returns New index root CID
     */
    index(root: CID | null, prefix: string, terms: string[], id: string, value: string): Promise<CID>;
    /**
     * Search for items matching query terms.
     * Returns results sorted by score (number of matching terms) then by id.
     *
     * @param root Index root CID
     * @param prefix Namespace prefix to search within
     * @param query Search query text
     * @param options Search options (limit, fullMatch)
     */
    search(root: CID | null, prefix: string, query: string, options?: SearchOptions): Promise<SearchResult[]>;
    /**
     * Search for items using caller-supplied normalized terms.
     * This is useful when the app wants custom term expansion without reimplementing ranking.
     */
    searchTerms(root: CID | null, prefix: string, terms: Iterable<string>, options?: SearchOptions): Promise<SearchResult[]>;
    /**
     * Remove an item from the index.
     * Must provide the same terms it was indexed under.
     */
    remove(root: CID, prefix: string, terms: string[], id: string): Promise<CID | null>;
    /**
     * Merge two search index roots.
     * @param preferOther - If true, prefer other's values on conflict (e.g., other is from newer event)
     */
    merge(base: CID | null, other: CID | null, preferOther?: boolean): Promise<CID | null>;
    build(items: Iterable<[string, string]>): Promise<CID | null>;
    /**
     * Index an item with a CID link instead of string value.
     * Uses natural deduplication - same id will overwrite previous CID.
     *
     * @param root Current index root (null for new index)
     * @param prefix Namespace prefix (e.g., "v:" for videos)
     * @param terms Search terms to index under
     * @param id Unique identifier for deduplication (e.g., "pubkey:treeName")
     * @param targetCid CID to link to (e.g., video directory CID)
     * @returns New index root CID
     */
    indexLink(root: CID | null, prefix: string, terms: string[], id: string, targetCid: CID): Promise<CID>;
    buildLinks(items: Iterable<[string, CID]>): Promise<CID | null>;
    /**
     * Search for CID links matching query terms.
     * Returns results sorted by score (number of matching terms).
     *
     * @param root Index root CID
     * @param prefix Namespace prefix to search within
     * @param query Search query text
     * @param options Search options (limit, fullMatch)
     */
    searchLinks(root: CID | null, prefix: string, query: string, options?: SearchOptions): Promise<SearchLinkResult[]>;
    /**
     * Search for CID links using caller-supplied normalized terms.
     */
    searchLinkTerms(root: CID | null, prefix: string, terms: Iterable<string>, options?: SearchOptions): Promise<SearchLinkResult[]>;
    /**
     * Merge two search index roots with CID links.
     * @param preferOther - If true, prefer other's CIDs on conflict
     */
    mergeLinks(base: CID | null, other: CID | null, preferOther?: boolean): Promise<CID | null>;
}
//# sourceMappingURL=search.d.ts.map