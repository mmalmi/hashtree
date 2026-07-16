import type { RankedDocumentStats, RankedPosting, RankedSearchFieldManifest } from './ranked-types.js';
export declare function scoreBm25fTerm(options: {
    posting: RankedPosting;
    document: RankedDocumentStats;
    fields: ReadonlyMap<string, RankedSearchFieldManifest>;
    selectedFields: ReadonlySet<string>;
    corpusDocuments: number;
    documentFrequency: number;
    k1: number;
}): number;
export declare function countMatchedPhrases(phrases: readonly (readonly string[])[], postingsByTerm: ReadonlyMap<string, RankedPosting>, selectedFields: ReadonlySet<string>): number;
//# sourceMappingURL=ranked-score.d.ts.map