import { type CID, type Store } from '@hashtree/core';
export interface BTreeOptions {
    /** Max entries per node before splitting. Default: 32 */
    order?: number;
}
export interface BTreeSampleOptions {
    totalCount?: number;
    random?: () => number;
}
export declare class BTree {
    private tree;
    private order;
    private maxKeys;
    constructor(store: Store, options?: BTreeOptions);
    insert(root: CID | null, key: string, value: string): Promise<CID>;
    get(root: CID | null, key: string): Promise<string | null>;
    /**
     * Insert a CID link into the tree.
     * Uses LinkType.File to store the target CID directly as a native link.
     * This enables natural deduplication and avoids JSON serialization.
     */
    insertLink(root: CID | null, key: string, targetCid: CID): Promise<CID>;
    /**
     * Get a CID link from the tree.
     */
    getLink(root: CID | null, key: string): Promise<CID | null>;
    /**
     * Iterate all CID links in the tree.
     */
    linksEntries(root: CID | null): AsyncGenerator<[string, CID]>;
    /**
     * Prefix search for CID links.
     */
    prefixLinks(root: CID, prefix: string): AsyncGenerator<[string, CID]>;
    /**
     * Count CID links by walking the tree.
     * Uses stored subtree sizes when available, but may scan descendants when
     * older roots do not carry complete counts.
     */
    countLinks(root: CID | null): Promise<number>;
    /**
     * Count CID links by walking the tree.
     */
    scanLinks(root: CID | null): Promise<number>;
    /**
     * Read the stored CID-link count from the root node without scanning.
     * Returns null when the root was built by older code that does not store
     * complete subtree sizes.
     */
    countStoredLinks(root: CID | null): Promise<number | null>;
    /**
     * Read the Nth CID link in sorted key order.
     */
    getLinkEntryAt(root: CID | null, ordinal: number): Promise<[string, CID] | null>;
    /**
     * Sample CID links uniformly by random ordinal.
     */
    sampleLinks(root: CID | null, limit: number, options?: BTreeSampleOptions): Promise<Array<[string, CID]>>;
    /**
     * Merge two BTree roots with CID link values.
     */
    mergeLinks(base: CID | null, other: CID | null, preferOther?: boolean): Promise<CID | null>;
    build(items: Iterable<[string, string]>): Promise<CID | null>;
    buildLinks(items: Iterable<[string, CID]>): Promise<CID | null>;
    private cidEquals;
    private createLeafWithLink;
    private insertLinkRecursive;
    private insertLinkIntoLeaf;
    private insertLinkIntoInternal;
    private splitLeafWithLinks;
    private traverseLinksInOrder;
    private rangeLinkTraverse;
    private countLinksRecursive;
    private countLinkEntries;
    private storedLinkSubtreeCount;
    private countLinkEntriesOrSubtrees;
    private getLinkEntryAtRecursive;
    private listCachedEntries;
    private insertRecursive;
    private insertIntoLeaf;
    private insertIntoInternal;
    private splitLeaf;
    private splitInternal;
    private findChild;
    private sortEntries;
    private isLeafNode;
    private createLeaf;
    private createInternalNode;
    delete(root: CID, key: string): Promise<CID | null>;
    entries(root: CID | null): AsyncGenerator<[string, string]>;
    private traverseInOrder;
    range(root: CID, start?: string, end?: string): AsyncGenerator<[string, string]>;
    private rangeTraverse;
    prefix(root: CID, prefix: string): AsyncGenerator<[string, string]>;
    merge(base: CID | null, other: CID | null, preferOther?: boolean): Promise<CID | null>;
}
export declare function escapeKey(key: string): string;
export declare function unescapeKey(name: string): string;
//# sourceMappingURL=btree.d.ts.map