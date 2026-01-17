/**
 * PWA Saver - saves PWA assets to hashtree and returns nhash URL
 */

import { getTree } from '../store';
import { nhashEncode, LinkType, type CID } from 'hashtree';
import type { PWAInfo, PWAAsset } from './pwaFetcher';

interface DirectoryEntry {
  name: string;
  cid: CID;
  size: number;
  type?: LinkType;
}

/**
 * Get all directory paths that need to be created
 */
function getDirectoryPaths(assets: PWAAsset[]): string[] {
  const dirs = new Set<string>();

  for (const asset of assets) {
    const parts = asset.path.split('/');
    parts.pop(); // Remove filename

    // Add all parent paths
    for (let i = 1; i <= parts.length; i++) {
      dirs.add(parts.slice(0, i).join('/'));
    }
  }

  // Sort by depth (shallowest first) then by name
  return Array.from(dirs).sort((a, b) => {
    const depthA = a.split('/').length;
    const depthB = b.split('/').length;
    if (depthA !== depthB) return depthA - depthB;
    return a.localeCompare(b);
  });
}

/**
 * Save PWA assets to hashtree
 * Returns the nhash URL for the root directory
 */
export async function savePWAToHashtree(pwaInfo: PWAInfo): Promise<string> {
  const tree = getTree();

  // Store all files and build directory entries
  const fileCids = new Map<string, { cid: CID; size: number }>();

  for (const asset of pwaInfo.assets) {
    const { cid, size } = await tree.putFile(asset.data);
    fileCids.set(asset.path, { cid, size });
  }

  // Build directory structure from deepest to shallowest
  const dirPaths = getDirectoryPaths(pwaInfo.assets);
  const dirCids = new Map<string, CID>();

  // Process directories from deepest to shallowest
  const sortedDirs = [...dirPaths].reverse();

  for (const dirPath of sortedDirs) {
    const entries: DirectoryEntry[] = [];

    // Add files in this directory
    for (const asset of pwaInfo.assets) {
      const parts = asset.path.split('/');
      const fileName = parts.pop()!;
      const assetDir = parts.join('/') || '';

      if (assetDir === dirPath) {
        const fileInfo = fileCids.get(asset.path)!;
        entries.push({
          name: fileName,
          cid: fileInfo.cid,
          size: fileInfo.size,
          type: LinkType.Blob,
        });
      }
    }

    // Add subdirectories
    for (const [subDirPath, subDirCid] of dirCids) {
      const parts = subDirPath.split('/');
      const dirName = parts.pop()!;
      const parentPath = parts.join('/') || '';

      if (parentPath === dirPath) {
        entries.push({
          name: dirName,
          cid: subDirCid,
          size: 0,
          type: LinkType.Dir,
        });
      }
    }

    // Create directory
    const { cid } = await tree.putDirectory(entries);
    dirCids.set(dirPath, cid);
  }

  // Create root directory with top-level files and directories
  const rootEntries: DirectoryEntry[] = [];

  // Add root-level files
  for (const asset of pwaInfo.assets) {
    if (!asset.path.includes('/')) {
      const fileInfo = fileCids.get(asset.path)!;
      rootEntries.push({
        name: asset.path,
        cid: fileInfo.cid,
        size: fileInfo.size,
        type: LinkType.Blob,
      });
    }
  }

  // Add top-level directories
  for (const [dirPath, dirCid] of dirCids) {
    if (!dirPath.includes('/')) {
      rootEntries.push({
        name: dirPath,
        cid: dirCid,
        size: 0,
        type: LinkType.Dir,
      });
    }
  }

  // Create root directory
  const { cid: rootCid } = await tree.putDirectory(rootEntries);

  // Generate nhash URL
  const nhash = nhashEncode(rootCid);

  return `/${nhash}`;
}
