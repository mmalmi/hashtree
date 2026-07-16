import { type CID, type HashTree } from '@hashtree/core';
import { BTree } from './btree.js';
import type { RankedSearchOptions, RankedSearchResult } from './ranked-types.js';
export declare function queryRankedSegment(btree: BTree, tree: HashTree, root: CID, query: string, options: RankedSearchOptions): Promise<RankedSearchResult[]>;
//# sourceMappingURL=ranked-query.d.ts.map