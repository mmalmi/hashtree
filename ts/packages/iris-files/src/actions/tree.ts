/**
 * Tree operations - create, fork, verify trees
 */
import { navigate } from '../utils/navigate';
import { parseRoute } from '../utils/route';
import { verifyTree, toHex, LinkType } from 'hashtree';
import type { CID } from 'hashtree';
import { saveHashtree, useNostrStore } from '../nostr';
import { nip19 } from 'nostr-tools';
import { localStore, getTree } from '../store';
import { autosaveIfOwn } from '../nostr';
import { getCurrentRootCid, getCurrentPathFromUrl } from './route';
import { updateLocalRootCache } from '../treeRootCache';

// Helper to initialize a virtual tree (when rootCid is null but we're in a tree route)
export async function initVirtualTree(entries: { name: string; cid: CID; size: number; type?: LinkType }[]): Promise<CID | null> {
  const route = parseRoute();
  if (!route.npub || !route.treeName) return null;

  const tree = getTree();
  const nostrStore = useNostrStore.getState();

  let routePubkey: string;
  try {
    const decoded = nip19.decode(route.npub);
    if (decoded.type !== 'npub') return null;
    routePubkey = decoded.data as string;
  } catch {
    return null;
  }

  const isOwnTree = routePubkey === nostrStore.pubkey;
  if (!isOwnTree) return null; // Can only create in own trees

  // Create new encrypted tree with the entries (using DirEntry format)
  const dirEntries = entries.map(e => ({
    name: e.name,
    cid: e.cid,
    size: e.size,
    type: e.type,
  }));
  const { cid: newRootCid } = await tree.putDirectory(dirEntries);

  // Preserve current tree's visibility when updating
  const currentVisibility = nostrStore.selectedTree?.visibility ?? 'public';

  // Update UI state immediately (uses hex for storage)
  useNostrStore.setSelectedTree({
    id: '',
    name: route.treeName,
    pubkey: routePubkey,
    rootHash: toHex(newRootCid.hash),
    rootKey: newRootCid.key ? toHex(newRootCid.key) : undefined,
    visibility: currentVisibility,
    created_at: Math.floor(Date.now() / 1000),
  });

  // Save to Nostr (fire-and-forget, also updates local cache)
  void saveHashtree(route.treeName, newRootCid, { visibility: currentVisibility });

  return newRootCid;
}

// Create new folder
export async function createFolder(name: string) {
  if (!name) return;

  const rootCid = getCurrentRootCid();
  const tree = getTree();
  const currentPath = getCurrentPathFromUrl();

  // putDirectory returns CID (encrypted by default)
  const { cid: emptyDirCid } = await tree.putDirectory([]);

  if (rootCid) {
    // Add to existing tree
    const newRootCid = await tree.setEntry(
      rootCid,
      currentPath,
      name,
      emptyDirCid,
      0,
      LinkType.Dir
    );
    // Publish to nostr - resolver will pick up the update
    autosaveIfOwn(newRootCid);
  } else {
    // Initialize virtual tree with this folder
    await initVirtualTree([{ name, cid: emptyDirCid, size: 0, type: LinkType.Dir }]);
  }
}

// Create new Yjs document folder (folder with .yjs config file)
export async function createDocument(name: string) {
  if (!name) return;

  const rootCid = getCurrentRootCid();
  const tree = getTree();
  const currentPath = getCurrentPathFromUrl();

  // Create .yjs config file with owner's npub as first editor
  const nostrState = useNostrStore.getState();
  const ownerNpub = nostrState.npub || '';
  const yjsContent = new TextEncoder().encode(ownerNpub ? ownerNpub + '\n' : '');
  const { cid: yjsFileCid, size: yjsFileSize } = await tree.putFile(yjsContent);

  // Create directory with .yjs file inside
  const { cid: docDirCid } = await tree.putDirectory([
    { name: '.yjs', cid: yjsFileCid, size: yjsFileSize, type: LinkType.Blob }
  ]);

  if (rootCid) {
    // Add to existing tree
    const newRootCid = await tree.setEntry(
      rootCid,
      currentPath,
      name,
      docDirCid,
      0,
      LinkType.Dir
    );
    // Publish to nostr
    autosaveIfOwn(newRootCid);

    // Update local cache for subsequent saves (visibility is preserved from selectedTree)
    const route = parseRoute();
    const nostrStore = useNostrStore.getState();
    if (nostrStore.npub && route.treeName) {
      updateLocalRootCache(nostrStore.npub, route.treeName, newRootCid.hash, newRootCid.key, nostrStore.selectedTree?.visibility);
    }
  } else {
    // Initialize virtual tree with this document folder
    await initVirtualTree([{ name, cid: docDirCid, size: 0, type: LinkType.Dir }]);
  }
}

// Fork a directory as a new top-level tree
// Re-encrypts if source is unencrypted to ensure all forked content is encrypted
export async function forkTree(dirCid: CID, name: string, visibility: import('hashtree').TreeVisibility = 'public'): Promise<{ success: boolean; linkKey?: string }> {
  if (!name) return { success: false };

  const { saveHashtree } = await import('../nostr');
  const { storeLinkKey } = await import('../stores/trees');

  // If source is unencrypted (no key), re-encrypt it
  let finalCid = dirCid;
  if (!dirCid.key) {
    console.log('[Fork] Source is unencrypted, re-encrypting...');
    const tree = getTree();

    // Rebuild with encryption (doesn't publish to Nostr - we'll do that below)
    const rebuildWithEncryption = async (oldCid: CID): Promise<CID> => {
      if (oldCid.key) return oldCid;

      const isDir = await tree.isDirectory(oldCid);
      if (isDir) {
        const entries = await tree.listDirectory(oldCid);
        const newEntries = [];
        for (const entry of entries) {
          const newChildCid = await rebuildWithEncryption(entry.cid);
          newEntries.push({
            name: entry.name,
            cid: newChildCid,
            size: entry.size,
            type: entry.type ?? 0,
            meta: entry.meta,
          });
        }
        return (await tree.putDirectory(newEntries, {})).cid;
      } else {
        const data = await tree.readFile(oldCid);
        if (!data) return oldCid;
        return (await tree.putFile(data, {})).cid;
      }
    };

    finalCid = await rebuildWithEncryption(dirCid);
    console.log('[Fork] Re-encryption complete');
  }

  const nostrState = useNostrStore.getState();

  if (!nostrState.npub || !nostrState.pubkey) return { success: false };

  useNostrStore.setSelectedTree({
    id: '',
    name,
    pubkey: nostrState.pubkey,
    rootHash: toHex(finalCid.hash),
    rootKey: finalCid.key ? toHex(finalCid.key) : undefined,
    visibility,
    created_at: Math.floor(Date.now() / 1000),
  });

  // Save to Nostr (also updates local cache)
  const result = await saveHashtree(name, finalCid, { visibility });

  // For link-visible trees, store link key locally and append to URL
  if (result.linkKey) {
    storeLinkKey(nostrState.npub, name, result.linkKey);
  }

  if (result.success) {
    const linkKeyParam = result.linkKey ? `?k=${result.linkKey}` : '';
    navigate(`/${encodeURIComponent(nostrState.npub)}/${encodeURIComponent(name)}${linkKeyParam}`);
  }
  return result;
}

// Create a new tree (top-level folder on nostr or local)
// Creates encrypted trees by default
// Set skipNavigation=true to create without navigating (for batch creation)
export async function createTree(name: string, visibility: import('hashtree').TreeVisibility = 'public', skipNavigation = false): Promise<{ success: boolean; linkKey?: string }> {
  if (!name) return { success: false };

  const { saveHashtree } = await import('../nostr');
  const { storeLinkKey } = await import('../stores/trees');

  const tree = getTree();
  // Create encrypted empty directory (default)
  const { cid: rootCid } = await tree.putDirectory([]);

  const nostrState = useNostrStore.getState();

  // If logged in, publish to nostr
  if (nostrState.isLoggedIn && nostrState.npub && nostrState.pubkey) {
    // Set selectedTree BEFORE saving so updates work (only if we're navigating)
    if (!skipNavigation) {
      useNostrStore.setSelectedTree({
        id: '', // Will be set by actual nostr event
        name,
        pubkey: nostrState.pubkey,
        rootHash: toHex(rootCid.hash),
        rootKey: rootCid.key ? toHex(rootCid.key) : undefined,
        visibility,
        created_at: Math.floor(Date.now() / 1000),
      });
    }

    // Save to Nostr (also updates local cache immediately for subsequent operations)
    const result = await saveHashtree(name, rootCid, { visibility });

    // For link-visible trees, store link key locally and append to URL
    if (result.linkKey) {
      storeLinkKey(nostrState.npub, name, result.linkKey);
    }

    if (!skipNavigation) {
      const linkKeyParam = result.linkKey ? `?k=${result.linkKey}` : '';
      navigate(`/${encodeURIComponent(nostrState.npub)}/${encodeURIComponent(name)}${linkKeyParam}`);
    }
    return result;
  }

  // Not logged in - can't create trees without nostr
  return { success: false };
}

// Create a new tree as a document (with .yjs config file)
// Used by docs app to create standalone documents
export async function createDocumentTree(
  name: string,
  visibility: import('hashtree').TreeVisibility = 'public'
): Promise<{ success: boolean; npub?: string; treeName?: string; linkKey?: string }> {
  if (!name) return { success: false };

  const { saveHashtree } = await import('../nostr');
  const { storeLinkKey } = await import('../stores/trees');

  const tree = getTree();
  const nostrState = useNostrStore.getState();

  if (!nostrState.isLoggedIn || !nostrState.npub || !nostrState.pubkey) {
    return { success: false };
  }

  const treeName = `docs/${name}`;

  // Create .yjs config file with owner's npub as first editor
  const yjsContent = new TextEncoder().encode(nostrState.npub + '\n');
  const { cid: yjsFileCid, size: yjsFileSize } = await tree.putFile(yjsContent);

  // Create root directory with .yjs file inside
  const { cid: rootCid } = await tree.putDirectory([
    { name: '.yjs', cid: yjsFileCid, size: yjsFileSize, type: LinkType.Blob }
  ]);

  // Set selectedTree for updates
  useNostrStore.setSelectedTree({
    id: '',
    name: treeName,
    pubkey: nostrState.pubkey,
    rootHash: toHex(rootCid.hash),
    rootKey: rootCid.key ? toHex(rootCid.key) : undefined,
    visibility,
    created_at: Math.floor(Date.now() / 1000),
  });

  // Save to Nostr with docs label (also updates local cache)
  const result = await saveHashtree(treeName, rootCid, { visibility, labels: ['docs'] });

  // Store link key for link-visible documents
  if (result.linkKey) {
    storeLinkKey(nostrState.npub, treeName, result.linkKey);
  }

  return { success: true, npub: nostrState.npub, treeName, linkKey: result.linkKey };
}

// Verify tree
export async function verifyCurrentTree(): Promise<{ valid: boolean; missing: number }> {
  const rootCid = getCurrentRootCid();
  if (!rootCid) return { valid: false, missing: 0 };

  const { valid, missing } = await verifyTree(localStore, rootCid.hash);
  return { valid, missing: missing.length };
}

// Clear store
export function clearStore() {
  localStore.clear();
  navigate('/');
}
