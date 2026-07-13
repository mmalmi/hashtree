import { HashTree, LinkType, toHex } from '@hashtree/core';
const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder();
export class BTree {
    tree;
    order;
    maxKeys;
    constructor(store, options = {}) {
        this.tree = new HashTree({ store });
        this.order = options.order ?? 32;
        this.maxKeys = this.order - 1;
    }
    // ============ String Value Methods (existing) ============
    async insert(root, key, value) {
        if (!root) {
            return this.createLeaf([[key, value]]);
        }
        const result = await this.insertRecursive(root, key, value);
        if (result.unchanged) {
            return root;
        }
        if (result.split) {
            return (await this.tree.putDirectory([
                treeEntry(escapeKey(result.split.leftFirstKey), result.split.left, 0, LinkType.Dir),
                treeEntry(escapeKey(result.split.rightFirstKey), result.split.right, 0, LinkType.Dir),
            ])).cid;
        }
        return result.cid;
    }
    async get(root, key) {
        if (!root)
            return null;
        const entries = await this.tree.listDirectory(root);
        const isLeaf = this.isLeafNode(entries);
        if (isLeaf) {
            const escapedKey = escapeKey(key);
            const entry = entries.find(e => e.name === escapedKey);
            if (!entry || entry.type !== LinkType.Blob)
                return null;
            const data = await this.tree.readFile(entry.cid);
            if (!data)
                return null;
            return textDecoder.decode(data);
        }
        const { child } = this.findChild(entries, key);
        return this.get(child.cid, key);
    }
    // ============ CID Link Methods (new) ============
    /**
     * Insert a CID link into the tree.
     * Uses LinkType.File to store the target CID directly as a native link.
     * This enables natural deduplication and avoids JSON serialization.
     */
    async insertLink(root, key, targetCid, options = {}) {
        if (!root) {
            return this.createLeafWithLink([[key, targetCid]]);
        }
        const result = await this.insertLinkRecursive(root, key, targetCid, options.signal);
        if (result.unchanged) {
            return root;
        }
        if (result.split) {
            return (await this.tree.putDirectory([
                treeEntry(escapeKey(result.split.leftFirstKey), result.split.left, result.split.leftCount, LinkType.Dir),
                treeEntry(escapeKey(result.split.rightFirstKey), result.split.right, result.split.rightCount, LinkType.Dir),
            ])).cid;
        }
        return result.cid;
    }
    /**
     * Get a CID link from the tree.
     */
    async getLink(root, key, options = {}) {
        if (!root)
            return null;
        const entries = await this.tree.listDirectory(root, options.signal);
        const isLeaf = this.isLeafNode(entries);
        if (isLeaf) {
            const escapedKey = escapeKey(key);
            const entry = entries.find(e => e.name === escapedKey);
            if (!entry || entry.type !== LinkType.File)
                return null;
            return entry.cid;
        }
        const { child } = this.findChild(entries, key);
        return this.getLink(child.cid, key, options);
    }
    /**
     * Iterate all CID links in the tree.
     */
    async *linksEntries(root, options = {}) {
        if (!root)
            return;
        if (options.verifyCount === true) {
            yield* this.verifiedLinksEntries(root);
            return;
        }
        yield* this.traverseLinksInOrder(root);
    }
    /**
     * Iterate all CID links and throw if stored subtree counts disagree with
     * the number of yielded links. This protects callers from accepting a
     * partial traversal when a child node is unreadable or malformed.
     */
    async *verifiedLinksEntries(root) {
        if (!root)
            return;
        const expectedCount = await this.countReportedLinks(root);
        const yieldedCount = yield* this.traverseLinksInOrderVerified(root, expectedCount);
        if (expectedCount !== null && yieldedCount !== expectedCount) {
            throw new Error(`BTree link traversal yielded ${yieldedCount} links, expected ${expectedCount}`);
        }
    }
    /**
     * Prefix search for CID links.
     */
    async *prefixLinks(root, prefix) {
        const endPrefix = incrementPrefix(prefix);
        yield* this.rangeLinkTraverse(root, prefix, endPrefix);
    }
    /**
     * Count CID links by walking the tree.
     * Uses stored subtree sizes when available, but may scan descendants when
     * older roots do not carry complete counts.
     */
    async countLinks(root) {
        return await this.scanLinks(root);
    }
    /**
     * Count CID links by walking the tree.
     */
    async scanLinks(root) {
        if (!root) {
            return 0;
        }
        return await this.countLinksRecursive(root, createLinkTraversalCache());
    }
    /**
     * Explicit count-scan alias for callers that need to make scan semantics
     * clear at the call site.
     */
    async scanLinkCount(root) {
        return await this.scanLinks(root);
    }
    /**
     * Read the stored CID-link count from the root node without scanning.
     * Returns null when the root was built by older code that does not store
     * complete subtree sizes.
     */
    async countStoredLinks(root) {
        if (!root) {
            return 0;
        }
        const entries = await this.tree.listDirectory(root);
        if (this.isLeafNode(entries)) {
            return this.countLinkEntries(entries);
        }
        let count = 0;
        for (const entry of entries) {
            const childCount = this.storedLinkSubtreeCount(entry);
            if (childCount === null) {
                return null;
            }
            count += childCount;
        }
        return count;
    }
    /**
     * Explicit no-scan reported-count alias. Returns null when the B-tree does
     * not carry complete stored subtree sizes.
     */
    async countReportedLinks(root) {
        return await this.countStoredLinks(root);
    }
    /**
     * Read the Nth CID link in sorted key order.
     */
    async getLinkEntryAt(root, ordinal) {
        if (!root || ordinal < 0) {
            return null;
        }
        return await this.getLinkEntryAtRecursive(root, Math.floor(ordinal), createLinkTraversalCache());
    }
    /**
     * Sample CID links uniformly by random ordinal.
     */
    async sampleLinks(root, limit, options = {}) {
        if (!root) {
            return [];
        }
        const effectiveLimit = Number.isFinite(limit) ? Math.max(0, Math.floor(limit)) : 0;
        if (effectiveLimit === 0) {
            return [];
        }
        const cache = createLinkTraversalCache();
        const totalCount = Number.isFinite(options.totalCount)
            ? Math.max(0, Math.floor(options.totalCount ?? 0))
            : await this.countLinksRecursive(root, cache);
        if (totalCount === 0) {
            return [];
        }
        const targetCount = Math.min(totalCount, effectiveLimit);
        const ordinals = sampleUniqueIntegers(totalCount, targetCount, options.random ?? Math.random);
        const results = [];
        for (const ordinal of ordinals) {
            const entry = await this.getLinkEntryAtRecursive(root, ordinal, cache);
            if (entry) {
                results.push(entry);
            }
        }
        return results;
    }
    /**
     * Merge two BTree roots with CID link values.
     */
    async mergeLinks(base, other, preferOther = false) {
        if (!other)
            return base;
        if (!base)
            return other;
        let result = base;
        for await (const [key, cid] of this.linksEntries(other)) {
            const existingCid = await this.getLink(result, key);
            if (existingCid === null || preferOther) {
                result = await this.insertLink(result, key, cid);
            }
        }
        return result;
    }
    async build(items) {
        return this.buildTree(items, (chunk) => this.createLeaf(chunk), false);
    }
    async buildLinks(items) {
        return this.buildTree(items, (chunk) => this.createLeafWithLink(chunk), true);
    }
    async buildTree(items, createLeaf, preserveCounts) {
        const sorted = [...items];
        if (sorted.length === 0) {
            return null;
        }
        sorted.sort((left, right) => compareKeys(left[0], right[0]));
        const deduped = [];
        for (const [key, value] of sorted) {
            const last = deduped[deduped.length - 1];
            if (last && last[0] === key) {
                last[1] = value;
                continue;
            }
            deduped.push([key, value]);
        }
        let level = [];
        for (let index = 0; index < deduped.length; index += this.maxKeys) {
            const chunk = deduped.slice(index, index + this.maxKeys);
            level.push({
                firstKey: chunk[0][0],
                cid: await createLeaf(chunk),
                count: preserveCounts ? chunk.length : 0,
            });
        }
        while (level.length > 1) {
            const nextLevel = [];
            for (let index = 0; index < level.length; index += this.maxKeys) {
                const chunk = level.slice(index, index + this.maxKeys);
                nextLevel.push({
                    firstKey: chunk[0].firstKey,
                    cid: await this.createInternalNode(chunk),
                    count: chunk.reduce((sum, child) => sum + child.count, 0),
                });
            }
            level = nextLevel;
        }
        return level[0]?.cid ?? null;
    }
    // ============ Private Link Helpers ============
    cidEquals(a, b) {
        if (a.hash.length !== b.hash.length)
            return false;
        if (!a.hash.every((byte, i) => byte === b.hash[i]))
            return false;
        if (!a.key && !b.key)
            return true;
        if (!a.key || !b.key)
            return false;
        if (a.key.length !== b.key.length)
            return false;
        return a.key.every((byte, i) => byte === b.key[i]);
    }
    async createLeafWithLink(items) {
        return (await this.tree.putDirectory(items.map(([key, cid]) => treeEntry(escapeKey(key), cid, 0, LinkType.File)))).cid;
    }
    async insertLinkRecursive(node, key, targetCid, signal) {
        const entries = await this.tree.listDirectory(node, signal);
        const isLeaf = this.isLeafNode(entries);
        if (isLeaf) {
            return this.insertLinkIntoLeaf(node, entries, key, targetCid);
        }
        return this.insertIntoInternal(node, entries, key, (child) => this.insertLinkRecursive(child, key, targetCid, signal), true);
    }
    async insertLinkIntoLeaf(node, entries, key, targetCid) {
        const escapedKey = escapeKey(key);
        const existing = entries.find((entry) => entry.name === escapedKey);
        if (existing?.type === LinkType.File && this.cidEquals(existing.cid, targetCid)) {
            return { cid: node, unchanged: true };
        }
        const newEntries = this.sortEntries([
            ...entries.filter((entry) => entry.name !== escapedKey),
            treeEntry(escapedKey, targetCid, 0, LinkType.File),
        ]);
        const newNode = (await this.tree.putDirectory(newEntries)).cid;
        if (newEntries.length > this.maxKeys) {
            const split = await this.splitLeafWithLinks(newEntries);
            return {
                cid: newNode,
                count: split.leftCount + split.rightCount,
                split,
            };
        }
        return {
            cid: newNode,
            count: this.countLinkEntries(newEntries),
        };
    }
    async insertIntoInternal(node, entries, key, insert, preserveCounts) {
        const { child } = this.findChild(entries, key);
        const result = await insert(child.cid);
        if (result.unchanged) {
            return { cid: node, unchanged: true };
        }
        const newEntries = entries.filter((entry) => entry.name !== child.name);
        if (result.split) {
            newEntries.push(treeEntry(escapeKey(result.split.leftFirstKey), result.split.left, result.split.leftCount, LinkType.Dir), treeEntry(escapeKey(result.split.rightFirstKey), result.split.right, result.split.rightCount, LinkType.Dir));
        }
        else {
            newEntries.push(treeEntry(child.name, result.cid, preserveCounts ? result.count : 0, LinkType.Dir));
        }
        const sortedEntries = this.sortEntries(newEntries);
        const newNode = (await this.tree.putDirectory(sortedEntries)).cid;
        if (sortedEntries.length > this.maxKeys) {
            const split = await this.splitInternal(sortedEntries, preserveCounts);
            return {
                cid: newNode,
                count: split.leftCount + split.rightCount,
                split,
            };
        }
        return {
            cid: newNode,
            count: preserveCounts ? await this.countLinkEntriesOrSubtrees(sortedEntries) : 0,
        };
    }
    async splitLeafWithLinks(entries) {
        const sorted = this.sortEntries(entries);
        const mid = Math.floor(sorted.length / 2);
        const leftEntries = sorted.slice(0, mid);
        const rightEntries = sorted.slice(mid);
        const left = (await this.tree.putDirectory(leftEntries)).cid;
        const right = (await this.tree.putDirectory(rightEntries)).cid;
        return {
            left,
            right,
            leftFirstKey: unescapeKey(leftEntries[0].name),
            rightFirstKey: unescapeKey(rightEntries[0].name),
            leftCount: this.countLinkEntries(leftEntries),
            rightCount: this.countLinkEntries(rightEntries),
        };
    }
    async *traverseLinksInOrder(node) {
        const entries = await this.tree.listDirectory(node);
        const isLeaf = this.isLeafNode(entries);
        const sorted = this.sortEntries(entries);
        if (isLeaf) {
            for (const entry of sorted) {
                if (entry.type === LinkType.File) {
                    yield [unescapeKey(entry.name), entry.cid];
                }
            }
        }
        else {
            for (const child of sorted) {
                yield* this.traverseLinksInOrder(child.cid);
            }
        }
    }
    async *traverseLinksInOrderVerified(node, expectedCount) {
        const entries = await this.tree.listDirectory(node);
        const isLeaf = this.isLeafNode(entries);
        const sorted = this.sortEntries(entries);
        let yieldedCount = 0;
        if (isLeaf) {
            for (const entry of sorted) {
                if (entry.type === LinkType.File) {
                    yieldedCount += 1;
                    yield [unescapeKey(entry.name), entry.cid];
                }
            }
        }
        else {
            for (const child of sorted) {
                const childExpectedCount = this.storedLinkSubtreeCount(child);
                const childYieldedCount = yield* this.traverseLinksInOrderVerified(child.cid, childExpectedCount);
                if (childExpectedCount !== null && childYieldedCount !== childExpectedCount) {
                    throw new Error(`BTree link subtree ${toHex(child.cid.hash)} yielded `
                        + `${childYieldedCount} links, expected ${childExpectedCount}`);
                }
                yieldedCount += childYieldedCount;
            }
        }
        if (expectedCount !== null && yieldedCount !== expectedCount) {
            throw new Error(`BTree link subtree ${toHex(node.hash)} yielded `
                + `${yieldedCount} links, expected ${expectedCount}`);
        }
        return yieldedCount;
    }
    async *rangeLinkTraverse(node, start, end) {
        const entries = await this.tree.listDirectory(node);
        const isLeaf = this.isLeafNode(entries);
        const sorted = this.sortEntries(entries);
        if (isLeaf) {
            for (const entry of sorted) {
                if (entry.type !== LinkType.File)
                    continue;
                const key = unescapeKey(entry.name);
                if (start !== undefined && compareKeys(key, start) < 0)
                    continue;
                if (end !== undefined && compareKeys(key, end) >= 0)
                    return;
                yield [key, entry.cid];
            }
        }
        else {
            for (let i = 0; i < sorted.length; i++) {
                const child = sorted[i];
                const childMinKey = unescapeKey(child.name);
                const childMaxKey = i < sorted.length - 1 ? unescapeKey(sorted[i + 1].name) : undefined;
                if (start !== undefined && childMaxKey !== undefined && compareKeys(childMaxKey, start) <= 0)
                    continue;
                if (end !== undefined && compareKeys(childMinKey, end) >= 0)
                    return;
                yield* this.rangeLinkTraverse(child.cid, start, end);
            }
        }
    }
    async countLinksRecursive(node, cache) {
        const cacheKey = cidCacheKey(node);
        const cached = cache.counts.get(cacheKey);
        if (cached !== undefined) {
            return cached;
        }
        const entries = await this.listCachedEntries(node, cache);
        const count = this.isLeafNode(entries)
            ? this.countLinkEntries(entries)
            : (await Promise.all(entries.map(async (entry) => {
                const childCount = this.storedLinkSubtreeCount(entry);
                return childCount ?? await this.countLinksRecursive(entry.cid, cache);
            }))).reduce((sum, childCount) => sum + childCount, 0);
        cache.counts.set(cacheKey, count);
        return count;
    }
    countLinkEntries(entries) {
        return entries.filter((entry) => entry.type === LinkType.File).length;
    }
    storedLinkSubtreeCount(entry) {
        if (entry.type !== LinkType.Dir || !Number.isFinite(entry.size) || entry.size <= 0) {
            return null;
        }
        return Math.floor(entry.size);
    }
    async countLinkEntriesOrSubtrees(entries) {
        if (this.isLeafNode(entries)) {
            return this.countLinkEntries(entries);
        }
        const counts = await Promise.all(entries.map(async (entry) => {
            const childCount = this.storedLinkSubtreeCount(entry);
            return childCount ?? await this.countLinksRecursive(entry.cid, createLinkTraversalCache());
        }));
        return counts.reduce((sum, count) => sum + count, 0);
    }
    async getLinkEntryAtRecursive(node, ordinal, cache) {
        const entries = await this.listCachedEntries(node, cache);
        if (this.isLeafNode(entries)) {
            const links = entries.filter((entry) => entry.type === LinkType.File);
            const entry = links[ordinal];
            return entry ? [unescapeKey(entry.name), entry.cid] : null;
        }
        let remaining = ordinal;
        for (const entry of entries) {
            const childCount = this.storedLinkSubtreeCount(entry)
                ?? await this.countLinksRecursive(entry.cid, cache);
            if (remaining < childCount) {
                return await this.getLinkEntryAtRecursive(entry.cid, remaining, cache);
            }
            remaining -= childCount;
        }
        return null;
    }
    async listCachedEntries(node, cache) {
        const cacheKey = cidCacheKey(node);
        const cached = cache.entries.get(cacheKey);
        if (cached) {
            return cached;
        }
        const entries = this.sortEntries(await this.tree.listDirectory(node));
        cache.entries.set(cacheKey, entries);
        return entries;
    }
    // ============ Original Private Methods ============
    async insertRecursive(node, key, value) {
        const entries = await this.tree.listDirectory(node);
        const isLeaf = this.isLeafNode(entries);
        if (isLeaf) {
            return this.insertIntoLeaf(node, entries, key, value);
        }
        return this.insertIntoInternal(node, entries, key, (child) => this.insertRecursive(child, key, value), false);
    }
    async insertIntoLeaf(node, entries, key, value) {
        const escapedKey = escapeKey(key);
        const existing = entries.find((entry) => entry.name === escapedKey);
        if (existing?.type === LinkType.Blob) {
            const data = await this.tree.readFile(existing.cid);
            if (data && textDecoder.decode(data) === value) {
                return { cid: node, unchanged: true };
            }
        }
        const { cid, size } = await this.tree.putFile(textEncoder.encode(value));
        const newEntries = this.sortEntries([
            ...entries.filter((entry) => entry.name !== escapedKey),
            treeEntry(escapedKey, cid, size, LinkType.Blob),
        ]);
        const newNode = (await this.tree.putDirectory(newEntries)).cid;
        if (newEntries.length > this.maxKeys) {
            return { cid: newNode, count: 0, split: await this.splitLeaf(newEntries) };
        }
        return { cid: newNode, count: 0 };
    }
    async splitLeaf(entries) {
        const sorted = this.sortEntries(entries);
        const mid = Math.floor(sorted.length / 2);
        const leftEntries = sorted.slice(0, mid);
        const rightEntries = sorted.slice(mid);
        const left = (await this.tree.putDirectory(leftEntries.map((entry) => ({
            ...entry,
            type: LinkType.Blob,
        })))).cid;
        const right = (await this.tree.putDirectory(rightEntries.map((entry) => ({
            ...entry,
            type: LinkType.Blob,
        })))).cid;
        return {
            left,
            right,
            leftFirstKey: unescapeKey(leftEntries[0].name),
            rightFirstKey: unescapeKey(rightEntries[0].name),
            leftCount: 0,
            rightCount: 0,
        };
    }
    async splitInternal(entries, preserveCounts = false) {
        const sorted = this.sortEntries(entries);
        const mid = Math.floor(sorted.length / 2);
        const leftEntries = sorted.slice(0, mid);
        const rightEntries = sorted.slice(mid);
        const leftCount = preserveCounts ? await this.countLinkEntriesOrSubtrees(leftEntries) : 0;
        const rightCount = preserveCounts ? await this.countLinkEntriesOrSubtrees(rightEntries) : 0;
        const left = (await this.tree.putDirectory(leftEntries.map((entry) => ({
            ...entry,
            size: preserveCounts ? entry.size : 0,
            type: LinkType.Dir,
        })))).cid;
        const right = (await this.tree.putDirectory(rightEntries.map((entry) => ({
            ...entry,
            size: preserveCounts ? entry.size : 0,
            type: LinkType.Dir,
        })))).cid;
        return {
            left,
            right,
            leftFirstKey: unescapeKey(leftEntries[0].name),
            rightFirstKey: unescapeKey(rightEntries[0].name),
            leftCount,
            rightCount,
        };
    }
    findChild(entries, key) {
        const sorted = this.sortEntries(entries);
        for (let i = 0; i < sorted.length - 1; i++) {
            const nextName = unescapeKey(sorted[i + 1].name);
            if (compareKeys(key, nextName) < 0) {
                return { child: sorted[i], childIndex: i };
            }
        }
        return { child: sorted[sorted.length - 1], childIndex: sorted.length - 1 };
    }
    sortEntries(entries) {
        return [...entries].sort((a, b) => compareKeys(unescapeKey(a.name), unescapeKey(b.name)));
    }
    isLeafNode(entries) {
        // Leaf nodes contain values (Blob or File), internal nodes contain only Dir
        return entries.length === 0 || entries.some(e => e.type !== LinkType.Dir);
    }
    async createLeaf(items) {
        const entries = [];
        for (const [key, value] of items) {
            const { cid, size } = await this.tree.putFile(textEncoder.encode(value));
            entries.push(treeEntry(escapeKey(key), cid, size, LinkType.Blob));
        }
        return (await this.tree.putDirectory(entries)).cid;
    }
    async createInternalNode(children) {
        const entries = children.map((child) => treeEntry(escapeKey(child.firstKey), child.cid, child.count, LinkType.Dir));
        return (await this.tree.putDirectory(entries)).cid;
    }
    async delete(root, key) {
        const entries = await this.tree.listDirectory(root);
        const isLeaf = this.isLeafNode(entries);
        if (isLeaf) {
            const escapedKey = escapeKey(key);
            const entry = entries.find(e => e.name === escapedKey);
            if (!entry)
                return root;
            const newRoot = await this.tree.removeEntry(root, [], escapedKey);
            const newEntries = await this.tree.listDirectory(newRoot);
            if (newEntries.length === 0)
                return null;
            return newRoot;
        }
        const { child } = this.findChild(entries, key);
        const newChild = await this.delete(child.cid, key);
        if (!newChild) {
            const newRoot = await this.tree.removeEntry(root, [], child.name);
            const newEntries = await this.tree.listDirectory(newRoot);
            if (newEntries.length === 0)
                return null;
            if (newEntries.length === 1 && newEntries[0].type === LinkType.Dir) {
                return newEntries[0].cid;
            }
            return newRoot;
        }
        if (newChild === child.cid)
            return root;
        return this.tree.setEntry(root, [], child.name, newChild, 0, LinkType.Dir);
    }
    async *entries(root) {
        if (!root)
            return;
        yield* this.traverseInOrder(root);
    }
    async *traverseInOrder(node) {
        const entries = await this.tree.listDirectory(node);
        const isLeaf = this.isLeafNode(entries);
        const sorted = this.sortEntries(entries);
        if (isLeaf) {
            for (const entry of sorted) {
                if (entry.type !== LinkType.Blob)
                    continue;
                const data = await this.tree.readFile(entry.cid);
                if (data) {
                    yield [unescapeKey(entry.name), textDecoder.decode(data)];
                }
            }
        }
        else {
            for (const child of sorted) {
                yield* this.traverseInOrder(child.cid);
            }
        }
    }
    async *range(root, start, end) {
        yield* this.rangeTraverse(root, start, end);
    }
    async *rangeTraverse(node, start, end) {
        const entries = await this.tree.listDirectory(node);
        const isLeaf = this.isLeafNode(entries);
        const sorted = this.sortEntries(entries);
        if (isLeaf) {
            for (const entry of sorted) {
                if (entry.type !== LinkType.Blob)
                    continue;
                const key = unescapeKey(entry.name);
                if (start !== undefined && compareKeys(key, start) < 0)
                    continue;
                if (end !== undefined && compareKeys(key, end) >= 0)
                    return;
                const data = await this.tree.readFile(entry.cid);
                if (data) {
                    yield [key, textDecoder.decode(data)];
                }
            }
        }
        else {
            for (let i = 0; i < sorted.length; i++) {
                const child = sorted[i];
                const childMinKey = unescapeKey(child.name);
                const childMaxKey = i < sorted.length - 1 ? unescapeKey(sorted[i + 1].name) : undefined;
                if (start !== undefined && childMaxKey !== undefined && compareKeys(childMaxKey, start) <= 0)
                    continue;
                if (end !== undefined && compareKeys(childMinKey, end) >= 0)
                    return;
                yield* this.rangeTraverse(child.cid, start, end);
            }
        }
    }
    async *prefix(root, prefix) {
        const endPrefix = incrementPrefix(prefix);
        yield* this.range(root, prefix, endPrefix);
    }
    async merge(base, other, preferOther = false) {
        if (!other)
            return base;
        if (!base)
            return other;
        let result = base;
        for await (const [key, value] of this.entries(other)) {
            const existingValue = await this.get(result, key);
            if (existingValue === null || preferOther) {
                result = await this.insert(result, key, value);
            }
        }
        return result;
    }
}
function treeEntry(name, cid, size, type) {
    return { name, cid, size, type };
}
export function escapeKey(key) {
    return key
        .replace(/%/g, '%25')
        .replace(/\//g, '%2F')
        .replace(/\0/g, '%00');
}
function createLinkTraversalCache() {
    return {
        entries: new Map(),
        counts: new Map(),
    };
}
function cidCacheKey(cid) {
    return `${toHex(cid.hash)}:${cid.key ? toHex(cid.key) : ''}`;
}
function sampleUniqueIntegers(total, limit, random) {
    const effectiveTotal = Number.isFinite(total) ? Math.max(0, Math.floor(total)) : 0;
    const effectiveLimit = Number.isFinite(limit)
        ? Math.min(effectiveTotal, Math.max(0, Math.floor(limit)))
        : effectiveTotal;
    if (effectiveTotal === 0 || effectiveLimit === 0) {
        return [];
    }
    if (effectiveLimit >= effectiveTotal) {
        return shuffleItems(Array.from({ length: effectiveTotal }, (_, index) => index), random);
    }
    const selected = new Set();
    let attempts = 0;
    const maxAttempts = Math.max(effectiveLimit * 8, 32);
    while (selected.size < effectiveLimit && attempts < maxAttempts) {
        selected.add(Math.floor(random() * effectiveTotal));
        attempts += 1;
    }
    if (selected.size < effectiveLimit) {
        const remaining = [];
        for (let index = 0; index < effectiveTotal; index += 1) {
            if (!selected.has(index)) {
                remaining.push(index);
            }
        }
        for (const index of shuffleItems(remaining, random)) {
            selected.add(index);
            if (selected.size >= effectiveLimit) {
                break;
            }
        }
    }
    return shuffleItems([...selected], random);
}
function shuffleItems(items, random) {
    const shuffled = [...items];
    for (let index = shuffled.length - 1; index > 0; index -= 1) {
        const swapIndex = Math.floor(random() * (index + 1));
        [shuffled[index], shuffled[swapIndex]] = [shuffled[swapIndex], shuffled[index]];
    }
    return shuffled;
}
export function unescapeKey(name) {
    return name
        .replace(/%2F/gi, '/')
        .replace(/%00/gi, '\0')
        .replace(/%25/g, '%');
}
function compareKeys(left, right) {
    if (left < right)
        return -1;
    if (left > right)
        return 1;
    return 0;
}
function incrementPrefix(str) {
    if (str.length === 0)
        return str;
    const lastChar = str.charCodeAt(str.length - 1);
    return str.slice(0, -1) + String.fromCharCode(lastChar + 1);
}
//# sourceMappingURL=btree.js.map