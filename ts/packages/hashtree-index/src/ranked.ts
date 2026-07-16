import {
  HashTree,
  type CID,
  type Store,
} from '@hashtree/core';
import { BTree } from './btree.js';
import { buildRankedSegment } from './ranked-build.js';
import { queryRankedSegment } from './ranked-query.js';
import { readRankedSegment } from './ranked-segment.js';
import type {
  RankedSearchBuildOptions,
  RankedSearchDocument,
  RankedSearchIndexOptions,
  RankedSearchOptions,
  RankedSearchResult,
  RankedSearchSegmentManifest,
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

  async search(
    root: CID,
    query: string,
    options: RankedSearchOptions = {},
  ): Promise<RankedSearchResult[]> {
    return await queryRankedSegment(this.btree, this.tree, root, query, options);
  }
}
