import { type CID } from '@hashtree/core';
import { BTree } from './btree.js';
import { type RankedCandidate } from './ranked-candidates.js';
import type { ParsedRankedQuery, RankedSearchResult, RankedSearchSegmentManifest } from './ranked-types.js';
type ScoredCandidate = Omit<RankedSearchResult, 'value'>;
export declare function scoreTopCandidates(options: {
    btree: BTree;
    documentsRoot: CID;
    candidates: ReadonlyMap<string, RankedCandidate>;
    parsed: ParsedRankedQuery;
    frequencies: ReadonlyMap<string, number>;
    fields: ReadonlyMap<string, RankedSearchSegmentManifest['fields'][number]>;
    selectedFields: ReadonlySet<string>;
    manifest: RankedSearchSegmentManifest;
    limit: number;
}): Promise<ScoredCandidate[]>;
export {};
//# sourceMappingURL=ranked-ranking.d.ts.map