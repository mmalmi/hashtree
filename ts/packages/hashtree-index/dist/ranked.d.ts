import { type CID, type Store } from '@hashtree/core';
import type { RankedSearchBuildOptions, RankedSearchDocument, RankedSearchIndexOptions, RankedSearchOptions, RankedSearchResult, RankedSearchSegmentManifest } from './ranked-types.js';
export { RANKED_SEARCH_SEGMENT_FORMAT } from './ranked-schema.js';
export declare class RankedSearchIndex {
    private readonly btree;
    private readonly tree;
    constructor(store: Store, options?: RankedSearchIndexOptions);
    buildSegment(documents: Iterable<RankedSearchDocument>, options: RankedSearchBuildOptions): Promise<CID>;
    readManifest(root: CID): Promise<RankedSearchSegmentManifest>;
    search(root: CID, query: string, options?: RankedSearchOptions): Promise<RankedSearchResult[]>;
}
//# sourceMappingURL=ranked.d.ts.map