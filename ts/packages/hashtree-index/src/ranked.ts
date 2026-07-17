import {
  HashTree,
  type CID,
  type Store,
} from '@hashtree/core';
import { BTree } from './btree.js';
import { buildRankedSegment } from './ranked-build.js';
import { queryRankedSegment } from './ranked-query.js';
import { decodeTermStats } from './ranked-schema.js';
import { readRankedSegment } from './ranked-segment.js';
import type {
  RankedSearchBuildOptions,
  RankedSearchDocument,
  RankedSearchIndexOptions,
  RankedSearchOptions,
  RankedSearchResult,
  RankedSearchSegmentManifest,
  RankedTermStats,
} from './ranked-types.js';

export { RANKED_SEARCH_SEGMENT_FORMAT } from './ranked-schema.js';

export class RankedSearchIndex {
  private readonly btree: BTree;
  private readonly tree: HashTree;

  constructor(store: Store, options: RankedSearchIndexOptions = {}) {
    this.btree = new BTree(store, { order: options.order ?? 64 });
    this.tree = new HashTree({ store });
  }

  async buildSegment(
    documents: Iterable<RankedSearchDocument>,
    options: RankedSearchBuildOptions,
  ): Promise<CID> {
    return await buildRankedSegment(this.btree, this.tree, documents, options);
  }

  async readManifest(root: CID): Promise<RankedSearchSegmentManifest> {
    return (await readRankedSegment(this.tree, root)).manifest;
  }

  async *streamTermStatistics(root: CID): AsyncGenerator<[string, RankedTermStats]> {
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

  async readTermStatistics(
    root: CID,
    terms: readonly string[],
  ): Promise<Map<string, RankedTermStats>> {
    const uniqueTerms = [...new Set(terms)];
    if (uniqueTerms.length === 0) return new Map();
    const { manifest, roots } = await readRankedSegment(this.tree, root);
    if (!roots.terms) {
      if (manifest.termCount !== 0) {
        throw new Error('Missing ranked search term statistics index');
      }
      return new Map();
    }

    const rawStatistics = await Promise.all(uniqueTerms.map(async (term) =>
      await this.btree.get(roots.terms, term)));
    const statistics = new Map<string, RankedTermStats>();
    for (let index = 0; index < uniqueTerms.length; index += 1) {
      const raw = rawStatistics[index];
      if (raw !== null) statistics.set(uniqueTerms[index], decodeTermStats(raw));
    }
    return statistics;
  }

  async search(
    root: CID,
    query: string,
    options: RankedSearchOptions = {},
  ): Promise<RankedSearchResult[]> {
    return await queryRankedSegment(this.btree, this.tree, root, query, options);
  }
}
