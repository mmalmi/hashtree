import { BTree } from '@hashtree/index';
import { createSearchIndex, defaultSearchPrefix, materializeKeyValues, materializeSearchEntries, materializeSearchTerms, } from './helpers.js';
import { collectionManifestFromState, collectionStateFromManifest, createEmptyCollectionState } from './manifest.js';
import { normalizeCollectionItem } from './schema.js';
export class CollectionWriter {
    store;
    definition;
    hasDerivedIndexes;
    byIdIndex;
    linkIndex;
    searchIndexes = new Map();
    state;
    constructor(store, definition, initialManifest) {
        this.store = store;
        this.definition = definition;
        this.hasDerivedIndexes = (definition.keyIndexes?.length ?? 0) > 0 || (definition.searchIndexes?.length ?? 0) > 0;
        this.byIdIndex = new BTree(store);
        this.linkIndex = new BTree(store);
        this.state = initialManifest
            ? collectionStateFromManifest(definition, initialManifest)
            : createEmptyCollectionState(definition);
        for (const index of definition.searchIndexes ?? []) {
            this.searchIndexes.set(index.name, createSearchIndex(this.store, index.options));
        }
    }
    get snapshot() {
        return {
            byIdRoot: this.state.byIdRoot,
            keyRoots: { ...this.state.keyRoots },
            searchRoots: { ...this.state.searchRoots },
            itemCount: this.state.itemCount,
            updatedAt: this.state.updatedAt,
        };
    }
    manifest(metadata) {
        return collectionManifestFromState(this.definition, this.state, metadata);
    }
    normalize(item, fromVersion) {
        return normalizeCollectionItem(this.definition, item, { fromVersion });
    }
    async replace(item, cid, previous, options = {}) {
        return this.put(item, cid, {
            ...options,
            previous,
        });
    }
    async put(item, cid, options = {}) {
        const nextItem = this.normalize(item);
        const id = this.definition.getId(nextItem).trim();
        if (!id) {
            throw new Error('Collection item id must not be empty');
        }
        if (!options.previous && this.hasDerivedIndexes) {
            const existingCid = this.state.byIdRoot
                ? await this.byIdIndex.getLink(this.state.byIdRoot, id)
                : null;
            if (existingCid) {
                throw new Error(`CollectionWriter.put requires options.previous when replacing existing id "${id}" in a collection with derived indexes; use replace(...) or reindex(...) when the previous item is unavailable`);
            }
        }
        if (options.previous) {
            await this.delete(options.previous, {
                context: options.previousContext ?? options.context,
            });
        }
        const existed = this.state.byIdRoot
            ? (await this.byIdIndex.getLink(this.state.byIdRoot, id)) !== null
            : false;
        this.state.byIdRoot = await this.byIdIndex.insertLink(this.state.byIdRoot, id, cid);
        for (const index of this.definition.keyIndexes ?? []) {
            let root = this.state.keyRoots[index.name] ?? null;
            for (const key of materializeKeyValues(index, nextItem)) {
                root = await this.linkIndex.insertLink(root, key, cid);
            }
            this.state.keyRoots[index.name] = root;
        }
        const searchRootGroups = new Map();
        for (const index of this.definition.searchIndexes ?? []) {
            const searchIndex = this.searchIndexes.get(index.name);
            if (!searchIndex) {
                continue;
            }
            const rootName = index.rootName ?? index.name;
            let root = searchRootGroups.has(rootName)
                ? searchRootGroups.get(rootName) ?? null
                : this.readSearchRootGroup(rootName);
            for (const entry of materializeSearchEntries(index, nextItem, {
                id,
                cid,
                writeContext: options.context,
            })) {
                const terms = materializeSearchTerms(index, searchIndex, entry.text);
                if (terms.length === 0) {
                    continue;
                }
                root = await searchIndex.indexLink(root, entry.prefix ?? index.prefix ?? defaultSearchPrefix(index.name), terms, entry.id ?? id, entry.cid ?? cid);
            }
            searchRootGroups.set(rootName, root);
        }
        if (searchRootGroups.size > 0) {
            this.assignSearchRootGroups(searchRootGroups);
        }
        if (!existed) {
            this.state.itemCount += 1;
        }
        this.state.updatedAt = Date.now();
        return this.snapshot;
    }
    async delete(item, options = {}) {
        const nextItem = this.normalize(item);
        const id = this.definition.getId(nextItem).trim();
        if (!id) {
            return this.snapshot;
        }
        let existed = false;
        if (this.state.byIdRoot) {
            existed = (await this.byIdIndex.getLink(this.state.byIdRoot, id)) !== null;
            if (existed) {
                this.state.byIdRoot = await this.byIdIndex.delete(this.state.byIdRoot, id);
            }
        }
        for (const index of this.definition.keyIndexes ?? []) {
            let root = this.state.keyRoots[index.name] ?? null;
            if (!root) {
                continue;
            }
            for (const key of materializeKeyValues(index, nextItem)) {
                if (!root) {
                    break;
                }
                root = await this.linkIndex.delete(root, key);
            }
            this.state.keyRoots[index.name] = root;
        }
        const searchRootGroups = new Map();
        for (const index of this.definition.searchIndexes ?? []) {
            const searchIndex = this.searchIndexes.get(index.name);
            const rootName = index.rootName ?? index.name;
            let root = searchRootGroups.has(rootName)
                ? searchRootGroups.get(rootName) ?? null
                : this.readSearchRootGroup(rootName);
            if (!searchIndex || !root) {
                continue;
            }
            for (const entry of materializeSearchEntries(index, nextItem, {
                id,
                cid: null,
                writeContext: options.context,
            })) {
                const terms = materializeSearchTerms(index, searchIndex, entry.text);
                const entryId = entry.id ?? id;
                const prefix = entry.prefix ?? index.prefix ?? defaultSearchPrefix(index.name);
                for (const term of terms) {
                    root = await this.linkIndex.delete(root, `${prefix}${term}:${entryId}`);
                    if (!root) {
                        break;
                    }
                }
                if (!root) {
                    break;
                }
            }
            searchRootGroups.set(rootName, root);
        }
        if (searchRootGroups.size > 0) {
            this.assignSearchRootGroups(searchRootGroups);
        }
        else {
            for (const index of this.definition.searchIndexes ?? []) {
                if (!Object.hasOwn(this.state.searchRoots, index.name)) {
                    continue;
                }
                const rootName = index.rootName ?? index.name;
                this.state.searchRoots[index.name] = this.readSearchRootGroup(rootName);
            }
        }
        if (existed) {
            this.state.itemCount = Math.max(0, this.state.itemCount - 1);
            this.state.updatedAt = Date.now();
        }
        return this.snapshot;
    }
    async batch(mutations) {
        for (const mutation of mutations) {
            if (mutation.type === 'put') {
                await this.put(mutation.item, mutation.cid, {
                    previous: mutation.previous,
                    context: mutation.context,
                    previousContext: mutation.previousContext,
                });
            }
            else {
                await this.delete(mutation.item, {
                    context: mutation.context,
                });
            }
        }
        return this.snapshot;
    }
    async rebuild(entries) {
        this.state = createEmptyCollectionState(this.definition);
        const byIdEntries = new Map();
        const uniqueIds = new Set();
        const keyEntries = new Map();
        const searchEntries = new Map();
        const searchBuilders = new Map();
        for await (const entry of entries) {
            const nextItem = this.normalize(entry.item);
            const id = this.definition.getId(nextItem).trim();
            if (!id) {
                throw new Error('Collection item id must not be empty');
            }
            byIdEntries.set(id, entry.cid);
            uniqueIds.add(id);
            for (const index of this.definition.keyIndexes ?? []) {
                let indexEntries = keyEntries.get(index.name);
                if (!indexEntries) {
                    indexEntries = new Map();
                    keyEntries.set(index.name, indexEntries);
                }
                for (const key of materializeKeyValues(index, nextItem)) {
                    indexEntries.set(key, entry.cid);
                }
            }
            for (const index of this.definition.searchIndexes ?? []) {
                const searchIndex = this.searchIndexes.get(index.name);
                if (!searchIndex) {
                    continue;
                }
                const rootName = index.rootName ?? index.name;
                let rootEntries = searchEntries.get(rootName);
                if (!rootEntries) {
                    rootEntries = new Map();
                    searchEntries.set(rootName, rootEntries);
                }
                if (!searchBuilders.has(rootName)) {
                    searchBuilders.set(rootName, searchIndex);
                }
                for (const searchEntry of materializeSearchEntries(index, nextItem, {
                    id,
                    cid: entry.cid,
                    writeContext: entry.context,
                })) {
                    const targetCid = searchEntry.cid ?? entry.cid;
                    if (!targetCid) {
                        continue;
                    }
                    const terms = materializeSearchTerms(index, searchIndex, searchEntry.text);
                    if (terms.length === 0) {
                        continue;
                    }
                    const prefix = searchEntry.prefix ?? index.prefix ?? defaultSearchPrefix(index.name);
                    const entryId = searchEntry.id ?? id;
                    for (const term of terms) {
                        rootEntries.set(`${prefix}${term}:${entryId}`, targetCid);
                    }
                }
            }
        }
        this.state.byIdRoot = await this.byIdIndex.buildLinks(byIdEntries);
        for (const index of this.definition.keyIndexes ?? []) {
            this.state.keyRoots[index.name] = await this.linkIndex.buildLinks(keyEntries.get(index.name) ?? []);
        }
        const searchRootGroups = new Map();
        for (const [rootName, rootEntries] of searchEntries) {
            const searchIndex = searchBuilders.get(rootName);
            searchRootGroups.set(rootName, searchIndex ? await searchIndex.buildLinks(rootEntries) : null);
        }
        if (searchRootGroups.size > 0) {
            this.assignSearchRootGroups(searchRootGroups);
        }
        this.state.itemCount = uniqueIds.size;
        this.state.updatedAt = uniqueIds.size > 0 ? Date.now() : 0;
        return this.snapshot;
    }
    async reindex(entries) {
        return this.rebuild(entries);
    }
    readSearchRootGroup(rootName) {
        for (const index of this.definition.searchIndexes ?? []) {
            if ((index.rootName ?? index.name) === rootName) {
                return this.state.searchRoots[index.name] ?? null;
            }
        }
        return null;
    }
    assignSearchRootGroups(groups) {
        for (const index of this.definition.searchIndexes ?? []) {
            const rootName = index.rootName ?? index.name;
            if (groups.has(rootName)) {
                this.state.searchRoots[index.name] = groups.get(rootName) ?? null;
            }
        }
    }
}
//# sourceMappingURL=writer.js.map