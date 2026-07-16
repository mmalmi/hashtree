import { HashTree, } from '@hashtree/core';
import { BTree } from './btree.js';
import { buildRankedSegment } from './ranked-build.js';
import { queryRankedSegment } from './ranked-query.js';
import { readRankedSegment } from './ranked-segment.js';
export { RANKED_SEARCH_SEGMENT_FORMAT } from './ranked-schema.js';
export class RankedSearchIndex {
    btree;
    tree;
    constructor(store, options = {}) {
        this.btree = new BTree(store, { order: options.order ?? 64 });
        this.tree = new HashTree({ store });
    }
    async buildSegment(documents, options) {
        return await buildRankedSegment(this.btree, this.tree, documents, options);
    }
    async readManifest(root) {
        return (await readRankedSegment(this.tree, root)).manifest;
    }
    async search(root, query, options = {}) {
        return await queryRankedSegment(this.btree, this.tree, root, query, options);
    }
}
//# sourceMappingURL=ranked.js.map