import { BTree, type SearchIndex, type SearchLinkResult } from '@hashtree/index';
import type {
  CID,
  CollectionIndexLinkResult,
  CollectionManifest,
  CollectionSourceQueryDefinition,
  CollectionSearchManifestIndex,
  SearchOptions,
  Store,
} from './types.js';
import { deserializeCid } from './cid.js';
import { createSearchIndex, materializeSearchTerms } from './helpers.js';

type QueryOptions = { prefix?: string; limit?: number };

export class CollectionSource {
  readonly manifest: CollectionManifest;
  private readonly linkIndex: BTree;
  private readonly byIdRoot;
  private readonly searchIndexes = new Map<string, SearchIndex>();
  private readonly searchDefinitions = new Map<string, NonNullable<CollectionSourceQueryDefinition['searchIndexes']>[number]>();

  constructor(store: Store, manifest: CollectionManifest, definition?: CollectionSourceQueryDefinition | null) {
    this.manifest = manifest;
    this.linkIndex = new BTree(store);
    this.byIdRoot = deserializeCid(manifest.byIdRoot);

    for (const [name, index] of Object.entries(manifest.indexes ?? {})) {
      if (index.kind === 'search') {
        this.searchIndexes.set(name, createSearchIndex(store, index.options));
      }
    }

    for (const searchIndex of definition?.searchIndexes ?? []) {
      this.searchDefinitions.set(searchIndex.name, searchIndex);
    }
  }

  async get(id: string): Promise<CID | null> {
    if (!this.byIdRoot) {
      return null;
    }

    return await this.linkIndex.getLink(this.byIdRoot, id);
  }

  async getIndexLink(indexName: string, key: string): Promise<CID | null> {
    const manifestIndex = this.manifest.indexes[indexName];
    if (!manifestIndex) {
      return null;
    }

    const root = deserializeCid(manifestIndex.root);
    if (!root) {
      return null;
    }

    return await this.linkIndex.getLink(root, key);
  }

  async count(): Promise<number> {
    return await this.exactCount();
  }

  async countReported(): Promise<number | null> {
    if (!this.byIdRoot) {
      return 0;
    }

    return await this.linkIndex.countReportedLinks(this.byIdRoot);
  }

  async exactCount(): Promise<number> {
    if (!this.byIdRoot) {
      return 0;
    }

    return await this.linkIndex.countLinks(this.byIdRoot);
  }

  async sampleById(limit: number, random: () => number = Math.random): Promise<CollectionIndexLinkResult[]> {
    if (!this.byIdRoot) {
      return [];
    }

    return (await this.linkIndex.sampleLinks(this.byIdRoot, limit, { random }))
      .map(([key, cid]) => ({ key, cid }));
  }

  async queryById(options: QueryOptions = {}): Promise<CollectionIndexLinkResult[]> {
    return collectLinks(this.streamQueryById(options));
  }

  streamQueryById(options: QueryOptions = {}): AsyncGenerator<CollectionIndexLinkResult> {
    return this.streamLinks(this.byIdRoot, options);
  }

  async search(indexName: string, query: string, options: SearchOptions = {}): Promise<SearchLinkResult[]> {
    const manifestIndex = this.manifest.indexes[indexName];
    if (!manifestIndex || manifestIndex.kind !== 'search') {
      return [];
    }

    const root = deserializeCid(manifestIndex.root);
    const searchIndex = this.searchIndexes.get(indexName);
    if (!root || !searchIndex) {
      return [];
    }

    return await searchIndex.searchLinkTerms(
      root,
      manifestIndex.prefix,
      this.parseSearchTerms(indexName, query),
      options,
    );
  }

  async searchTerms(
    indexName: string,
    terms: Iterable<string>,
    options: SearchOptions = {},
  ): Promise<SearchLinkResult[]> {
    const manifestIndex = this.manifest.indexes[indexName];
    if (!manifestIndex || manifestIndex.kind !== 'search') {
      return [];
    }

    const root = deserializeCid(manifestIndex.root);
    const searchIndex = this.searchIndexes.get(indexName);
    if (!root || !searchIndex) {
      return [];
    }

    return await searchIndex.searchLinkTerms(root, manifestIndex.prefix, terms, options);
  }

  parseSearchTerms(indexName: string, query: string): string[] {
    const searchIndex = this.searchIndexes.get(indexName);
    if (!searchIndex) {
      return [];
    }

    const definition = this.searchDefinitions.get(indexName);
    if (!definition) {
      return searchIndex.parseKeywords(query);
    }

    return materializeSearchTerms(definition, searchIndex, query);
  }

  async queryIndex(
    indexName: string,
    options: QueryOptions = {},
  ): Promise<CollectionIndexLinkResult[]> {
    return collectLinks(this.streamQueryIndex(indexName, options));
  }

  async *streamQueryIndex(
    indexName: string,
    options: QueryOptions = {},
  ): AsyncGenerator<CollectionIndexLinkResult> {
    const manifestIndex = this.manifest.indexes[indexName];
    if (!manifestIndex) {
      return;
    }

    yield* this.streamLinks(deserializeCid(manifestIndex.root), options);
  }

  private async *streamLinks(root: CID | null, options: QueryOptions): AsyncGenerator<CollectionIndexLinkResult> {
    const limit = options.limit ?? Number.POSITIVE_INFINITY;
    if (!root || limit <= 0) {
      return;
    }

    let emitted = 0;
    const iterator = options.prefix
      ? this.linkIndex.prefixLinks(root, options.prefix)
      : this.linkIndex.linksEntries(root);

    for await (const [key, cid] of iterator) {
      yield { key, cid };
      emitted += 1;
      if (emitted >= limit) {
        break;
      }
    }
  }

  getSearchManifestIndex(indexName: string): CollectionSearchManifestIndex | null {
    const manifestIndex = this.manifest.indexes[indexName];
    if (!manifestIndex || manifestIndex.kind !== 'search') {
      return null;
    }

    return manifestIndex;
  }
}

async function collectLinks(iterator: AsyncIterable<CollectionIndexLinkResult>): Promise<CollectionIndexLinkResult[]> {
  const results: CollectionIndexLinkResult[] = [];
  for await (const result of iterator) results.push(result);
  return results;
}
