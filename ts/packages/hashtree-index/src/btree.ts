import { HashTree, LinkType, type CID, type Store, type TreeEntry } from 'hashtree';

export interface BTreeOptions {
  /** Max entries per node before splitting. Default: 32 */
  order?: number;
}

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

    // Early exit: if key already exists with same value, return unchanged
    const existingValue = await this.get(root, key);
    if (existingValue === value) {
      return root;
    }

    const result = await this.insertRecursive(root, key, value);

    if (result.split) {
      let newRoot = (await this.tree.putDirectory([])).cid;
      newRoot = await this.tree.setEntry(newRoot, [], escapeKey(result.split.leftFirstKey), result.split.left, 0, LinkType.Dir);
      newRoot = await this.tree.setEntry(newRoot, [], escapeKey(result.split.rightFirstKey), result.split.right, 0, LinkType.Dir);
      return newRoot;
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
      return new TextDecoder().decode(data);
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
  async insertLink(root: CID | null, key: string, targetCid: CID): Promise<CID> {
    if (!root) {
      return this.createLeafWithLink([[key, targetCid]]);
    }

    // Early exit: if key already exists with same CID, return unchanged
    const existingCid = await this.getLink(root, key);
    if (existingCid && this.cidEquals(existingCid, targetCid)) {
      return root;
    }

    const result = await this.insertLinkRecursive(root, key, targetCid);

    if (result.split) {
      let newRoot = (await this.tree.putDirectory([])).cid;
      newRoot = await this.tree.setEntry(newRoot, [], escapeKey(result.split.leftFirstKey), result.split.left, 0, LinkType.Dir);
      newRoot = await this.tree.setEntry(newRoot, [], escapeKey(result.split.rightFirstKey), result.split.right, 0, LinkType.Dir);
      return newRoot;
    }

    return result.cid;
  }

  /**
   * Get a CID link from the tree.
   */
  async getLink(root: CID | null, key: string): Promise<CID | null> {
    if (!root) return null;

    const entries = await this.tree.listDirectory(root);
    const isLeaf = this.isLeafNode(entries);

    if (isLeaf) {
      const escapedKey = escapeKey(key);
      const entry = entries.find(e => e.name === escapedKey);
      if (!entry || entry.type !== LinkType.File) return null;
      return entry.cid;
    }

    const { child } = this.findChild(entries, key);
    return this.getLink(child.cid, key);
  }

  /**
   * Iterate all CID links in the tree.
   */
  async *linksEntries(root: CID | null): AsyncGenerator<[string, CID]> {
    if (!root) return;
    yield* this.traverseLinksInOrder(root);
  }

  /**
   * Prefix search for CID links.
   */
  async *prefixLinks(root: CID, prefix: string): AsyncGenerator<[string, CID]> {
    const endPrefix = incrementPrefix(prefix);
    yield* this.rangeLinkTraverse(root, prefix, endPrefix);
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
    let node = (await this.tree.putDirectory([])).cid;

    for (const [key, targetCid] of items) {
      node = await this.tree.setEntry(node, [], escapeKey(key), targetCid, 0, LinkType.File);
    }

    return node;
  }

  private async insertLinkRecursive(
    node: CID,
    key: string,
    targetCid: CID
  ): Promise<{ cid: CID; split?: SplitResult }> {
    const entries = await this.tree.listDirectory(node);
    const isLeaf = this.isLeafNode(entries);

    if (isLeaf) {
      return this.insertLinkIntoLeaf(node, entries, key, targetCid);
    } else {
      return this.insertLinkIntoInternal(node, entries, key, targetCid);
    }
  }

  private async insertLinkIntoLeaf(
    node: CID,
    entries: TreeEntry[],
    key: string,
    targetCid: CID
  ): Promise<{ cid: CID; split?: SplitResult }> {
    const escapedKey = escapeKey(key);
    const newNode = await this.tree.setEntry(node, [], escapedKey, targetCid, 0, LinkType.File);

    const newEntries = await this.tree.listDirectory(newNode);
    if (newEntries.length > this.maxKeys) {
      return { cid: newNode, split: await this.splitLeafWithLinks(newEntries) };
    }

    return { cid: newNode };
  }

  private async insertLinkIntoInternal(
    node: CID,
    entries: TreeEntry[],
    key: string,
    targetCid: CID
  ): Promise<{ cid: CID; split?: SplitResult }> {
    const { child } = this.findChild(entries, key);
    const result = await this.insertLinkRecursive(child.cid, key, targetCid);

    let newNode = await this.tree.setEntry(node, [], child.name, result.cid, 0, LinkType.Dir);

    if (result.split) {
      newNode = await this.tree.removeEntry(newNode, [], child.name);
      newNode = await this.tree.setEntry(newNode, [], escapeKey(result.split.leftFirstKey), result.split.left, 0, LinkType.Dir);
      newNode = await this.tree.setEntry(newNode, [], escapeKey(result.split.rightFirstKey), result.split.right, 0, LinkType.Dir);
    }

    const newEntries = await this.tree.listDirectory(newNode);
    if (newEntries.length > this.maxKeys) {
      return { cid: newNode, split: await this.splitInternal(newEntries) };
    }

    return { cid: newNode };
  }

  private async splitLeafWithLinks(entries: TreeEntry[]): Promise<SplitResult> {
    const sorted = this.sortEntries(entries);
    const mid = Math.floor(sorted.length / 2);
    const leftEntries = sorted.slice(0, mid);
    const rightEntries = sorted.slice(mid);

    let left = (await this.tree.putDirectory([])).cid;
    for (const entry of leftEntries) {
      left = await this.tree.setEntry(left, [], entry.name, entry.cid, entry.size, entry.type);
    }

    let right = (await this.tree.putDirectory([])).cid;
    for (const entry of rightEntries) {
      right = await this.tree.setEntry(right, [], entry.name, entry.cid, entry.size, entry.type);
    }

    return {
      left,
      right,
      leftFirstKey: unescapeKey(leftEntries[0].name),
      rightFirstKey: unescapeKey(rightEntries[0].name),
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
        if (start !== undefined && key < start) continue;
        if (end !== undefined && key >= end) return;
        yield [key, entry.cid];
      }
    } else {
      for (let i = 0; i < sorted.length; i++) {
        const child = sorted[i];
        const childMinKey = unescapeKey(child.name);
        const childMaxKey = i < sorted.length - 1 ? unescapeKey(sorted[i + 1].name) : undefined;

        if (start !== undefined && childMaxKey !== undefined && childMaxKey <= start) continue;
        if (end !== undefined && childMinKey >= end) return;

        yield* this.rangeLinkTraverse(child.cid, start, end);
      }
    }
  }

  // ============ Original Private Methods ============

  private async insertRecursive(
    node: CID,
    key: string,
    value: string
  ): Promise<{ cid: CID; split?: SplitResult }> {
    const entries = await this.tree.listDirectory(node);
    const isLeaf = this.isLeafNode(entries);

    if (isLeaf) {
      return this.insertIntoLeaf(node, entries, key, value);
    } else {
      return this.insertIntoInternal(node, entries, key, value);
    }
  }

  private async insertIntoLeaf(
    node: CID,
    entries: TreeEntry[],
    key: string,
    value: string
  ): Promise<{ cid: CID; split?: SplitResult }> {
    const escapedKey = escapeKey(key);
    const { cid: valueCid, size } = await this.tree.putFile(new TextEncoder().encode(value));
    const newNode = await this.tree.setEntry(node, [], escapedKey, valueCid, size, LinkType.Blob);

    const newEntries = await this.tree.listDirectory(newNode);
    if (newEntries.length > this.maxKeys) {
      return { cid: newNode, split: await this.splitLeaf(newEntries) };
    }

    return { cid: newNode };
  }

  private async insertIntoInternal(
    node: CID,
    entries: TreeEntry[],
    key: string,
    value: string
  ): Promise<{ cid: CID; split?: SplitResult }> {
    const { child } = this.findChild(entries, key);
    const result = await this.insertRecursive(child.cid, key, value);

    let newNode = await this.tree.setEntry(node, [], child.name, result.cid, 0, LinkType.Dir);

    if (result.split) {
      newNode = await this.tree.removeEntry(newNode, [], child.name);
      newNode = await this.tree.setEntry(newNode, [], escapeKey(result.split.leftFirstKey), result.split.left, 0, LinkType.Dir);
      newNode = await this.tree.setEntry(newNode, [], escapeKey(result.split.rightFirstKey), result.split.right, 0, LinkType.Dir);
    }

    const newEntries = await this.tree.listDirectory(newNode);
    if (newEntries.length > this.maxKeys) {
      return { cid: newNode, split: await this.splitInternal(newEntries) };
    }

    return { cid: newNode };
  }

  private async splitLeaf(entries: TreeEntry[]): Promise<SplitResult> {
    const sorted = this.sortEntries(entries);
    const mid = Math.floor(sorted.length / 2);
    const leftEntries = sorted.slice(0, mid);
    const rightEntries = sorted.slice(mid);

    let left = (await this.tree.putDirectory([])).cid;
    for (const entry of leftEntries) {
      left = await this.tree.setEntry(left, [], entry.name, entry.cid, entry.size, LinkType.Blob);
    }

    let right = (await this.tree.putDirectory([])).cid;
    for (const entry of rightEntries) {
      right = await this.tree.setEntry(right, [], entry.name, entry.cid, entry.size, LinkType.Blob);
    }

    return {
      left,
      right,
      leftFirstKey: unescapeKey(leftEntries[0].name),
      rightFirstKey: unescapeKey(rightEntries[0].name),
    };
  }

  private async splitInternal(entries: TreeEntry[]): Promise<SplitResult> {
    const sorted = this.sortEntries(entries);
    const mid = Math.floor(sorted.length / 2);
    const leftEntries = sorted.slice(0, mid);
    const rightEntries = sorted.slice(mid);

    let left = (await this.tree.putDirectory([])).cid;
    for (const entry of leftEntries) {
      left = await this.tree.setEntry(left, [], entry.name, entry.cid, 0, LinkType.Dir);
    }

    let right = (await this.tree.putDirectory([])).cid;
    for (const entry of rightEntries) {
      right = await this.tree.setEntry(right, [], entry.name, entry.cid, 0, LinkType.Dir);
    }

    return {
      left,
      right,
      leftFirstKey: unescapeKey(leftEntries[0].name),
      rightFirstKey: unescapeKey(rightEntries[0].name),
    };
  }

  private findChild(entries: TreeEntry[], key: string): { child: TreeEntry; childIndex: number } {
    const sorted = this.sortEntries(entries);

    for (let i = 0; i < sorted.length - 1; i++) {
      const nextName = unescapeKey(sorted[i + 1].name);
      if (key < nextName) {
        return { child: sorted[i], childIndex: i };
      }
    }

    return { child: sorted[sorted.length - 1], childIndex: sorted.length - 1 };
  }

  private sortEntries(entries: TreeEntry[]): TreeEntry[] {
    return [...entries].sort((a, b) =>
      unescapeKey(a.name).localeCompare(unescapeKey(b.name))
    );
  }

  private isLeafNode(entries: TreeEntry[]): boolean {
    // Leaf nodes contain values (Blob or File), internal nodes contain only Dir
    return entries.length === 0 || entries.some(e => e.type !== LinkType.Dir);
  }

  private async createLeaf(items: Array<[string, string]>): Promise<CID> {
    let node = (await this.tree.putDirectory([])).cid;

    for (const [key, value] of items) {
      const { cid: valueCid, size } = await this.tree.putFile(new TextEncoder().encode(value));
      node = await this.tree.setEntry(node, [], escapeKey(key), valueCid, size, LinkType.Blob);
    }

    return node;
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
          yield [unescapeKey(entry.name), new TextDecoder().decode(data)];
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
        if (start !== undefined && key < start) continue;
        if (end !== undefined && key >= end) return;

        const data = await this.tree.readFile(entry.cid);
        if (data) {
          yield [key, new TextDecoder().decode(data)];
        }
      }
    } else {
      for (let i = 0; i < sorted.length; i++) {
        const child = sorted[i];
        const childMinKey = unescapeKey(child.name);
        const childMaxKey = i < sorted.length - 1 ? unescapeKey(sorted[i + 1].name) : undefined;

        if (start !== undefined && childMaxKey !== undefined && childMaxKey <= start) continue;
        if (end !== undefined && childMinKey >= end) return;

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
}

export function escapeKey(key: string): string {
  return key
    .replace(/%/g, '%25')
    .replace(/\//g, '%2F')
    .replace(/\0/g, '%00');
}

export function unescapeKey(name: string): string {
  return name
    .replace(/%2F/gi, '/')
    .replace(/%00/gi, '\0')
    .replace(/%25/g, '%');
}

function incrementPrefix(str: string): string {
  if (str.length === 0) return str;
  const lastChar = str.charCodeAt(str.length - 1);
  return str.slice(0, -1) + String.fromCharCode(lastChar + 1);
}
