export interface ResolvedByteRange {
    start: number;
    endInclusive: number;
}
export type ParsedHttpRange = {
    kind: 'range';
    range: ResolvedByteRange;
} | {
    kind: 'unsatisfiable';
} | {
    kind: 'unsupported';
};
export declare function parseHttpByteRange(rangeHeader: string | null | undefined, totalSize: number): ParsedHttpRange;
//# sourceMappingURL=httpRange.d.ts.map