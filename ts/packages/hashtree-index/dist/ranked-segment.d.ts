import { HashTree, type CID } from '@hashtree/core';
import type { RankedSearchSegmentManifest } from './ranked-types.js';
export type RankedSegmentRoots = {
    postings: CID | null;
    terms: CID | null;
    documents: CID | null;
    values: CID | null;
};
export declare function writeRankedSegment(tree: HashTree, manifest: RankedSearchSegmentManifest, roots: RankedSegmentRoots): Promise<CID>;
export declare function readRankedSegment(tree: HashTree, root: CID): Promise<{
    manifest: RankedSearchSegmentManifest;
    roots: RankedSegmentRoots;
}>;
//# sourceMappingURL=ranked-segment.d.ts.map