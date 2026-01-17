/**
 * Re-encryption migration
 *
 * Detects unencrypted trees (CID has no key) and re-encrypts them.
 * This rebuilds the tree with encryption and publishes the new root.
 */

import { getTree } from '../store';
import { updateLocalRootCacheHex } from '../treeRootCache';
import { toHex, LinkType } from 'hashtree';
import type { CID, Hash, TreeVisibility } from 'hashtree';
import { getRefResolver } from '../refResolver';
import { getWorkerAdapter } from '../lib/workerInit';

interface TreeInfo {
  name: string;
  hash: Hash;
  key?: Hash;
  visibility?: TreeVisibility;
}

interface ReencryptStats {
  treesChecked: number;
  treesReencrypted: number;
  errors: number;
}

/**
 * Fetch all trees for a user that need re-encryption (no key)
 */
async function fetchUnencryptedTrees(npub: string): Promise<TreeInfo[]> {
  return new Promise((resolve) => {
    const resolver = getRefResolver();
    if (!resolver.list) {
      resolve([]);
      return;
    }

    const unencryptedTrees: TreeInfo[] = [];
    let lastUpdateTime = Date.now();
    let checkInterval: ReturnType<typeof setInterval>;
    const unsub: { current?: () => void } = {};

    const checkStable = () => {
      if (Date.now() - lastUpdateTime > 1000) {
        clearInterval(checkInterval);
        unsub.current?.();
        resolve(unencryptedTrees);
      }
    };

    setTimeout(() => {
      checkInterval = setInterval(checkStable, 200);
    }, 500);

    setTimeout(() => {
      clearInterval(checkInterval);
      unsub.current?.();
      resolve(unencryptedTrees);
    }, 10000);

    unsub.current = resolver.list(npub, (trees) => {
      lastUpdateTime = Date.now();
      unencryptedTrees.length = 0;

      for (const tree of trees) {
        // Only include trees without a key (unencrypted)
        if (!tree.key) {
          unencryptedTrees.push({
            name: tree.name,
            hash: tree.hash,
            key: tree.key,
            visibility: tree.visibility,
          });
        }
      }
    });
  });
}

/**
 * Re-encrypt a single tree by rebuilding it with encryption
 */
async function reencryptTree(
  npub: string,
  treeInfo: TreeInfo
): Promise<CID | null> {
  const tree = getTree();
  const oldCid: CID = { hash: treeInfo.hash };

  console.log(`[Reencrypt] Processing tree: ${treeInfo.name}`);

  try {
    // Recursively rebuild the tree with encryption
    const newCid = await rebuildWithEncryption(tree, oldCid);

    if (!newCid.key) {
      console.warn(`[Reencrypt] Rebuild produced no key for ${treeInfo.name}`);
      return null;
    }

    console.log(`[Reencrypt] Rebuilt ${treeInfo.name} with encryption`);
    return newCid;
  } catch (e) {
    console.error(`[Reencrypt] Failed to rebuild ${treeInfo.name}:`, e);
    return null;
  }
}

/**
 * Recursively rebuild a tree/file with encryption
 * @param force - Force re-encryption even if CID has a key (for fixing unencrypted blob data)
 */
async function rebuildWithEncryption(
  tree: ReturnType<typeof getTree>,
  oldCid: CID,
  force: boolean = false
): Promise<CID> {
  // If already encrypted (has key) and not forcing, return as-is
  if (oldCid.key && !force) {
    return oldCid;
  }

  // When forcing re-encryption, we need to read data WITHOUT the key
  // because the blob data is actually unencrypted despite CID having a key
  const readCid: CID = force ? { hash: oldCid.hash } : oldCid;

  // Check if it's a directory (use readCid to read unencrypted data when forcing)
  console.log(`[Reencrypt] Checking if directory: hash=${toHex(readCid.hash).slice(0, 8)}, hasKey=${!!readCid.key}, force=${force}`);
  const isDir = await tree.isDirectory(readCid);
  console.log(`[Reencrypt] isDirectory=${isDir}`);

  if (isDir) {
    // It's a directory - rebuild each entry recursively
    const entries = await tree.listDirectory(readCid);
    console.log(`[Reencrypt] Directory has ${entries.length} entries`);
    const newEntries: Array<{
      name: string;
      cid: CID;
      size: number;
      type: number;
      meta?: Record<string, unknown>;
    }> = [];

    for (const entry of entries) {
      console.log(`[Reencrypt] Processing entry: ${entry.name}, hash=${toHex(entry.cid.hash).slice(0, 8)}, hasKey=${!!entry.cid.key}`);
      // For child entries, also force re-encryption if we're forcing
      const childCid = force ? { hash: entry.cid.hash } : entry.cid;
      const newChildCid = await rebuildWithEncryption(tree, childCid, force);
      console.log(`[Reencrypt] Entry ${entry.name} re-encrypted: newHash=${toHex(newChildCid.hash).slice(0, 8)}, hasKey=${!!newChildCid.key}`);
      newEntries.push({
        name: entry.name,
        cid: newChildCid,
        size: entry.size,
        type: entry.type ?? LinkType.File,
        meta: entry.meta,
      });
    }

    // Create new encrypted directory
    console.log(`[Reencrypt] Creating encrypted directory with ${newEntries.length} entries`);
    const result = await tree.putDirectory(newEntries, {}); // encrypted by default
    console.log(`[Reencrypt] New directory: hash=${toHex(result.cid.hash).slice(0, 8)}, hasKey=${!!result.cid.key}`);
    return result.cid;
  } else {
    // It's a file - read and re-store with encryption
    console.log(`[Reencrypt] Reading file without key: hash=${toHex(readCid.hash).slice(0, 8)}`);
    const data = await tree.readFile(readCid);
    if (!data) {
      console.warn(`[Reencrypt] Could not read file data for hash=${toHex(readCid.hash).slice(0, 8)}`);
      return oldCid;
    }

    console.log(`[Reencrypt] File data read: ${data.length} bytes`);
    const result = await tree.putFile(data, {}); // encrypted by default
    console.log(`[Reencrypt] File re-encrypted: newHash=${toHex(result.cid.hash).slice(0, 8)}, hasKey=${!!result.cid.key}`);
    return result.cid;
  }
}

/**
 * Publish the re-encrypted tree to Nostr
 */
async function publishReencryptedTree(
  npub: string,
  treeName: string,
  newCid: CID,
  visibility: TreeVisibility = 'public'
): Promise<boolean> {
  const resolver = getRefResolver();

  if (!resolver.publish) {
    console.error('[Reencrypt] Resolver has no publish method');
    return false;
  }

  try {
    // Update local cache first
    updateLocalRootCacheHex(npub, treeName, toHex(newCid.hash), newCid.key ? toHex(newCid.key) : undefined);

    // Publish to Nostr
    await resolver.publish(`${npub}/${treeName}`, newCid, { visibility });

    // Push to Blossom
    const adapter = getWorkerAdapter();
    if (adapter) {
      await adapter.pushToBlossom(newCid.hash, newCid.key, treeName);
    }

    console.log(`[Reencrypt] Published ${treeName} with new encrypted root`);
    return true;
  } catch (e) {
    console.error(`[Reencrypt] Failed to publish ${treeName}:`, e);
    return false;
  }
}

/**
 * Run the re-encryption migration for a user
 */
export async function runReencryptMigration(npub: string): Promise<void> {
  console.log('[Reencrypt] Starting re-encryption migration...');

  const stats: ReencryptStats = {
    treesChecked: 0,
    treesReencrypted: 0,
    errors: 0,
  };

  // Find all unencrypted trees
  const unencryptedTrees = await fetchUnencryptedTrees(npub);
  console.log(`[Reencrypt] Found ${unencryptedTrees.length} unencrypted trees`);

  for (const treeInfo of unencryptedTrees) {
    stats.treesChecked++;

    try {
      // Re-encrypt the tree
      const newCid = await reencryptTree(npub, treeInfo);
      if (!newCid) {
        stats.errors++;
        continue;
      }

      // Publish the re-encrypted tree
      const success = await publishReencryptedTree(
        npub,
        treeInfo.name,
        newCid,
        treeInfo.visibility
      );

      if (success) {
        stats.treesReencrypted++;
      } else {
        stats.errors++;
      }
    } catch (e) {
      console.error(`[Reencrypt] Error processing ${treeInfo.name}:`, e);
      stats.errors++;
    }
  }

  console.log('[Reencrypt] Migration complete:', stats);
}

/**
 * Check if a specific tree needs re-encryption
 */
export function needsReencryption(cid: CID): boolean {
  return !cid.key;
}

/**
 * Re-encrypt a single tree on demand (e.g., from push button)
 * @param force - Force re-encryption even if CID has a key (for fixing unencrypted blob data)
 */
export async function reencryptSingleTree(
  npub: string,
  treeName: string,
  oldCid: CID,
  visibility: TreeVisibility = 'public',
  force: boolean = false
): Promise<CID | null> {
  if (oldCid.key && !force) {
    console.log('[Reencrypt] Tree already encrypted (use force=true to re-encrypt anyway)');
    return oldCid;
  }

  const tree = getTree();

  try {
    console.log(`[Reencrypt] Re-encrypting ${treeName}${force ? ' (forced)' : ''}...`);
    const newCid = await rebuildWithEncryption(tree, oldCid, force);

    if (!newCid.key) {
      console.error('[Reencrypt] Rebuild failed - no key');
      return null;
    }

    // Publish the re-encrypted tree
    const success = await publishReencryptedTree(npub, treeName, newCid, visibility);
    if (!success) {
      return null;
    }

    return newCid;
  } catch (e) {
    console.error(`[Reencrypt] Failed:`, e);
    return null;
  }
}
