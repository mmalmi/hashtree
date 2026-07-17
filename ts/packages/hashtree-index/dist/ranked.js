import { HashTree, } from '@hashtree/core';
import { BTree } from './btree.js';
import { buildRankedSegment } from './ranked-build.js';
import { queryRankedSegment } from './ranked-query.js';
import { decodeTermStats } from './ranked-schema.js';
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
    async *streamTermStatistics(root) {
        const { manifest, roots } = await readRankedSegment(this.tree, root);
        if (!roots.terms) {
            if (manifest.termCount !== 0) {
                throw new Error('Missing ranked search term statistics index');
            }
            return;
        }
        let count = 0;
        for await (const [term, raw] of this.btree.entries(roots.terms)) {
            count += 1;
            if (count > manifest.termCount) {
                throw new Error('Ranked search term statistics exceed manifest coverage');
            }
            yield [term, decodeTermStats(raw)];
        }
        if (count !== manifest.termCount) {
            throw new Error('Ranked search term statistics do not match manifest coverage');
        }
    }
    async readTermStatistics(root, terms) {
        const uniqueTerms = [...new Set(terms)];
        if (uniqueTerms.length === 0)
            return new Map();
        const { manifest, roots } = await readRankedSegment(this.tree, root);
        if (!roots.terms) {
            if (manifest.termCount !== 0) {
                throw new Error('Missing ranked search term statistics index');
            }
            return new Map();
        }
        const rawStatistics = await Promise.all(uniqueTerms.map(async (term) => await this.btree.get(roots.terms, term)));
        const statistics = new Map();
        for (let index = 0; index < uniqueTerms.length; index += 1) {
            const raw = rawStatistics[index];
            if (raw !== null)
                statistics.set(uniqueTerms[index], decodeTermStats(raw));
        }
        return statistics;
    }
    async search(root, query, options = {}) {
        return await queryRankedSegment(this.btree, this.tree, root, query, options);
    }
}
//# sourceMappingURL=ranked.js.map