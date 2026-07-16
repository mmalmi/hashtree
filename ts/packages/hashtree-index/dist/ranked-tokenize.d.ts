import type { ParsedRankedQuery, RankedSearchFieldValue } from './ranked-types.js';
export interface RankedTokenOccurrence {
    term: string;
    position: number;
}
export interface TokenizedRankedField {
    length: number;
    occurrences: RankedTokenOccurrence[];
}
export declare function tokenizeRankedField(value: RankedSearchFieldValue | undefined, maxTokens: number): TokenizedRankedField;
export declare function parseRankedQuery(query: string): ParsedRankedQuery;
export declare function normalizeRankedText(text: string): string;
//# sourceMappingURL=ranked-tokenize.d.ts.map