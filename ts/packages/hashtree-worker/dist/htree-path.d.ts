export interface ParsedMutableHtreePath {
    npub: string;
    treeName: string;
    filePath: string;
}
export interface ParsedImmutableHtreePath {
    nhash: string;
    filePath: string;
}
export declare function getRawHtreePath(url: URL): string;
export declare function parseMutableHtreePath(rawPath: string): ParsedMutableHtreePath | null;
export declare function parseImmutableHtreePath(rawPath: string): ParsedImmutableHtreePath | null;
//# sourceMappingURL=htree-path.d.ts.map