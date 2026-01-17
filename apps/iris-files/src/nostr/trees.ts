/**
 * Tree Publishing and Management
 */
import { nip19 } from 'nostr-tools';
import {
  toHex,
  fromHex,
  type CID,
  type TreeVisibility,
} from 'hashtree';
import { nostrStore } from './store';
import { ndk } from './ndk';
import { updateLocalRootCache } from '../treeRootCache';
import { parseRoute } from '../utils/route';
import { getRefResolver } from '../refResolver';

// Re-export visibility hex helpers from hashtree lib
export { visibilityHex as linkKeyUtils } from 'hashtree';

export interface SaveHashtreeOptions {
  visibility?: TreeVisibility;
  /** Link key for link-visible trees - if not provided, one will be generated */
  linkKey?: string;
  /** Additional l-tags to add (e.g., ['docs'] for document trees) */
  labels?: string[];
}

/**
 * Parse visibility from Nostr event tags
 */
export function parseVisibility(tags: string[][]): { visibility: TreeVisibility; rootKey?: string; encryptedKey?: string; keyId?: string; selfEncryptedKey?: string; selfEncryptedLinkKey?: string } {
  const rootKey = tags.find(t => t[0] === 'key')?.[1];
  const encryptedKey = tags.find(t => t[0] === 'encryptedKey')?.[1];
  const keyId = tags.find(t => t[0] === 'keyId')?.[1];
  const selfEncryptedKey = tags.find(t => t[0] === 'selfEncryptedKey')?.[1];
  const selfEncryptedLinkKey = tags.find(t => t[0] === 'selfEncryptedLinkKey')?.[1];

  let visibility: TreeVisibility;
  // link-visible has encryptedKey but also selfEncryptedLinkKey (for owner to recover link key)
  if (encryptedKey) {
    visibility = 'link-visible';
  } else if (selfEncryptedKey) {
    visibility = 'private';
  } else {
    visibility = 'public';
  }

  return { visibility, rootKey, encryptedKey, keyId, selfEncryptedKey, selfEncryptedLinkKey };
}

/**
 * Save/publish hashtree to relays
 * Uses the resolver's publish method which handles all visibility encryption.
 * @param name - Tree name
 * @param rootCid - Root CID (hash + optional encryption key)
 * @param options - Visibility options
 * @returns Object with success status and linkKey (for link-visible trees)
 */
export async function saveHashtree(
  name: string,
  rootCid: CID,
  options: SaveHashtreeOptions = {}
): Promise<{ success: boolean; linkKey?: string }> {
  const state = nostrStore.getState();
  if (!state.pubkey || !state.npub) return { success: false };

  const visibility = options.visibility ?? 'public';
  const resolver = getRefResolver();

  // Optimistically update local state for offline-first behavior
  const currentSelected = state.selectedTree;
  if (currentSelected && currentSelected.name === name && currentSelected.pubkey === state.pubkey) {
    nostrStore.setSelectedTree({
      ...currentSelected,
      rootHash: toHex(rootCid.hash),
      rootKey: rootCid.key ? toHex(rootCid.key) : undefined,
      visibility,
      created_at: Math.floor(Date.now() / 1000),
    });
  }

  // Update treeRootCache immediately so local ops don't wait on publish
  updateLocalRootCache(state.npub, name, rootCid.hash, rootCid.key, visibility);

  // Use resolver to publish - it handles all visibility encryption
  const result = await resolver.publish?.(
    `${state.npub}/${name}`,
    rootCid,
    {
      visibility,
      linkKey: options.linkKey ? fromHex(options.linkKey) : undefined,
      labels: options.labels,
    }
  );

  if (!result?.success) {
    return { success: false, linkKey: result?.linkKey ? toHex(result.linkKey) : undefined };
  }

  return {
    success: true,
    linkKey: result.linkKey ? toHex(result.linkKey) : undefined,
  };
}

/**
 * Check if the selected tree belongs to the logged-in user
 */
export function isOwnTree(): boolean {
  const state = nostrStore.getState();
  if (!state.isLoggedIn || !state.selectedTree || !state.pubkey) return false;
  return state.selectedTree.pubkey === state.pubkey;
}

/**
 * Autosave current tree if it's our own.
 * Updates local cache immediately, publishing is throttled.
 * @param rootCid - Root CID (contains hash and optional encryption key)
 */
export function autosaveIfOwn(rootCid: CID): void {
  const state = nostrStore.getState();
  if (!isOwnTree() || !state.selectedTree || !state.npub) return;

  // Update local cache - this triggers throttled publish to Nostr
  // Pass visibility to ensure correct tags are published
  updateLocalRootCache(state.npub, state.selectedTree.name, rootCid.hash, rootCid.key, state.selectedTree.visibility);

  // Update selectedTree state immediately for UI (uses hex for state storage)
  const rootHash = toHex(rootCid.hash);
  const rootKey = rootCid.key ? toHex(rootCid.key) : undefined;
  nostrStore.setSelectedTree({
    ...state.selectedTree,
    rootHash,
    rootKey: state.selectedTree.visibility === 'public' ? rootKey : state.selectedTree.rootKey,
  });
}

/**
 * Publish tree root to Nostr (called by treeRootCache after throttle)
 * This is the ONLY place that should publish merkle roots.
 *
 * @param cachedVisibility - Visibility from the root cache. Use this first, then fall back to selectedTree.
 */
export async function publishTreeRoot(treeName: string, rootCid: CID, cachedVisibility?: TreeVisibility): Promise<boolean> {
  const state = nostrStore.getState();
  if (!state.pubkey || !ndk.signer) return false;

  // Priority: cached visibility > selectedTree visibility > 'public'
  let visibility: TreeVisibility = cachedVisibility ?? 'public';
  let linkKey: string | undefined;

  // If no cached visibility, try to get from selectedTree
  if (!cachedVisibility) {
    const isOwnSelectedTree = state.selectedTree?.name === treeName &&
      state.selectedTree?.pubkey === state.pubkey;
    if (isOwnSelectedTree && state.selectedTree?.visibility) {
      visibility = state.selectedTree.visibility;
    }
  }

  // For link-visible trees, get the linkKey from the URL
  if (visibility === 'link-visible') {
    const route = parseRoute();
    linkKey = route.params.get('k') ?? undefined;
  }

  const result = await saveHashtree(treeName, rootCid, {
    visibility,
    linkKey,
  });

  return result.success;
}

/**
 * Delete a tree (publishes event without hash to nullify)
 * Tree will disappear from listings but can be re-created with same name
 */
export async function deleteTree(treeName: string): Promise<boolean> {
  const state = nostrStore.getState();
  if (!state.npub) return false;

  // Cancel any pending throttled publish - this is critical!
  const { cancelPendingPublish } = await import('../treeRootCache');
  cancelPendingPublish(state.npub, treeName);

  // Remove from recents store
  const { removeRecentByTreeName } = await import('../stores/recents');
  removeRecentByTreeName(state.npub, treeName);

  const { getRefResolver } = await import('../refResolver');
  const resolver = getRefResolver();

  const key = `${state.npub}/${treeName}`;
  return resolver.delete?.(key) ?? false;
}

/**
 * Get npub from pubkey
 */
export function pubkeyToNpub(pk: string): string {
  return nip19.npubEncode(pk);
}

/**
 * Get pubkey from npub
 */
export function npubToPubkey(npubStr: string): string | null {
  try {
    const decoded = nip19.decode(npubStr);
    if (decoded.type !== 'npub') return null;
    return decoded.data as string;
  } catch {
    return null;
  }
}
