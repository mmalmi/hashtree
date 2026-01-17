/**
 * Tree editing operations
 */

import { Store, Hash, CID, cid, LinkType } from '../types.js';
import { putDirectory, type DirEntry, type CreateConfig } from './create.js';
import { listDirectory, resolvePath } from './read.js';

export interface EditConfig extends CreateConfig {}

/**
 * Add or update an entry in a directory
 * @returns New root hash
 */
export async function setEntry(
  config: EditConfig,
  rootHash: Hash,
  path: string[],
  name: string,
  entryCid: CID,
  size: number,
  type: LinkType = LinkType.Blob,
  meta?: Record<string, unknown>
): Promise<Hash> {
  const { store } = config;
  const dirHash = await resolvePathArray(store, rootHash, path);
  if (!dirHash) {
    throw new Error(`Path not found: ${path.join('/')}`);
  }

  const entries = await listDirectory(store, dirHash);
  const newEntries: DirEntry[] = entries
    .filter(e => e.name !== name)
    .map(e => ({ name: e.name, cid: e.cid, size: e.size, type: e.type, meta: e.meta }));

  newEntries.push({ name, cid: entryCid, size, type, meta });

  const newDirHash = await putDirectory(config, newEntries);
  return rebuildPath(config, rootHash, path, newDirHash);
}

/**
 * Remove an entry from a directory
 * @returns New root hash
 */
export async function removeEntry(
  config: EditConfig,
  rootHash: Hash,
  path: string[],
  name: string
): Promise<Hash> {
  const { store } = config;
  const dirHash = await resolvePathArray(store, rootHash, path);
  if (!dirHash) {
    throw new Error(`Path not found: ${path.join('/')}`);
  }

  const entries = await listDirectory(store, dirHash);
  const newEntries: DirEntry[] = entries
    .filter(e => e.name !== name)
    .map(e => ({ name: e.name, cid: e.cid, size: e.size, type: e.type, meta: e.meta }));

  const newDirHash = await putDirectory(config, newEntries);
  return rebuildPath(config, rootHash, path, newDirHash);
}

/**
 * Rename an entry in a directory
 * @returns New root hash
 */
export async function renameEntry(
  config: EditConfig,
  rootHash: Hash,
  path: string[],
  oldName: string,
  newName: string
): Promise<Hash> {
  if (oldName === newName) return rootHash;

  const { store } = config;
  const dirHash = await resolvePathArray(store, rootHash, path);
  if (!dirHash) {
    throw new Error(`Path not found: ${path.join('/')}`);
  }

  const entries = await listDirectory(store, dirHash);
  const entry = entries.find(e => e.name === oldName);
  if (!entry) {
    throw new Error(`Entry not found: ${oldName}`);
  }

  const newEntries: DirEntry[] = entries
    .filter(e => e.name !== oldName)
    .map(e => ({ name: e.name, cid: e.cid, size: e.size, type: e.type, meta: e.meta }));

  newEntries.push({ name: newName, cid: entry.cid, size: entry.size, type: entry.type, meta: entry.meta });

  const newDirHash = await putDirectory(config, newEntries);
  return rebuildPath(config, rootHash, path, newDirHash);
}

/**
 * Move an entry to a different directory
 * @returns New root hash
 */
export async function moveEntry(
  config: EditConfig,
  rootHash: Hash,
  sourcePath: string[],
  name: string,
  targetPath: string[]
): Promise<Hash> {
  const { store } = config;
  const sourceDirHash = await resolvePathArray(store, rootHash, sourcePath);
  if (!sourceDirHash) {
    throw new Error(`Source path not found: ${sourcePath.join('/')}`);
  }

  const sourceEntries = await listDirectory(store, sourceDirHash);
  const entry = sourceEntries.find(e => e.name === name);
  if (!entry) {
    throw new Error(`Entry not found: ${name}`);
  }

  // Remove from source
  let newRoot = await removeEntry(config, rootHash, sourcePath, name);

  // Add to target (preserving CID with encryption key)
  newRoot = await setEntry(
    config,
    newRoot,
    targetPath,
    name,
    entry.cid,
    entry.size,
    entry.type
  );

  return newRoot;
}

async function resolvePathArray(store: Store, rootHash: Hash, path: string[]): Promise<Hash | null> {
  if (path.length === 0) return rootHash;
  return resolvePath(store, rootHash, path.join('/'));
}

async function rebuildPath(
  config: EditConfig,
  rootHash: Hash,
  path: string[],
  newChildHash: Hash
): Promise<Hash> {
  if (path.length === 0) {
    return newChildHash;
  }

  const { store } = config;
  let childHash = newChildHash;
  const parts = [...path];

  while (parts.length > 0) {
    const childName = parts.pop()!;

    const parentHash = parts.length === 0
      ? rootHash
      : await resolvePathArray(store, rootHash, parts);

    if (!parentHash) {
      throw new Error(`Parent path not found: ${parts.join('/')}`);
    }

    const parentEntries = await listDirectory(store, parentHash);
    const newParentEntries: DirEntry[] = parentEntries.map(e =>
      e.name === childName
        ? { name: e.name, cid: cid(childHash), size: e.size, type: e.type, meta: e.meta }
        : { name: e.name, cid: e.cid, size: e.size, type: e.type, meta: e.meta }
    );

    childHash = await putDirectory(config, newParentEntries);
  }

  return childHash;
}
