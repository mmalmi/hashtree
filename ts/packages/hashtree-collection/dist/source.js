import { BTree } from '@hashtree/index';
import { deserializeCid } from './cid.js';
import { createSearchIndex, materializeSearchTerms } from './helpers.js';
export class CollectionSource {
    manifest;
    linkIndex;
    byIdRoot;
    searchIndexes = new Map();
    searchDefinitions = new Map();
    constructor(store, manifest, definition) {
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
    async get(id) {
        if (!this.byIdRoot) {
            return null;
        }
        return await this.linkIndex.getLink(this.byIdRoot, id);
    }
    async getIndexLink(indexName, key) {
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
    async count() {
        return await this.exactCount();
    }
    async countReported() {
        if (!this.byIdRoot) {
            return 0;
        }
        return await this.linkIndex.countReportedLinks(this.byIdRoot);
    }
    async exactCount() {
        if (!this.byIdRoot) {
            return 0;
        }
        return await this.linkIndex.countLinks(this.byIdRoot);
    }
    async sampleById(limit, random = Math.random) {
        if (!this.byIdRoot) {
            return [];
        }
        return (await this.linkIndex.sampleLinks(this.byIdRoot, limit, { random }))
            .map(([key, cid]) => ({ key, cid }));
    }
    async queryById(options = {}) {
        return collectLinks(this.streamQueryById(options));
    }
    streamQueryById(options = {}) {
        return this.streamLinks(this.byIdRoot, options);
    }
    async search(indexName, query, options = {}) {
        const manifestIndex = this.manifest.indexes[indexName];
        if (!manifestIndex || manifestIndex.kind !== 'search') {
            return [];
        }
        const root = deserializeCid(manifestIndex.root);
        const searchIndex = this.searchIndexes.get(indexName);
        if (!root || !searchIndex) {
            return [];
        }
        return await searchIndex.searchLinkTerms(root, manifestIndex.prefix, this.parseSearchTerms(indexName, query), options);
    }
    async searchTerms(indexName, terms, options = {}) {
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
    parseSearchTerms(indexName, query) {
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
    async queryIndex(indexName, options = {}) {
        return collectLinks(this.streamQueryIndex(indexName, options));
    }
    async *streamQueryIndex(indexName, options = {}) {
        const manifestIndex = this.manifest.indexes[indexName];
        if (!manifestIndex) {
            return;
        }
        yield* this.streamLinks(deserializeCid(manifestIndex.root), options);
    }
    async *streamLinks(root, options) {
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
    getSearchManifestIndex(indexName) {
        const manifestIndex = this.manifest.indexes[indexName];
        if (!manifestIndex || manifestIndex.kind !== 'search') {
            return null;
        }
        return manifestIndex;
    }
}
async function collectLinks(iterator) {
    const results = [];
    for await (const result of iterator)
        results.push(result);
    return results;
}
//# sourceMappingURL=source.js.map