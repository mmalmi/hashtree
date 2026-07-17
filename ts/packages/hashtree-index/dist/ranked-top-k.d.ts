import { HashTree, type CID } from '@hashtree/core';
import { BTree } from './btree.js';
import type { RankedDocumentStats, RankedPosting, RankedSearchSegmentManifest, RankedTermStats } from './ranked-types.js';
export declare const RANKED_TOP_K_FORMAT: "hashtree/ranked-top-k@1";
export declare const RANKED_TOP_K_BLOCK_SIZE = 32;
export declare const RANKED_TOP_K_FANOUT = 32;
export declare const RANKED_TOP_K_MIN_DOCUMENT_FREQUENCY = 128;
export interface RankedTopKManifest {
    format: typeof RANKED_TOP_K_FORMAT;
    blockSize: number;
    fanout: number;
    minimumDocumentFrequency: number;
    termCount: number;
    blockCount: number;
    postingCount: number;
}
export interface RankedTopKFieldBound {
    maxFrequency: number;
    minLength: number;
}
export interface RankedTopKSummary {
    count: number;
    minId: string;
    fields: Record<string, RankedTopKFieldBound>;
}
export interface RankedTopKBuildEntry {
    id: string;
    posting: RankedPosting;
    document: RankedDocumentStats;
}
export type RankedTopKNode = {
    summary: RankedTopKSummary;
} & ({
    kind: 'leaf';
    ids: string[];
} | {
    kind: 'internal';
    children: Array<{
        cid: CID;
        summary: RankedTopKSummary;
    }>;
});
export declare function buildRankedTopK(btree: BTree, tree: HashTree, entriesByTerm: ReadonlyMap<string, readonly RankedTopKBuildEntry[]>, termStatistics: ReadonlyMap<string, RankedTermStats>, segment: RankedSearchSegmentManifest): Promise<{
    manifest: RankedTopKManifest;
    roots: CID | null;
}>;
export declare function readRankedTopKNode(tree: HashTree, root: CID): Promise<RankedTopKNode>;
export declare function encodeRankedTopKManifest(manifest: RankedTopKManifest): string;
export declare function decodeRankedTopKManifest(raw: string): RankedTopKManifest;
//# sourceMappingURL=ranked-top-k.d.ts.map