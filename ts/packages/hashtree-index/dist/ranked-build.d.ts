import type { CID, HashTree } from '@hashtree/core';
import { BTree } from './btree.js';
import type { RankedSearchBuildOptions, RankedSearchDocument } from './ranked-types.js';
export declare function buildRankedSegment(btree: BTree, tree: HashTree, documents: Iterable<RankedSearchDocument>, options: RankedSearchBuildOptions): Promise<CID>;
//# sourceMappingURL=ranked-build.d.ts.map