import type { RankedDocumentStats, RankedPosting, RankedSearchBuildOptions, RankedSearchSegmentManifest, RankedTermStats } from './ranked-types.js';
export declare const RANKED_SEARCH_SEGMENT_FORMAT: "hashtree/ranked-search-segment@1";
export interface NormalizedRankedField {
    name: string;
    boost: number;
    lengthNormalization: number;
}
export interface NormalizedRankedBuildOptions {
    fields: NormalizedRankedField[];
    k1: number;
    maxTokensPerField: number;
}
export declare function normalizeRankedBuildOptions(options: RankedSearchBuildOptions): NormalizedRankedBuildOptions;
export declare function encodePosting(posting: RankedPosting): string;
export declare function decodePosting(raw: string): RankedPosting;
export declare function encodeDocumentStats(stats: RankedDocumentStats): string;
export declare function decodeDocumentStats(raw: string): RankedDocumentStats;
export declare function encodeTermStats(stats: RankedTermStats): string;
export declare function decodeTermStats(raw: string): RankedTermStats;
export declare function decodeRankedManifest(raw: string): RankedSearchSegmentManifest;
//# sourceMappingURL=ranked-schema.d.ts.map