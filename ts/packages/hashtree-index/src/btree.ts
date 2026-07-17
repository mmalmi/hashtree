import { HashTree, LinkType, toHex, type CID, type Store, type TreeEntry } from '@hashtree/core';

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder();

export interface BTreeOptions {
  /** Max entries per node before splitting. Default: 32 */
  order?: number;
}

export interface BTreeSampleOptions {
  totalCount?: number;
  random?: () => number;
}

export interface BTreeLinkEntriesOptions {
  verifyCount?: boolean;
}

export interface BTreeOperationOptions {
  signal?: AbortSignal;
}

type LinkTraversalCache = {
  entries: Map<string, TreeEntry[]>;
  counts: Map<string, number>;
};

type BTreeMutationResult =
  | { cid: CID; unchanged: true }
  | { cid: CID; count: number; split?: SplitResult; unchanged?: false };

export class BTree {
  private tree: HashTree;
  private order: number;
  private maxKeys: number;

  constructor(store: Store, options: BTreeOptions = {}) {
    this.tree = new HashTree({ store });
    this.order = options.order ?? 32;
    this.maxKeys = this.order - 1;
  }

  // ============ String Value Methods (existing) ============

  async insert(root: CID | null, key: string, value: string): Promise<CID> {
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

  async get(root: CID | null, key: string): Promise<string | null> {
    if (!root) return null;

    const entries = await this.tree.listDirectory(root);
    const isLeaf = this.isLeafNode(entries);

    if (isLeaf) {
      const escapedKey = escapeKey(key);
      const entry = entries.find(e => e.name === escapedKey);
      if (!entry || entry.type !== LinkType.Blob) return null;

      const data = await this.tree.readFile(entry.cid);
      if (!data) return null;
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
  async insertLink(
    root: CID | null,
    key: string,
    targetCid: CID,
    options: BTreeOperationOptions = {},
  ): Promise<CID> {
    if (!root) {
      return this.createLeafWithLink([[key, targetCid]]);
    }

    const result = await this.insertLinkRecursive(root, key, targetCid, options.signal);
    if (result.unchanged) {
      return root;
    }

    if (result.split) {
      return (await this.tree.putDirectory([
        treeEntry(
          escapeKey(result.split.leftFirstKey),
          result.split.left,
          result.split.leftCount,
          LinkType.Dir,
        ),
        treeEntry(
          escapeKey(result.split.rightFirstKey),
          result.split.right,
          result.split.rightCount,
          LinkType.Dir,
        ),
      ])).cid;
    }

    return result.cid;
  }

  /**
   * Get a CID link from the tree.
   */
  async getLink(
    root: CID | null,
    key: string,
    options: BTreeOperationOptions = {},
  ): Promise<CID | null> {
    if (!root) return null;

    const entries = await this.tree.listDirectory(root, options.signal);
    const isLeaf = this.isLeafNode(entries);

    if (isLeaf) {
      const escapedKey = escapeKey(key);
      const entry = entries.find(e => e.name === escapedKey);
      if (!entry || entry.type !== LinkType.File) return null;
      return entry.cid;
    }

    const { child } = this.findChild(entries, key);
    return this.getLink(child.cid, key, options);
  }

  /**
   * Iterate all CID links in the tree.
   */
  async *linksEntries(
    root: CID | null,
    options: BTreeLinkEntriesOptions = {}
  ): AsyncGenerator<[string, CID]> {
    if (!root) return;
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
  async *verifiedLinksEntries(root: CID | null): AsyncGenerator<[string, CID]> {
    if (!root) return;
    const expectedCount = await this.countReportedLinks(root);
    const yieldedCount = yield* this.traverseLinksInOrderVerified(root, expectedCount);
    if (expectedCount !== null && yieldedCount !== expectedCount) {
      throw new Error(
        `BTree link traversal yielded ${yieldedCount} links, expected ${expectedCount}`,
      );
    }
  }

  /**
   * Prefix search for CID links.
   */
  async *prefixLinks(root: CID, prefix: string): AsyncGenerator<[string, CID]> {
    const endPrefix = incrementPrefix(prefix);
    yield* this.rangeLinkTraverse(root, prefix, endPrefix);
  }

  /**
   * Count CID links by walking the tree.
   * Uses stored subtree sizes when available, but may scan descendants when
   * older roots do not carry complete counts.
   */
  async countLinks(root: CID | null): Promise<number> {
    return await this.scanLinks(root);
  }

  /**
   * Count CID links by walking the tree.
   */
  async scanLinks(root: CID | null): Promise<number> {
    if (!root) {
      return 0;
    }

    return await this.countLinksRecursive(root, createLinkTraversalCache());
  }

  /**
   * Explicit count-scan alias for callers that need to make scan semantics
   * clear at the call site.
   */
  async scanLinkCount(root: CID | null): Promise<number> {
    return await this.scanLinks(root);
  }

  /**
   * Read the stored CID-link count from the root node without scanning.
   * Returns null when the root was built by older code that does not store
   * complete subtree sizes.
   */
  async countStoredLinks(root: CID | null): Promise<number | null> {
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
  async countReportedLinks(root: CID | null): Promise<number | null> {
    return await this.countStoredLinks(root);
  }

  /**
   * Read the Nth CID link in sorted key order.
   */
  async getLinkEntryAt(root: CID | null, ordinal: number): Promise<[string, CID] | null> {
    if (!root || ordinal < 0) {
      return null;
    }

    return await this.getLinkEntryAtRecursive(root, Math.floor(ordinal), createLinkTraversalCache());
  }

  /**
   * Sample CID links uniformly by random ordinal.
   */
  async sampleLinks(root: CID | null, limit: number, options: BTreeSampleOptions = {}): Promise<Array<[string, CID]>> {
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
    const results: Array<[string, CID]> = [];

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
  async mergeLinks(
    base: CID | null,
    other: CID | null,
    preferOther = false
  ): Promise<CID | null> {
    if (!other) return base;
    if (!base) return other;

    let result = base;

    for await (const [key, cid] of this.linksEntries(other)) {
      const existingCid = await this.getLink(result, key);

      if (existingCid === null || preferOther) {
        result = await this.insertLink(result, key, cid);
      }
    }

    return result;
  }

  async build(items: Iterable<[string, string]>): Promise<CID | null> {
    return this.buildTree(items, (chunk) => this.createLeaf(chunk), false);
  }

  async buildLinks(items: Iterable<[string, CID]>): Promise<CID | null> {
    return this.buildTree(items, (chunk) => this.createLeafWithLink(chunk), true);
  }

  /**
   * Bulk-build from strictly increasing, unique entries without retaining the
   * complete input. Each tree level buffers at most one node beyond maxKeys.
   */
  async buildSortedAsync(
    items: AsyncIterable<[string, string]> | Iterable<[string, string]>,
  ): Promise<CID | null> {
    const levels: BuiltNode[][] = [];
    const leafEntries: Array<[string, string]> = [];

    const appendNode = async (level: number, node: BuiltNode): Promise<void> => {
      const nodes = levels[level] ?? [];
      levels[level] = nodes;
      nodes.push(node);
      if (nodes.length <= this.maxKeys) return;

      const children = nodes.splice(0, this.maxKeys);
      await appendNode(level + 1, {
        firstKey: children[0].firstKey,
        cid: await this.createInternalNode(children),
        count: children.reduce((sum, child) => sum + child.count, 0),
      });
    };

    const flushLeaf = async (): Promise<void> => {
      if (leafEntries.length === 0) return;
      const entries = leafEntries.splice(0, this.maxKeys);
      await appendNode(0, {
        firstKey: entries[0][0],
        cid: await this.createLeaf(entries),
        count: 0,
      });
    };

    let previousKey: string | undefined;
    for await (const entry of items) {
      const [key] = entry;
      if (previousKey !== undefined && compareKeys(previousKey, key) >= 0) {
        throw new Error('Sorted BTree entries must have strictly increasing unique keys');
      }
      previousKey = key;
      leafEntries.push(entry);
      if (leafEntries.length > this.maxKeys) {
        await flushLeaf();
      }
    }
    await flushLeaf();

    for (let level = 0; level < levels.length; level += 1) {
      const nodes = levels[level];
      if (!nodes || nodes.length === 0) continue;
      const hasHigherNodes = levels.slice(level + 1).some((higher) => higher.length > 0);
      if (!hasHigherNodes) {
        if (nodes.length === 1) return nodes[0].cid;
        return await this.createInternalNode(nodes);
      }

      const children = nodes.splice(0);
      await appendNode(level + 1, {
        firstKey: children[0].firstKey,
        cid: await this.createInternalNode(children),
        count: children.reduce((sum, child) => sum + child.count, 0),
      });
    }

    return null;
  }

  private async buildTree<T>(
    items: Iterable<[string, T]>,
    createLeaf: (items: Array<[string, T]>) => Promise<CID>,
    preserveCounts: boolean,
  ): Promise<CID | null> {
    const sorted = [...items];
    if (sorted.length === 0) {
      return null;
    }

    sorted.sort((left, right) => compareKeys(left[0], right[0]));

    const deduped: Array<[string, T]> = [];
    for (const [key, value] of sorted) {
      const last = deduped[deduped.length - 1];
      if (last && last[0] === key) {
        last[1] = value;
        continue;
      }
      deduped.push([key, value]);
    }

    let level: BuiltNode[] = [];
    for (let index = 0; index < deduped.length; index += this.maxKeys) {
      const chunk = deduped.slice(index, index + this.maxKeys);
      level.push({
        firstKey: chunk[0][0],
        cid: await createLeaf(chunk),
        count: preserveCounts ? chunk.length : 0,
      });
    }

    while (level.length > 1) {
      const nextLevel: BuiltNode[] = [];
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

  private cidEquals(a: CID, b: CID): boolean {
    if (a.hash.length !== b.hash.length) return false;
    if (!a.hash.every((byte, i) => byte === b.hash[i])) return false;
    if (!a.key && !b.key) return true;
    if (!a.key || !b.key) return false;
    if (a.key.length !== b.key.length) return false;
    return a.key.every((byte, i) => byte === b.key![i]);
  }

  private async createLeafWithLink(items: Array<[string, CID]>): Promise<CID> {
    return (await this.tree.putDirectory(items.map(([key, cid]) =>
      treeEntry(escapeKey(key), cid, 0, LinkType.File)
    ))).cid;
  }

  private async insertLinkRecursive(
    node: CID,
    key: string,
    targetCid: CID,
    signal?: AbortSignal,
  ): Promise<BTreeMutationResult> {
    const entries = await this.tree.listDirectory(node, signal);
    const isLeaf = this.isLeafNode(entries);

    if (isLeaf) {
      return this.insertLinkIntoLeaf(node, entries, key, targetCid);
    }
    return this.insertIntoInternal(
      node,
      entries,
      key,
      (child) => this.insertLinkRecursive(child, key, targetCid, signal),
      true,
    );
  }

  private async insertLinkIntoLeaf(
    node: CID,
    entries: TreeEntry[],
    key: string,
    targetCid: CID
  ): Promise<BTreeMutationResult> {
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

  private async insertIntoInternal(
    node: CID,
    entries: TreeEntry[],
    key: string,
    insert: (child: CID) => Promise<BTreeMutationResult>,
    preserveCounts: boolean,
  ): Promise<BTreeMutationResult> {
    const { child } = this.findChild(entries, key);
    const result = await insert(child.cid);
    if (result.unchanged) {
      return { cid: node, unchanged: true };
    }

    const newEntries = entries.filter((entry) => entry.name !== child.name);
    if (result.split) {
      newEntries.push(
        treeEntry(
          escapeKey(result.split.leftFirstKey),
          result.split.left,
          result.split.leftCount,
          LinkType.Dir,
        ),
        treeEntry(
          escapeKey(result.split.rightFirstKey),
          result.split.right,
          result.split.rightCount,
          LinkType.Dir,
        ),
      );
    } else {
      newEntries.push(treeEntry(
        child.name,
        result.cid,
        preserveCounts ? result.count : 0,
        LinkType.Dir,
      ));
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

  private async splitLeafWithLinks(entries: TreeEntry[]): Promise<SplitResult> {
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

  private async *traverseLinksInOrder(node: CID): AsyncGenerator<[string, CID]> {
    const entries = await this.tree.listDirectory(node);
    const isLeaf = this.isLeafNode(entries);
    const sorted = this.sortEntries(entries);

    if (isLeaf) {
      for (const entry of sorted) {
        if (entry.type === LinkType.File) {
          yield [unescapeKey(entry.name), entry.cid];
        }
      }
    } else {
      for (const child of sorted) {
        yield* this.traverseLinksInOrder(child.cid);
      }
    }
  }

  private async *traverseLinksInOrderVerified(
    node: CID,
    expectedCount: number | null
  ): AsyncGenerator<[string, CID], number, undefined> {
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
    } else {
      for (const child of sorted) {
        const childExpectedCount = this.storedLinkSubtreeCount(child);
        const childYieldedCount = yield* this.traverseLinksInOrderVerified(
          child.cid,
          childExpectedCount,
        );
        if (childExpectedCount !== null && childYieldedCount !== childExpectedCount) {
          throw new Error(
            `BTree link subtree ${toHex(child.cid.hash)} yielded `
            + `${childYieldedCount} links, expected ${childExpectedCount}`,
          );
        }
        yieldedCount += childYieldedCount;
      }
    }

    if (expectedCount !== null && yieldedCount !== expectedCount) {
      throw new Error(
        `BTree link subtree ${toHex(node.hash)} yielded `
        + `${yieldedCount} links, expected ${expectedCount}`,
      );
    }

    return yieldedCount;
  }

  private async *rangeLinkTraverse(
    node: CID,
    start?: string,
    end?: string
  ): AsyncGenerator<[string, CID]> {
    const entries = await this.tree.listDirectory(node);
    const isLeaf = this.isLeafNode(entries);
    const sorted = this.sortEntries(entries);

    if (isLeaf) {
      for (const entry of sorted) {
        if (entry.type !== LinkType.File) continue;
        const key = unescapeKey(entry.name);
        if (start !== undefined && compareKeys(key, start) < 0) continue;
        if (end !== undefined && compareKeys(key, end) >= 0) return;
        yield [key, entry.cid];
      }
    } else {
      for (let i = 0; i < sorted.length; i++) {
        const child = sorted[i];
        const childMinKey = unescapeKey(child.name);
        const childMaxKey = i < sorted.length - 1 ? unescapeKey(sorted[i + 1].name) : undefined;

        if (start !== undefined && childMaxKey !== undefined && compareKeys(childMaxKey, start) <= 0) continue;
        if (end !== undefined && compareKeys(childMinKey, end) >= 0) return;

        yield* this.rangeLinkTraverse(child.cid, start, end);
      }
    }
  }

  private async countLinksRecursive(node: CID, cache: LinkTraversalCache): Promise<number> {
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

  private countLinkEntries(entries: TreeEntry[]): number {
    return entries.filter((entry) => entry.type === LinkType.File).length;
  }

  private storedLinkSubtreeCount(entry: TreeEntry): number | null {
    if (entry.type !== LinkType.Dir || !Number.isFinite(entry.size) || entry.size <= 0) {
      return null;
    }
    return Math.floor(entry.size);
  }

  private async countLinkEntriesOrSubtrees(entries: TreeEntry[]): Promise<number> {
    if (this.isLeafNode(entries)) {
      return this.countLinkEntries(entries);
    }
    const counts = await Promise.all(entries.map(async (entry) => {
      const childCount = this.storedLinkSubtreeCount(entry);
      return childCount ?? await this.countLinksRecursive(entry.cid, createLinkTraversalCache());
    }));
    return counts.reduce((sum, count) => sum + count, 0);
  }

  private async getLinkEntryAtRecursive(
    node: CID,
    ordinal: number,
    cache: LinkTraversalCache
  ): Promise<[string, CID] | null> {
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

  private async listCachedEntries(node: CID, cache: LinkTraversalCache): Promise<TreeEntry[]> {
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

  private async insertRecursive(
    node: CID,
    key: string,
    value: string
  ): Promise<BTreeMutationResult> {
    const entries = await this.tree.listDirectory(node);
    const isLeaf = this.isLeafNode(entries);

    if (isLeaf) {
      return this.insertIntoLeaf(node, entries, key, value);
    }
    return this.insertIntoInternal(
      node,
      entries,
      key,
      (child) => this.insertRecursive(child, key, value),
      false,
    );
  }

  private async insertIntoLeaf(
    node: CID,
    entries: TreeEntry[],
    key: string,
    value: string
  ): Promise<BTreeMutationResult> {
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

  private async splitLeaf(entries: TreeEntry[]): Promise<SplitResult> {
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

  private async splitInternal(entries: TreeEntry[], preserveCounts = false): Promise<SplitResult> {
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

  private findChild(entries: TreeEntry[], key: string): { child: TreeEntry; childIndex: number } {
    const sorted = this.sortEntries(entries);

    for (let i = 0; i < sorted.length - 1; i++) {
      const nextName = unescapeKey(sorted[i + 1].name);
      if (compareKeys(key, nextName) < 0) {
        return { child: sorted[i], childIndex: i };
      }
    }

    return { child: sorted[sorted.length - 1], childIndex: sorted.length - 1 };
  }

  private sortEntries(entries: TreeEntry[]): TreeEntry[] {
    return [...entries].sort((a, b) =>
      compareKeys(unescapeKey(a.name), unescapeKey(b.name))
    );
  }

  private isLeafNode(entries: TreeEntry[]): boolean {
    // Leaf nodes contain values (Blob or File), internal nodes contain only Dir
    return entries.length === 0 || entries.some(e => e.type !== LinkType.Dir);
  }

  private async createLeaf(items: Array<[string, string]>): Promise<CID> {
    const entries: TreeEntry[] = [];
    for (const [key, value] of items) {
      const { cid, size } = await this.tree.putFile(textEncoder.encode(value));
      entries.push(treeEntry(escapeKey(key), cid, size, LinkType.Blob));
    }
    return (await this.tree.putDirectory(entries)).cid;
  }

  private async createInternalNode(children: BuiltNode[]): Promise<CID> {
    const entries = children.map((child) =>
      treeEntry(escapeKey(child.firstKey), child.cid, child.count, LinkType.Dir)
    );
    return (await this.tree.putDirectory(entries)).cid;
  }

  async delete(root: CID, key: string): Promise<CID | null> {
    const entries = await this.tree.listDirectory(root);
    const isLeaf = this.isLeafNode(entries);

    if (isLeaf) {
      const escapedKey = escapeKey(key);
      const entry = entries.find(e => e.name === escapedKey);
      if (!entry) return root;

      const newRoot = await this.tree.removeEntry(root, [], escapedKey);
      const newEntries = await this.tree.listDirectory(newRoot);
      if (newEntries.length === 0) return null;

      return newRoot;
    }

    const { child } = this.findChild(entries, key);
    const newChild = await this.delete(child.cid, key);

    if (!newChild) {
      const newRoot = await this.tree.removeEntry(root, [], child.name);
      const newEntries = await this.tree.listDirectory(newRoot);

      if (newEntries.length === 0) return null;
      if (newEntries.length === 1 && newEntries[0].type === LinkType.Dir) {
        return newEntries[0].cid;
      }
      return newRoot;
    }

    if (newChild === child.cid) return root;

    return this.tree.setEntry(root, [], child.name, newChild, 0, LinkType.Dir);
  }

  async *entries(root: CID | null): AsyncGenerator<[string, string]> {
    if (!root) return;
    yield* this.traverseInOrder(root);
  }

  private async *traverseInOrder(node: CID): AsyncGenerator<[string, string]> {
    const entries = await this.tree.listDirectory(node);
    const isLeaf = this.isLeafNode(entries);
    const sorted = this.sortEntries(entries);

    if (isLeaf) {
      for (const entry of sorted) {
        if (entry.type !== LinkType.Blob) continue;
        const data = await this.tree.readFile(entry.cid);
        if (data) {
          yield [unescapeKey(entry.name), textDecoder.decode(data)];
        }
      }
    } else {
      for (const child of sorted) {
        yield* this.traverseInOrder(child.cid);
      }
    }
  }

  async *range(root: CID, start?: string, end?: string): AsyncGenerator<[string, string]> {
    yield* this.rangeTraverse(root, start, end);
  }

  private async *rangeTraverse(
    node: CID,
    start?: string,
    end?: string
  ): AsyncGenerator<[string, string]> {
    const entries = await this.tree.listDirectory(node);
    const isLeaf = this.isLeafNode(entries);
    const sorted = this.sortEntries(entries);

    if (isLeaf) {
      for (const entry of sorted) {
        if (entry.type !== LinkType.Blob) continue;
        const key = unescapeKey(entry.name);
        if (start !== undefined && compareKeys(key, start) < 0) continue;
        if (end !== undefined && compareKeys(key, end) >= 0) return;

        const data = await this.tree.readFile(entry.cid);
        if (data) {
          yield [key, textDecoder.decode(data)];
        }
      }
    } else {
      for (let i = 0; i < sorted.length; i++) {
        const child = sorted[i];
        const childMinKey = unescapeKey(child.name);
        const childMaxKey = i < sorted.length - 1 ? unescapeKey(sorted[i + 1].name) : undefined;

        if (start !== undefined && childMaxKey !== undefined && compareKeys(childMaxKey, start) <= 0) continue;
        if (end !== undefined && compareKeys(childMinKey, end) >= 0) return;

        yield* this.rangeTraverse(child.cid, start, end);
      }
    }
  }

  async *prefix(root: CID, prefix: string): AsyncGenerator<[string, string]> {
    const endPrefix = incrementPrefix(prefix);
    yield* this.range(root, prefix, endPrefix);
  }

  async merge(
    base: CID | null,
    other: CID | null,
    preferOther = false
  ): Promise<CID | null> {
    if (!other) return base;
    if (!base) return other;

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

interface SplitResult {
  left: CID;
  right: CID;
  leftFirstKey: string;
  rightFirstKey: string;
  leftCount: number;
  rightCount: number;
}

interface BuiltNode {
  firstKey: string;
  cid: CID;
  count: number;
}

function treeEntry(name: string, cid: CID, size: number, type: LinkType): TreeEntry {
  return { name, cid, size, type };
}

export function escapeKey(key: string): string {
  return key
    .replace(/%/g, '%25')
    .replace(/\//g, '%2F')
    .replace(/\0/g, '%00');
}

function createLinkTraversalCache(): LinkTraversalCache {
  return {
    entries: new Map(),
    counts: new Map(),
  };
}

function cidCacheKey(cid: CID): string {
  return `${toHex(cid.hash)}:${cid.key ? toHex(cid.key) : ''}`;
}

function sampleUniqueIntegers(total: number, limit: number, random: () => number): number[] {
  const effectiveTotal = Number.isFinite(total) ? Math.max(0, Math.floor(total)) : 0;
  const effectiveLimit = Number.isFinite(limit)
    ? Math.min(effectiveTotal, Math.max(0, Math.floor(limit)))
    : effectiveTotal;

  if (effectiveTotal === 0 || effectiveLimit === 0) {
    return [];
  }

  if (effectiveLimit >= effectiveTotal) {
    return shuffleItems(
      Array.from({ length: effectiveTotal }, (_, index) => index),
      random,
    );
  }

  const selected = new Set<number>();
  let attempts = 0;
  const maxAttempts = Math.max(effectiveLimit * 8, 32);

  while (selected.size < effectiveLimit && attempts < maxAttempts) {
    selected.add(Math.floor(random() * effectiveTotal));
    attempts += 1;
  }

  if (selected.size < effectiveLimit) {
    const remaining: number[] = [];
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

function shuffleItems<T>(items: T[], random: () => number): T[] {
  const shuffled = [...items];
  for (let index = shuffled.length - 1; index > 0; index -= 1) {
    const swapIndex = Math.floor(random() * (index + 1));
    [shuffled[index], shuffled[swapIndex]] = [shuffled[swapIndex], shuffled[index]];
  }
  return shuffled;
}

export function unescapeKey(name: string): string {
  return name
    .replace(/%2F/gi, '/')
    .replace(/%00/gi, '\0')
    .replace(/%25/g, '%');
}

function compareKeys(left: string, right: string): number {
  if (left < right) return -1;
  if (left > right) return 1;
  return 0;
}

function incrementPrefix(str: string): string {
  if (str.length === 0) return str;
  const lastChar = str.charCodeAt(str.length - 1);
  return str.slice(0, -1) + String.fromCharCode(lastChar + 1);
}
