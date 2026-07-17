import type { RankedSearchFieldManifest, RankedSearchScoringContext, RankedSearchSegmentManifest } from './ranked-types.js';
export interface PreparedRankedScoringContext {
    corpusDocuments: number;
    k1: number;
    fields: ReadonlyMap<string, RankedSearchFieldManifest>;
    frequencies: ReadonlyMap<string, number>;
}
export declare function prepareRankedScoringContext(context: RankedSearchScoringContext, segment: RankedSearchSegmentManifest, queryTerms: readonly string[], selectedFields: ReadonlySet<string>, localFrequencies: ReadonlyMap<string, number>): PreparedRankedScoringContext;
//# sourceMappingURL=ranked-scoring-context.d.ts.map