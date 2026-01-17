/**
 * Entry operations - rename, delete, move entries
 */
import { navigate } from '../utils/navigate';
import { parseRoute } from '../utils/route';
import { autosaveIfOwn } from '../nostr';
import { getTree } from '../store';
import { LinkType } from 'hashtree';
import { getCurrentRootCid, getCurrentPathFromUrl, buildRouteUrl, updateRoute } from './route';

// Rename entry
export async function renameEntry(oldName: string, newName: string) {
  if (!newName || oldName === newName) return;

  const rootCid = getCurrentRootCid();
  if (!rootCid) return;

  const tree = getTree();
  const route = parseRoute();
  const urlPath = route.path;

  // Check if we're renaming the current directory (we're inside it)
  const lastSegment = urlPath.length > 0 ? urlPath[urlPath.length - 1] : null;
  const isRenamingCurrentDir = lastSegment === oldName && !/\.[a-zA-Z0-9]+$/.test(oldName);

  let parentPath: string[];
  if (isRenamingCurrentDir) {
    // Renaming current directory - parent is everything except last segment
    parentPath = urlPath.slice(0, -1);
  } else {
    // Renaming item within current directory
    parentPath = getCurrentPathFromUrl();
  }

  const newRootCid = await tree.renameEntry(
    rootCid,
    parentPath,
    oldName,
    newName
  );
  // Update local cache (publishes to nostr with throttle)
  autosaveIfOwn(newRootCid);

  // Update URL if renamed file/dir was selected or we're inside it
  if (isRenamingCurrentDir) {
    // Navigate to the renamed directory
    const newPath = [...parentPath, newName];
    const url = buildRouteUrl(route.npub, route.treeName, newPath, undefined, route.params.get('k'));
    navigate(url);
  } else if (lastSegment === oldName) {
    updateRoute(newName);
  }
}

// Delete entry
export async function deleteEntry(name: string) {
  const rootCid = getCurrentRootCid();
  if (!rootCid) return;

  const tree = getTree();
  const currentPath = getCurrentPathFromUrl();

  const newRootCid = await tree.removeEntry(
    rootCid,
    currentPath,
    name
  );
  // Update local cache (publishes to nostr with throttle)
  autosaveIfOwn(newRootCid);

  // Navigate to directory if deleted file was active
  const route = parseRoute();
  const urlFileName = route.path.length > 0 ? route.path[route.path.length - 1] : null;
  if (urlFileName === name) {
    const url = buildRouteUrl(route.npub, route.treeName, currentPath, undefined, route.params.get('k'));
    navigate(url);
  }
}

// Delete current folder (must be in a subdirectory)
export async function deleteCurrentFolder() {
  const rootCid = getCurrentRootCid();
  if (!rootCid) return;

  const route = parseRoute();
  if (route.path.length === 0) return; // Can't delete root

  const folderName = route.path[route.path.length - 1];
  const parentPath = route.path.slice(0, -1);

  const tree = getTree();

  const newRootCid = await tree.removeEntry(
    rootCid,
    parentPath,
    folderName
  );
  // Update local cache (publishes to nostr with throttle)
  autosaveIfOwn(newRootCid);

  // Navigate to parent directory
  const url = buildRouteUrl(route.npub, route.treeName, parentPath, undefined, route.params.get('k'));
  navigate(url);
}

// Move entry into a directory
export async function moveEntry(sourceName: string, targetDirName: string) {
  const rootCid = getCurrentRootCid();
  if (!rootCid) return;
  if (sourceName === targetDirName) return;

  const tree = getTree();
  const currentPath = getCurrentPathFromUrl();

  // Resolve target directory from tree
  const targetPath = [...currentPath, targetDirName].join('/');
  const targetResult = await tree.resolvePath(rootCid, targetPath);
  if (!targetResult) return;

  // Check target is a directory
  if (targetResult.type !== LinkType.Dir) return;

  // Check for name collision
  const targetContents = await tree.listDirectory(targetResult.cid);
  if (targetContents.some(e => e.name === sourceName)) {
    alert(`A file named "${sourceName}" already exists in "${targetDirName}"`);
    return;
  }

  const newRootCid = await tree.moveEntry(rootCid, currentPath, sourceName, [...currentPath, targetDirName]);
  // Update local cache (publishes to nostr with throttle)
  autosaveIfOwn(newRootCid);

  // Clear selection if moved file was active
  const route = parseRoute();
  const urlFileName = route.path.length > 0 ? route.path[route.path.length - 1] : null;
  if (urlFileName === sourceName) {
    const url = buildRouteUrl(route.npub, route.treeName, currentPath, undefined, route.params.get('k'));
    navigate(url);
  }
}

// Move entry to parent directory
export async function moveToParent(sourceName: string) {
  const rootCid = getCurrentRootCid();
  if (!rootCid) return;

  const currentPath = getCurrentPathFromUrl();
  if (currentPath.length === 0) return; // Already at root

  const tree = getTree();
  const parentPath = currentPath.slice(0, -1);

  // Check for name collision in parent
  const parentCid = parentPath.length === 0
    ? rootCid
    : (await tree.resolvePath(rootCid, parentPath.join('/')))?.cid;
  if (!parentCid) return;

  const parentEntries = await tree.listDirectory(parentCid);
  if (parentEntries.some(e => e.name === sourceName)) {
    alert(`A file named "${sourceName}" already exists in the parent directory`);
    return;
  }

  const newRootCid = await tree.moveEntry(rootCid, currentPath, sourceName, parentPath);
  // Update local cache (publishes to nostr with throttle)
  autosaveIfOwn(newRootCid);

  // Clear selection if moved file was active
  const route = parseRoute();
  const urlFileName = route.path.length > 0 ? route.path[route.path.length - 1] : null;
  if (urlFileName === sourceName) {
    const url = buildRouteUrl(route.npub, route.treeName, currentPath, undefined, route.params.get('k'));
    navigate(url);
  }
}
