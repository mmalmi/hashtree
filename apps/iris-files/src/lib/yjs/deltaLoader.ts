/**
 * Delta Loader Module
 * Handles loading Yjs deltas and state from tree entries
 */
import * as Y from 'yjs';
import { LinkType, cid } from 'hashtree';
import type { TreeEntry } from 'hashtree';
import { getTree, decodeAsText } from '../../store';
import { subscribeToTreeRoot, waitForTreeRoot } from '../../stores/treeRoot';

const DELTAS_DIR = 'deltas';
const STATE_FILE = 'state.yjs';

/**
 * Load deltas from a directory's entries
 */
export async function loadDeltasFromEntries(docEntries: TreeEntry[]): Promise<Uint8Array[]> {
  const tree = getTree();
  const deltas: Uint8Array[] = [];

  // Load state.yjs if exists
  const stateEntry = docEntries.find(e => e.name === STATE_FILE && e.type !== LinkType.Dir);
  if (stateEntry) {
    const data = await tree.readFile(stateEntry.cid);
    if (data) deltas.push(data);
  }

  // Load deltas from deltas/ directory
  const deltasEntry = docEntries.find(e => e.name === DELTAS_DIR && e.type === LinkType.Dir);
  if (deltasEntry) {
    try {
      const deltaEntries = await tree.listDirectory(deltasEntry.cid);
      // Sort by name (timestamp-based or numeric)
      const sorted = deltaEntries
        .filter(e => e.type !== LinkType.Dir)
        .sort((a, b) => a.name.localeCompare(b.name, undefined, { numeric: true }));

      for (const entry of sorted) {
        const data = await tree.readFile(entry.cid);
        if (data) deltas.push(data);
      }
    } catch (err) {
      console.error(`[YjsDoc] loadDeltasFromEntries error:`, err);
    }
  }

  return deltas;
}

/**
 * Load deltas from all collaborators' trees
 */
export async function loadCollaboratorDeltas(
  collaboratorNpubs: string[],
  routeNpub: string | null,
  routePath: string[],
  treeName: string | null,
  ydoc: Y.Doc
): Promise<void> {
  const tree = getTree();
  const docPath = routePath.join('/');

  if (!treeName) return;

  // Filter out the currently viewed tree's npub (we already loaded local deltas)
  const otherEditors = collaboratorNpubs.filter(npub => npub !== routeNpub);

  for (const npub of otherEditors) {
    try {
      // Resolve the editor's tree root via treeRoot store (waits for worker readiness)
      const rootCid = await waitForTreeRoot(npub, treeName, 5000);

      if (!rootCid) continue;

      // Resolve the path to the document directory in their tree
      const result = await tree.resolvePath(rootCid, docPath);
      if (!result) continue;

      // Check if it's a directory
      const isDir = await tree.isDirectory(result.cid);
      if (!isDir) continue;

      // List entries in their document directory
      const collabEntries = await tree.listDirectory(result.cid);

      // Load and apply their deltas
      const collabDeltas = await loadDeltasFromEntries(collabEntries);
      for (const delta of collabDeltas) {
        Y.applyUpdate(ydoc, delta, 'remote');
      }

    } catch (err) {
      console.warn(`[YjsDoc] Failed to load deltas from collaborator ${npub}:`, err);
    }
  }
}

/**
 * Setup subscriptions to collaborators' trees for live updates
 * Returns a cleanup function to unsubscribe
 */
export function setupCollaboratorSubscriptions(
  collaboratorNpubs: string[],
  treeName: string | null,
  routePath: string[],
  viewedNpub: string | null,
  userNpub: string | null,
  ydoc: Y.Doc,
  getCollaborators: () => string[],
  setCollaborators: (npubs: string[]) => void
): () => void {
  const unsubscribes: (() => void)[] = [];

  if (collaboratorNpubs.length === 0 || !treeName) return () => {};

  const tree = getTree();
  const docPath = routePath.join('/');
  const docOwnerNpub = viewedNpub || userNpub;

  // Subscribe to collaborators' trees, but NOT our own tree
  // Our own updates are already in local state - re-applying them causes focus loss
  const otherCollaborators = collaboratorNpubs.filter(npub => npub !== userNpub);

  for (const npub of otherCollaborators) {
    const unsub = subscribeToTreeRoot(npub, treeName, async (hash, encryptionKey) => {
      if (!hash || !ydoc) {
        return;
      }

      try {
        // Fetch and apply deltas from this editor
        const rootCid = cid(hash, encryptionKey);
        const result = await tree.resolvePath(rootCid, docPath);
        if (!result) {
          return;
        }

        const isDir = await tree.isDirectory(result.cid);
        if (!isDir) {
          return;
        }

        const collabEntries = await tree.listDirectory(result.cid);

        // If this update is from the document owner, re-read .yjs to check for collaborator changes
        if (npub === docOwnerNpub) {
          const yjsConfigEntry = collabEntries.find(e => e.name === '.yjs' && e.type !== LinkType.Dir);
          if (yjsConfigEntry) {
            const data = await tree.readFile(yjsConfigEntry.cid);
            if (data) {
              const text = decodeAsText(data);
              if (text) {
                const newCollaborators = text.split('\n').filter(line => line.trim().startsWith('npub1'));
                // Update if changed
                if (JSON.stringify(newCollaborators) !== JSON.stringify(getCollaborators())) {
                  setCollaborators(newCollaborators);
                }
              }
            }
          }
        }

        // Load and apply deltas
        const collabDeltas = await loadDeltasFromEntries(collabEntries);
        for (const delta of collabDeltas) {
          Y.applyUpdate(ydoc, delta, 'remote');
        }

      } catch (err) {
        console.warn(`[YjsDoc] Failed to fetch updates from editor ${npub}:`, err);
      }
    });

    unsubscribes.push(unsub);
  }

  return () => {
    unsubscribes.forEach(unsub => unsub());
  };
}
