/**
 * Tree verification utilities
 */

import { Store, Hash, toHex } from './types.js';
import { tryDecodeTreeNode } from './codec.js';

/**
 * Verify tree integrity - checks that all referenced hashes exist
 */
export async function verifyTree(
  store: Store,
  rootHash: Hash
): Promise<{ valid: boolean; missing: Hash[] }> {
  const missing: Hash[] = [];
  const visited = new Set<string>();

  async function check(hash: Hash): Promise<void> {
    const hex = toHex(hash);
    if (visited.has(hex)) return;
    visited.add(hex);

    const data = await store.get(hash);
    if (!data) {
      missing.push(hash);
      return;
    }

    const node = tryDecodeTreeNode(data);
    if (node) {
      for (const link of node.links) {
        await check(link.hash);
      }
    }
  }

  await check(rootHash);

  return { valid: missing.length === 0, missing };
}
