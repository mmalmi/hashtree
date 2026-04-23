import { BTree, SearchIndex, type SearchLinkResult } from '@hashtree/index';
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
import { materializeSearchTerms } from './helpers.js';

export class CollectionSource {
  readonly manifest: CollectionManifest;
  private readonly byIdIndex: BTree;
  private readonly linkIndex: BTree;
  private readonly byIdRoot;
  private readonly itemCount;
  private readonly searchIndexes = new Map<string, SearchIndex>();
  private readonly searchDefinitions = new Map<string, NonNullable<CollectionSourceQueryDefinition['searchIndexes']>[number]>();

  constructor(store: Store, manifest: CollectionManifest, definition?: CollectionSourceQueryDefinition | null) {
    this.manifest = manifest;
    this.byIdIndex = new BTree(store);
    this.linkIndex = new BTree(store);
    this.byIdRoot = deserializeCid(manifest.byIdRoot);
    this.itemCount = Number.isFinite(manifest.itemCount) && manifest.itemCount >= 0
      ? Math.floor(manifest.itemCount)
      : null;

    for (const [name, index] of Object.entries(manifest.indexes ?? {})) {
      if (index.kind === 'search') {
        this.searchIndexes.set(name, new SearchIndex(store, {
          order: index.options?.order,
          minKeywordLength: index.options?.minKeywordLength,
          stopWords: index.options?.stopWords ? new Set(index.options.stopWords) : undefined,
        }));
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

    return await this.byIdIndex.getLink(this.byIdRoot, id);
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
    if (this.itemCount !== null) {
      return this.itemCount;
    }

    return await this.exactCount();
  }

  async exactCount(): Promise<number> {
    if (!this.byIdRoot) {
      return 0;
    }

    return await this.byIdIndex.countLinks(this.byIdRoot);
  }

  async sampleById(limit: number, random: () => number = Math.random): Promise<CollectionIndexLinkResult[]> {
    if (!this.byIdRoot) {
      return [];
    }

    return (await this.byIdIndex.sampleLinks(this.byIdRoot, limit, { random }))
      .map(([key, cid]) => ({ key, cid }));
  }

  async queryById(options: { prefix?: string; limit?: number } = {}): Promise<CollectionIndexLinkResult[]> {
    if (!this.byIdRoot) {
      return [];
    }

    const results: CollectionIndexLinkResult[] = [];
    const limit = options.limit ?? Number.POSITIVE_INFINITY;
    const iterator = options.prefix
      ? this.byIdIndex.prefixLinks(this.byIdRoot, options.prefix)
      : this.byIdIndex.linksEntries(this.byIdRoot);

    for await (const [key, cid] of iterator) {
      results.push({ key, cid });
      if (results.length >= limit) {
        break;
      }
    }

    return results;
  }

  async *streamQueryById(options: { prefix?: string; limit?: number } = {}): AsyncGenerator<CollectionIndexLinkResult> {
    if (!this.byIdRoot) {
      return;
    }

    const limit = options.limit ?? Number.POSITIVE_INFINITY;
    let emitted = 0;
    const iterator = options.prefix
      ? this.byIdIndex.prefixLinks(this.byIdRoot, options.prefix)
      : this.byIdIndex.linksEntries(this.byIdRoot);

    for await (const [key, cid] of iterator) {
      yield { key, cid };
      emitted += 1;
      if (emitted >= limit) {
        break;
      }
    }
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
    options: { prefix?: string; limit?: number } = {},
  ): Promise<CollectionIndexLinkResult[]> {
    const manifestIndex = this.manifest.indexes[indexName];
    if (!manifestIndex) {
      return [];
    }

    const root = deserializeCid(manifestIndex.root);
    if (!root) {
      return [];
    }

    const results: CollectionIndexLinkResult[] = [];
    const limit = options.limit ?? Number.POSITIVE_INFINITY;
    const iterator = options.prefix
      ? this.linkIndex.prefixLinks(root, options.prefix)
      : this.linkIndex.linksEntries(root);

    for await (const [key, cid] of iterator) {
      results.push({ key, cid });
      if (results.length >= limit) {
        break;
      }
    }

    return results;
  }

  async *streamQueryIndex(
    indexName: string,
    options: { prefix?: string; limit?: number } = {},
  ): AsyncGenerator<CollectionIndexLinkResult> {
    const manifestIndex = this.manifest.indexes[indexName];
    if (!manifestIndex) {
      return;
    }

    const root = deserializeCid(manifestIndex.root);
    if (!root) {
      return;
    }

    const limit = options.limit ?? Number.POSITIVE_INFINITY;
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
