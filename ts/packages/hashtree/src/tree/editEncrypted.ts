/**
 * Encrypted tree editing operations
 *
 * All operations take a root key and return new hash + key.
 * Keys are propagated through the tree structure.
 */

import { Store, Hash, LinkType } from '../types.js';
export { LinkType };
import { type EncryptionKey } from '../crypto.js';
import {
  putDirectoryEncrypted,
  listDirectoryEncrypted,
  type EncryptedDirEntry,
  type EncryptedTreeConfig,
} from '../encrypted.js';

export interface EncryptedEditConfig extends EncryptedTreeConfig {}

/**
 * Result of an encrypted edit operation
 */
export interface EncryptedEditResult {
  hash: Hash;
  key: EncryptionKey;
}

/**
 * Path resolution result with collected keys
 */
interface PathResolution {
  dirHash: Hash;
  dirKey: EncryptionKey;
  pathKeys: EncryptionKey[];
}

/**
 * Resolve a path and collect keys along the way
 */
async function resolvePathAndCollectKeys(
  store: Store,
  rootHash: Hash,
  rootKey: EncryptionKey,
  path: string[]
): Promise<PathResolution | null> {
  const pathKeys: EncryptionKey[] = [];
  let currentHash = rootHash;
  let currentKey = rootKey;

  for (const segment of path) {
    const entries = await listDirectoryEncrypted(store, currentHash, currentKey);
    const entry = entries.find(e => e.name === segment);

    if (!entry || !entry.key) {
      return null;
    }

    pathKeys.push(entry.key);
    currentHash = entry.hash;
    currentKey = entry.key;
  }

  return { dirHash: currentHash, dirKey: currentKey, pathKeys };
}

/**
 * Add or update an entry in an encrypted directory
 * @param config - Tree configuration
 * @param rootHash - Current root hash
 * @param rootKey - Current root key
 * @param path - Path to the directory containing the entry
 * @param name - Name of the entry to add/update
 * @param hash - Hash of the entry content
 * @param size - Size of the entry content
 * @param key - Encryption key of the entry (for encrypted content)
 * @param type - LinkType of the entry (Blob, File, or Dir)
 * @returns New root hash and key
 */
export async function setEntryEncrypted(
  config: EncryptedEditConfig,
  rootHash: Hash,
  rootKey: EncryptionKey,
  path: string[],
  name: string,
  hash: Hash,
  size: number,
  key?: EncryptionKey,
  type: LinkType = LinkType.Blob,
  meta?: Record<string, unknown>
): Promise<EncryptedEditResult> {
  const { store } = config;

  // Navigate to the target directory and collect keys
  const resolved = await resolvePathAndCollectKeys(store, rootHash, rootKey, path);

  if (!resolved) {
    throw new Error(`Path not found: ${path.join('/')}`);
  }

  const { dirHash, dirKey, pathKeys } = resolved;

  // List current entries
  const entries = await listDirectoryEncrypted(store, dirHash, dirKey);

  // Filter out existing entry and add new one
  const newEntries: EncryptedDirEntry[] = entries
    .filter(e => e.name !== name)
    .map(e => ({
      name: e.name,
      hash: e.hash,
      size: e.size,
      key: e.key,
      type: e.type,
      meta: e.meta,
    }));

  newEntries.push({ name, hash, size, key, type, meta });

  // Create new encrypted directory
  const newDir = await putDirectoryEncrypted(config, newEntries);

  // Rebuild the path with new directory
  return rebuildPathEncrypted(
    config,
    rootHash,
    rootKey,
    path,
    pathKeys,
    newDir.hash,
    newDir.key
  );
}

/**
 * Remove an entry from an encrypted directory
 */
export async function removeEntryEncrypted(
  config: EncryptedEditConfig,
  rootHash: Hash,
  rootKey: EncryptionKey,
  path: string[],
  name: string
): Promise<EncryptedEditResult> {
  const { store } = config;

  const resolved = await resolvePathAndCollectKeys(store, rootHash, rootKey, path);

  if (!resolved) {
    throw new Error(`Path not found: ${path.join('/')}`);
  }

  const { dirHash, dirKey, pathKeys } = resolved;

  const entries = await listDirectoryEncrypted(store, dirHash, dirKey);
  const newEntries: EncryptedDirEntry[] = entries
    .filter(e => e.name !== name)
    .map(e => ({
      name: e.name,
      hash: e.hash,
      size: e.size,
      key: e.key,
      type: e.type,
      meta: e.meta,
    }));

  const newDir = await putDirectoryEncrypted(config, newEntries);

  return rebuildPathEncrypted(
    config,
    rootHash,
    rootKey,
    path,
    pathKeys,
    newDir.hash,
    newDir.key
  );
}

/**
 * Rename an entry in an encrypted directory
 */
export async function renameEntryEncrypted(
  config: EncryptedEditConfig,
  rootHash: Hash,
  rootKey: EncryptionKey,
  path: string[],
  oldName: string,
  newName: string
): Promise<EncryptedEditResult> {
  if (oldName === newName) {
    return { hash: rootHash, key: rootKey };
  }

  const { store } = config;

  const resolved = await resolvePathAndCollectKeys(store, rootHash, rootKey, path);

  if (!resolved) {
    throw new Error(`Path not found: ${path.join('/')}`);
  }

  const { dirHash, dirKey, pathKeys } = resolved;

  const entries = await listDirectoryEncrypted(store, dirHash, dirKey);
  const entry = entries.find(e => e.name === oldName);
  if (!entry) {
    throw new Error(`Entry not found: ${oldName}`);
  }

  const newEntries: EncryptedDirEntry[] = entries
    .filter(e => e.name !== oldName)
    .map(e => ({
      name: e.name,
      hash: e.hash,
      size: e.size,
      key: e.key,
      type: e.type,
      meta: e.meta,
    }));

  newEntries.push({
    name: newName,
    hash: entry.hash,
    size: entry.size,
    key: entry.key,
    type: entry.type,
    meta: entry.meta,
  });

  const newDir = await putDirectoryEncrypted(config, newEntries);

  return rebuildPathEncrypted(
    config,
    rootHash,
    rootKey,
    path,
    pathKeys,
    newDir.hash,
    newDir.key
  );
}

/**
 * Move an entry from one directory to another in an encrypted tree
 */
export async function moveEntryEncrypted(
  config: EncryptedEditConfig,
  rootHash: Hash,
  rootKey: EncryptionKey,
  sourcePath: string[],
  name: string,
  targetPath: string[]
): Promise<EncryptedEditResult> {
  const { store } = config;

  // Resolve source directory
  const sourceResolved = await resolvePathAndCollectKeys(store, rootHash, rootKey, sourcePath);
  if (!sourceResolved) {
    throw new Error(`Source path not found: ${sourcePath.join('/')}`);
  }

  // Get the entry to move
  const sourceEntries = await listDirectoryEncrypted(store, sourceResolved.dirHash, sourceResolved.dirKey);
  const entryToMove = sourceEntries.find(e => e.name === name);
  if (!entryToMove) {
    throw new Error(`Entry not found: ${name}`);
  }

  // Remove from source
  const afterRemove = await removeEntryEncrypted(config, rootHash, rootKey, sourcePath, name);

  // Resolve target directory in the modified tree
  const targetResolved = await resolvePathAndCollectKeys(store, afterRemove.hash, afterRemove.key, targetPath);
  if (!targetResolved) {
    throw new Error(`Target path not found: ${targetPath.join('/')}`);
  }

  // Add to target
  const targetEntries = await listDirectoryEncrypted(store, targetResolved.dirHash, targetResolved.dirKey);

  // Check for name collision
  if (targetEntries.some(e => e.name === name)) {
    throw new Error(`Entry already exists in target: ${name}`);
  }

  const newTargetEntries: EncryptedDirEntry[] = targetEntries.map(e => ({
    name: e.name,
    hash: e.hash,
    size: e.size,
    key: e.key,
    type: e.type,
    meta: e.meta,
  }));

  newTargetEntries.push({
    name: entryToMove.name,
    hash: entryToMove.hash,
    size: entryToMove.size,
    key: entryToMove.key,
    type: entryToMove.type,
    meta: entryToMove.meta,
  });

  const newTargetDir = await putDirectoryEncrypted(config, newTargetEntries);

  return rebuildPathEncrypted(
    config,
    afterRemove.hash,
    afterRemove.key,
    targetPath,
    targetResolved.pathKeys,
    newTargetDir.hash,
    newTargetDir.key
  );
}

/**
 * Rebuild the path from a modified child up to the root
 */
async function rebuildPathEncrypted(
  config: EncryptedEditConfig,
  rootHash: Hash,
  rootKey: EncryptionKey,
  path: string[],
  pathKeys: EncryptionKey[],
  newChildHash: Hash,
  newChildKey: EncryptionKey
): Promise<EncryptedEditResult> {
  if (path.length === 0) {
    return { hash: newChildHash, key: newChildKey };
  }

  const { store } = config;
  let childHash = newChildHash;
  let childKey = newChildKey;
  const parts = [...path];

  while (parts.length > 0) {
    const childName = parts.pop()!;

    // Get parent directory
    let parentHash: Hash;
    let parentKey: EncryptionKey;

    if (parts.length === 0) {
      parentHash = rootHash;
      parentKey = rootKey;
    } else {
      // Use collected pathKeys to find the parent
      parentHash = rootHash;
      parentKey = rootKey;
      for (let i = 0; i < parts.length; i++) {
        const entries = await listDirectoryEncrypted(store, parentHash, parentKey);
        const entry = entries.find(e => e.name === parts[i]);
        if (!entry || !entry.key) {
          throw new Error(`Parent path not found: ${parts.join('/')}`);
        }
        parentHash = entry.hash;
        parentKey = pathKeys[i] ?? entry.key;
      }
    }

    // Get parent entries and update the child
    const parentEntries = await listDirectoryEncrypted(store, parentHash, parentKey);
    const newParentEntries: EncryptedDirEntry[] = parentEntries.map(e =>
      e.name === childName
        ? { name: e.name, hash: childHash, size: e.size, key: childKey, type: e.type, meta: e.meta }
        : { name: e.name, hash: e.hash, size: e.size, key: e.key, type: e.type, meta: e.meta }
    );

    const newParent = await putDirectoryEncrypted(config, newParentEntries);
    childHash = newParent.hash;
    childKey = newParent.key;
  }

  return { hash: childHash, key: childKey };
}
