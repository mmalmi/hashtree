import { type CID } from '@hashtree/core';
import { BTree } from './btree.js';
import type { ParsedRankedQuery, RankedPosting } from './ranked-types.js';
export type RankedCandidate = {
    postings: Map<string, RankedPosting>;
};
export declare function loadSelectedFrequencies(btree: BTree, termsRoot: CID, terms: readonly string[], selectedFields: ReadonlySet<string>, configuredFields: ReadonlyMap<string, unknown>, documentCount: number): Promise<Map<string, number>>;
export declare function hasMissingRequiredTerm(parsed: ParsedRankedQuery, frequencies: ReadonlyMap<string, number>, operator: 'or' | 'and'): boolean;
export declare function collectRankedCandidates(btree: BTree, postingsRoot: CID, terms: readonly string[], frequencies: ReadonlyMap<string, number>, fields: ReadonlySet<string>, operator: 'or' | 'and'): Promise<Map<string, RankedCandidate>>;
//# sourceMappingURL=ranked-candidates.d.ts.map