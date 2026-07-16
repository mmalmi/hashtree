export type RankedSearchFieldValue = string | readonly string[];
export interface RankedSearchDocument {
    id: string;
    fields: Readonly<Record<string, RankedSearchFieldValue | undefined>>;
    /** Opaque caller-owned value returned for ranked hits. */
    value?: string;
}
export interface RankedSearchFieldOptions {
    /** Relative BM25F field weight. Default: 1. */
    boost?: number;
    /** BM25 length normalization (b), from 0 through 1. Default: 0.75. */
    lengthNormalization?: number;
}
export interface RankedSearchBuildOptions {
    fields: Readonly<Record<string, RankedSearchFieldOptions>>;
    /** BM25 saturation parameter (k1). Default: 1.2. */
    k1?: number;
    /** Maximum lexical positions indexed in each field. Default: 4096. */
    maxTokensPerField?: number;
}
export interface RankedSearchIndexOptions {
    /** Max entries per B-tree node. Default: 64. */
    order?: number;
}
export interface RankedSearchOptions {
    /** Maximum results. Default: 20. */
    limit?: number;
    /** Whether every query term must match. Default: "or". */
    operator?: 'or' | 'and';
    /** Restrict matching and ranking to these configured fields. */
    fields?: readonly string[];
}
export interface RankedSearchResult {
    id: string;
    score: number;
    value?: string;
    matchedTerms: string[];
    matchedPhrases: number;
}
export interface RankedSearchFieldManifest {
    name: string;
    boost: number;
    lengthNormalization: number;
    totalLength: number;
    populatedDocumentCount: number;
}
export interface RankedSearchSegmentManifest {
    format: 'hashtree/ranked-search-segment@1';
    normalization: 'NFKC-lowercase@1';
    documentCount: number;
    termCount: number;
    postingCount: number;
    storedValueCount: number;
    k1: number;
    maxTokensPerField: number;
    fields: RankedSearchFieldManifest[];
}
export interface RankedPostingField {
    frequency: number;
    positions: number[];
}
export interface RankedPosting {
    fields: Record<string, RankedPostingField>;
}
export interface RankedDocumentStats {
    lengths: Record<string, number>;
}
export interface RankedTermStats {
    documentFrequency: number;
    fieldSets: Array<{
        fields: string[];
        documentFrequency: number;
    }>;
}
export interface ParsedRankedQuery {
    terms: string[];
    phrases: string[][];
}
//# sourceMappingURL=ranked-types.d.ts.map