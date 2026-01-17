/**
 * Track visited link-visible trees for background sync
 *
 * When a user visits a link-visible tree via a link, we store the tree info
 * so it can be synced in the background for offline access.
 */
import Dexie, { type Table } from 'dexie';
import type { CID } from 'hashtree';
import { toHex, fromHex, cid } from 'hashtree';

/**
 * Record of a visited link-visible tree
 */
export interface VisitedTree {
  /** "npub/treeName" - primary key */
  key: string;
  /** Owner's npub for quota grouping */
  ownerNpub: string;
  /** Link key used to access the tree (hex) */
  linkKeyHex: string;
  /** Last root hash seen (hex) */
  lastRootHash: string;
  /** Encryption key from CID (hex, optional) */
  encryptionKeyHex?: string;
  /** Timestamp when first visited */
  firstVisited: number;
  /** Timestamp when last visited */
  lastVisited: number;
}

class VisitedTreesDB extends Dexie {
  trees!: Table<VisitedTree, string>;

  constructor() {
    super('hashtree-visited-trees');
    this.version(1).stores({
      trees: '&key, ownerNpub, lastVisited',
    });
  }
}

const db = new VisitedTreesDB();

/**
 * Record a visit to a link-visible tree
 * Updates lastVisited and root hash if already exists
 */
export async function recordTreeVisit(
  key: string,
  rootCid: CID,
  linkKeyHex: string
): Promise<void> {
  const now = Date.now();
  const ownerNpub = key.split('/')[0];
  const lastRootHash = toHex(rootCid.hash);
  const encryptionKeyHex = rootCid.key ? toHex(rootCid.key) : undefined;

  const existing = await db.trees.get(key);

  if (existing) {
    // Update existing record
    await db.trees.update(key, {
      lastRootHash,
      encryptionKeyHex,
      lastVisited: now,
    });
  } else {
    // Create new record
    await db.trees.put({
      key,
      ownerNpub,
      linkKeyHex,
      lastRootHash,
      encryptionKeyHex,
      firstVisited: now,
      lastVisited: now,
    });
  }
}

/**
 * Get all visited link-visible trees
 */
export async function getVisitedTrees(): Promise<VisitedTree[]> {
  return db.trees.toArray();
}

/**
 * Get visited trees for a specific owner
 */
export async function getVisitedTreesByOwner(ownerNpub: string): Promise<VisitedTree[]> {
  return db.trees.where('ownerNpub').equals(ownerNpub).toArray();
}

/**
 * Get a specific visited tree
 */
export async function getVisitedTree(key: string): Promise<VisitedTree | undefined> {
  return db.trees.get(key);
}

/**
 * Remove a visited tree record
 */
export async function removeVisitedTree(key: string): Promise<void> {
  await db.trees.delete(key);
}

/**
 * Get CID for a visited tree (reconstructs from stored hash/key)
 */
export async function getVisitedTreeCid(key: string): Promise<CID | null> {
  const tree = await db.trees.get(key);
  if (!tree) return null;

  const hash = fromHex(tree.lastRootHash);
  const encKey = tree.encryptionKeyHex ? fromHex(tree.encryptionKeyHex) : undefined;
  return cid(hash, encKey);
}

/**
 * Clear all visited trees (for testing/reset)
 */
export async function clearVisitedTrees(): Promise<void> {
  await db.trees.clear();
}

// Export database for testing
export { db as visitedTreesDb };
