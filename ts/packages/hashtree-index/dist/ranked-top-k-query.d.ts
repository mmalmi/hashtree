import { type CID, type HashTree } from '@hashtree/core';
import { BTree } from './btree.js';
import { type RankedTopKManifest } from './ranked-top-k.js';
import type { ParsedRankedQuery, RankedSearchResult, RankedSearchSegmentManifest } from './ranked-types.js';
type ScoredCandidate = Omit<RankedSearchResult, 'value'>;
export declare function queryRankedTopK(options: {
    btree: BTree;
    tree: HashTree;
    topKRoots: CID;
    topKManifest: RankedTopKManifest;
    postingsRoot: CID;
    documentsRoot: CID;
    parsed: ParsedRankedQuery;
    localFrequencies: ReadonlyMap<string, number>;
    localDocumentFrequencies: ReadonlyMap<string, number>;
    frequencies: ReadonlyMap<string, number>;
    fields: ReadonlyMap<string, RankedSearchSegmentManifest['fields'][number]>;
    selectedFields: ReadonlySet<string>;
    manifest: RankedSearchSegmentManifest;
    corpusDocuments: number;
    k1: number;
    operator: 'or' | 'and';
    limit: number;
}): Promise<ScoredCandidate[]>;
export {};
//# sourceMappingURL=ranked-top-k-query.d.ts.map