/**
 * Git log and history operations
 */
import type { CID } from '@hashtree/core';
import { LinkType, toHex } from '@hashtree/core';
import { getTree } from '../../store';
import { withWasmGitLock, loadWasmGit, copyGitDirToWasmFS, rmRf, createRepoPath } from './core';

/**
 * Get current HEAD commit SHA
 * Reads .git/HEAD and resolves refs directly from hashtree - no wasm needed
 */
export async function getHead(
  rootCid: CID
): Promise<string | null> {
  const tree = getTree();
  const isSha = (value: string): boolean => /^[0-9a-f]{40}$/.test(value);

  // Check for .git directory
  const gitDirResult = await tree.resolvePath(rootCid, '.git');
  if (!gitDirResult || gitDirResult.type !== LinkType.Dir) {
    return null;
  }

  const readRefSha = async (refPath: string): Promise<string | null> => {
    const refResult = await tree.resolvePath(gitDirResult.cid, refPath);
    if (!refResult || refResult.type === LinkType.Dir) {
      return null;
    }
    const refData = await tree.readFile(refResult.cid);
    if (!refData) {
      return null;
    }
    const sha = new TextDecoder().decode(refData).trim();
    return isSha(sha) ? sha : null;
  };

  const readPackedRefs = async (): Promise<Map<string, string>> => {
    const refs = new Map<string, string>();
    const packedRefsResult = await tree.resolvePath(gitDirResult.cid, 'packed-refs');
    if (!packedRefsResult || packedRefsResult.type === LinkType.Dir) {
      return refs;
    }
    const packedRefsData = await tree.readFile(packedRefsResult.cid);
    if (!packedRefsData) {
      return refs;
    }
    const lines = new TextDecoder().decode(packedRefsData).split('\n');
    for (const rawLine of lines) {
      const line = rawLine.trim();
      if (!line || line.startsWith('#') || line.startsWith('^')) {
        continue;
      }
      const [sha, ref] = line.split(' ');
      if (sha && ref) {
        refs.set(ref, sha);
      }
    }
    return refs;
  };

  const fallbackHead = async (): Promise<string | null> => {
    const preferredBranches = ['main', 'master'];
    for (const branch of preferredBranches) {
      const sha = await readRefSha(`refs/heads/${branch}`);
      if (sha) {
        return sha;
      }
    }

    try {
      const refsResult = await tree.resolvePath(gitDirResult.cid, 'refs/heads');
      if (refsResult && refsResult.type === LinkType.Dir) {
        const entries = await tree.listDirectory(refsResult.cid);
        const branchNames = entries
          .filter(entry => entry.type !== LinkType.Dir)
          .map(entry => entry.name)
          .sort();
        for (const name of branchNames) {
          const sha = await readRefSha(`refs/heads/${name}`);
          if (sha) {
            return sha;
          }
        }
      }
    } catch {
      // Ignore fallback errors
    }

    const packedRefs = await readPackedRefs();
    for (const ref of ['refs/heads/main', 'refs/heads/master']) {
      const sha = packedRefs.get(ref);
      if (sha && isSha(sha)) {
        return sha;
      }
    }
    for (const [ref, sha] of packedRefs.entries()) {
      if (ref.startsWith('refs/heads/') && isSha(sha)) {
        return sha;
      }
    }

    return null;
  };

  try {
    // Read HEAD file
    const headResult = await tree.resolvePath(gitDirResult.cid, 'HEAD');
    if (!headResult || headResult.type === LinkType.Dir) {
      return await fallbackHead();
    }

    const headData = await tree.readFile(headResult.cid);
    if (!headData) {
      return await fallbackHead();
    }

    const headContent = new TextDecoder().decode(headData).trim();

    // Check if HEAD is a direct SHA (detached)
    if (isSha(headContent)) {
      return headContent;
    }

    // HEAD is a ref like "ref: refs/heads/master"
    const refMatch = headContent.match(/^ref: (.+)$/);
    if (!refMatch) {
      return await fallbackHead();
    }

    // Resolve the ref to get commit SHA
    const refPath = refMatch[1]; // e.g., "refs/heads/master"
    const sha = await readRefSha(refPath);
    if (sha) {
      return sha;
    }

    return await fallbackHead();
  } catch (err) {
    console.error('[git] getHead failed:', err);
    return await fallbackHead();
  }
}

export interface CommitInfo {
  oid: string;
  message: string;
  author: string;
  email: string;
  timestamp: number;
  parent: string[];
}

/**
 * Get commit log using wasm-git
 */
/**
 * Decompress zlib data using browser's DecompressionStream
 */
async function decompressZlib(data: Uint8Array): Promise<Uint8Array> {
  const ds = new DecompressionStream('deflate');
  const writer = ds.writable.getWriter();
  writer.write(data);
  writer.close();

  const chunks: Uint8Array[] = [];
  const reader = ds.readable.getReader();
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
  }

  const totalLength = chunks.reduce((sum, chunk) => sum + chunk.length, 0);
  const result = new Uint8Array(totalLength);
  let offset = 0;
  for (const chunk of chunks) {
    result.set(chunk, offset);
    offset += chunk.length;
  }
  return result;
}

function gitDirCacheKey(gitDirCid: CID): string {
  return gitDirCid.key ? `${toHex(gitDirCid.hash)}:${toHex(gitDirCid.key)}` : toHex(gitDirCid.hash);
}

// Cache for pack indexes and pack data per git dir CID
const packIndexCache = new Map<string, Map<string, { shas: string[]; offsets: number[]; shaToOffset: Map<string, number> }>>();
const packDataCache = new Map<string, Map<string, Uint8Array>>();

/**
 * Load pack index file (.idx) and return the SHA -> offset mapping
 */
async function loadPackIndex(
  tree: ReturnType<typeof getTree>,
  gitDirCid: CID,
  idxName: string
): Promise<{ fanout: Uint32Array; shas: string[]; offsets: number[]; shaToOffset: Map<string, number> } | null> {
  // Check cache first
  const cacheKey = gitDirCacheKey(gitDirCid);
  let dirCache = packIndexCache.get(cacheKey);
  if (dirCache?.has(idxName)) {
    const cached = dirCache.get(idxName)!;
    return { fanout: new Uint32Array(256), shas: cached.shas, offsets: cached.offsets, shaToOffset: cached.shaToOffset };
  }
  try {
    const idxResult = await tree.resolvePath(gitDirCid, `objects/pack/${idxName}`);
    if (!idxResult || idxResult.type === LinkType.Dir) {
      return null;
    }

    const idxData = await tree.readFile(idxResult.cid);
    if (!idxData) return null;

    const view = new DataView(idxData.buffer, idxData.byteOffset, idxData.byteLength);

    // Check magic number (0xff744f63 for v2)
    if (view.getUint32(0) !== 0xff744f63) {
      
      return null;
    }

    // Version should be 2
    if (view.getUint32(4) !== 2) {
      
      return null;
    }

    // Fanout table (256 entries, 4 bytes each) starts at offset 8
    const fanout = new Uint32Array(256);
    for (let i = 0; i < 256; i++) {
      fanout[i] = view.getUint32(8 + i * 4);
    }

    const numObjects = fanout[255];

    // SHA table starts after fanout (offset 8 + 256*4 = 1032)
    const shaOffset = 8 + 256 * 4;
    const shas: string[] = [];
    for (let i = 0; i < numObjects; i++) {
      const sha = Array.from(idxData.slice(shaOffset + i * 20, shaOffset + (i + 1) * 20))
        .map(b => b.toString(16).padStart(2, '0'))
        .join('');
      shas.push(sha);
    }

    // CRC table (skip it) - numObjects * 4 bytes
    const crcOffset = shaOffset + numObjects * 20;

    // Offset table starts after CRC
    const offsetOffset = crcOffset + numObjects * 4;
    const offsets: number[] = [];
    for (let i = 0; i < numObjects; i++) {
      offsets.push(view.getUint32(offsetOffset + i * 4));
    }

    // Build SHA -> offset map for fast lookups
    const shaToOffset = new Map<string, number>();
    for (let i = 0; i < shas.length; i++) {
      shaToOffset.set(shas[i], offsets[i]);
    }

    // Cache the result
    if (!dirCache) {
      dirCache = new Map();
      packIndexCache.set(cacheKey, dirCache);
    }
    dirCache.set(idxName, { shas, offsets, shaToOffset });

    return { fanout, shas, offsets, shaToOffset };
  } catch {
    return null;
  }
}

/**
 * Load pack file data with caching
 */
async function loadPackData(
  tree: ReturnType<typeof getTree>,
  gitDirCid: CID,
  packName: string
): Promise<Uint8Array | null> {
  const cacheKey = gitDirCacheKey(gitDirCid);
  let dirCache = packDataCache.get(cacheKey);
  if (dirCache?.has(packName)) {
    return dirCache.get(packName)!;
  }

  const packResult = await tree.resolvePath(gitDirCid, `objects/pack/${packName}`);
  if (!packResult || packResult.type === LinkType.Dir) return null;

  const packData = await tree.readFile(packResult.cid);
  if (!packData) return null;

  // Cache the result
  if (!dirCache) {
    dirCache = new Map();
    packDataCache.set(cacheKey, dirCache);
  }
  dirCache.set(packName, packData);

  return packData;
}

// Cache for pack directory listings
const packDirCache = new Map<string, string[]>();

/**
 * Find object in pack files
 */
async function findInPack(
  tree: ReturnType<typeof getTree>,
  gitDirCid: CID,
  sha: string
): Promise<{ packName: string; offset: number } | null> {
  const cacheKey = gitDirCacheKey(gitDirCid);

  // Get cached list of idx files or load it
  let idxFileNames = packDirCache.get(cacheKey);
  if (!idxFileNames) {
    const packDirResult = await tree.resolvePath(gitDirCid, 'objects/pack');
    if (!packDirResult || packDirResult.type !== LinkType.Dir) {
      return null;
    }
    const entries = await tree.listDirectory(packDirResult.cid);
    idxFileNames = entries.filter(e => e.name.endsWith('.idx')).map(e => e.name);
    packDirCache.set(cacheKey, idxFileNames);
  }

  for (const idxName of idxFileNames) {
    const idx = await loadPackIndex(tree, gitDirCid, idxName);
    if (!idx) continue;

    // Use the shaToOffset map for O(1) lookup instead of O(n) indexOf
    const offset = idx.shaToOffset.get(sha);
    if (offset !== undefined) {
      const packName = idxName.replace('.idx', '.pack');
      return { packName, offset };
    }
  }

  return null;
}

/**
 * Apply git delta instructions to a base object
 */
function applyDelta(base: Uint8Array, delta: Uint8Array): Uint8Array {
  let pos = 0;

  // Skip base size (variable length encoded, not needed for our purposes)
  let shift = 0;
  while (pos < delta.length) {
    const byte = delta[pos++];
    shift += 7;
    if (!(byte & 0x80)) break;
  }

  // Read result size (variable length)
  let resultSize = 0;
  shift = 0;
  while (pos < delta.length) {
    const byte = delta[pos++];
    resultSize |= (byte & 0x7f) << shift;
    shift += 7;
    if (!(byte & 0x80)) break;
  }

  const result = new Uint8Array(resultSize);
  let resultPos = 0;

  while (pos < delta.length && resultPos < resultSize) {
    const cmd = delta[pos++];

    if (cmd & 0x80) {
      // Copy from base
      let copyOffset = 0;
      let copySize = 0;

      if (cmd & 0x01) copyOffset = delta[pos++];
      if (cmd & 0x02) copyOffset |= delta[pos++] << 8;
      if (cmd & 0x04) copyOffset |= delta[pos++] << 16;
      if (cmd & 0x08) copyOffset |= delta[pos++] << 24;

      if (cmd & 0x10) copySize = delta[pos++];
      if (cmd & 0x20) copySize |= delta[pos++] << 8;
      if (cmd & 0x40) copySize |= delta[pos++] << 16;

      if (copySize === 0) copySize = 0x10000;

      result.set(base.subarray(copyOffset, copyOffset + copySize), resultPos);
      resultPos += copySize;
    } else if (cmd > 0) {
      // Insert new data
      result.set(delta.subarray(pos, pos + cmd), resultPos);
      pos += cmd;
      resultPos += cmd;
    }
  }

  return result;
}

/**
 * Read object from pack file at given offset (with delta resolution)
 */
async function readFromPack(
  tree: ReturnType<typeof getTree>,
  gitDirCid: CID,
  packName: string,
  offset: number,
  depth = 0
): Promise<{ type: string; content: Uint8Array } | null> {
  // Prevent infinite recursion
  if (depth > 50) return null;

  try {
    const packData = await loadPackData(tree, gitDirCid, packName);
    if (!packData) return null;

    // Read object header at offset
    let pos = offset;
    let byte = packData[pos++];
    const type = (byte >> 4) & 7;
    let size = byte & 15;
    let shift = 4;

    while (byte & 0x80) {
      byte = packData[pos++];
      size |= (byte & 0x7f) << shift;
      shift += 7;
    }

    // Type mapping: 1=commit, 2=tree, 3=blob, 4=tag, 6=ofs_delta, 7=ref_delta
    const typeNames = ['', 'commit', 'tree', 'blob', 'tag', '', 'ofs_delta', 'ref_delta'];

    if (type === 6) {
      // OFS_DELTA: read negative offset to base object
      let negOffset = 0;
      byte = packData[pos++];
      negOffset = byte & 0x7f;
      while (byte & 0x80) {
        byte = packData[pos++];
        negOffset = ((negOffset + 1) << 7) | (byte & 0x7f);
      }
      const baseOffset = offset - negOffset;

      // Decompress delta data
      const compressedDelta = packData.slice(pos);
      const deltaData = await decompressZlib(compressedDelta);

      // Recursively read base object
      const baseObj = await readFromPack(tree, gitDirCid, packName, baseOffset, depth + 1);
      if (!baseObj) return null;

      // Apply delta to base
      const content = applyDelta(baseObj.content, deltaData.slice(0, size > deltaData.length ? deltaData.length : undefined));
      return { type: baseObj.type, content };
    }

    if (type === 7) {
      // REF_DELTA: read 20-byte SHA of base object
      const baseShaBytes = packData.slice(pos, pos + 20);
      pos += 20;
      const baseSha = Array.from(baseShaBytes).map(b => b.toString(16).padStart(2, '0')).join('');

      // Decompress delta data
      const compressedDelta = packData.slice(pos);
      const deltaData = await decompressZlib(compressedDelta);

      // Find and read base object (could be in any pack or loose)
      const baseObj = await readGitObjectInternal(tree, gitDirCid, baseSha, depth + 1);
      if (!baseObj) return null;

      // Apply delta to base
      const content = applyDelta(baseObj.content, deltaData.slice(0, size > deltaData.length ? deltaData.length : undefined));
      return { type: baseObj.type, content };
    }

    // Regular object - decompress
    const compressedData = packData.slice(pos);
    const decompressed = await decompressZlib(compressedData);

    return { type: typeNames[type], content: decompressed.slice(0, size) };
  } catch {
    return null;
  }
}

/**
 * Read and parse a git object from hashtree (loose or packed)
 * Internal version with depth tracking for delta resolution
 */
async function readGitObjectInternal(
  tree: ReturnType<typeof getTree>,
  gitDirCid: CID,
  sha: string,
  depth = 0
): Promise<{ type: string; content: Uint8Array } | null> {
  // Prevent infinite recursion
  if (depth > 50) return null;

  // Try loose object first: .git/objects/<sha[0:2]>/<sha[2:]>
  const objPath = `objects/${sha.slice(0, 2)}/${sha.slice(2)}`;

  try {
    const objResult = await tree.resolvePath(gitDirCid, objPath);
    if (objResult && objResult.type !== LinkType.Dir) {
      const compressedData = await tree.readFile(objResult.cid);
      if (compressedData) {
        // Decompress the object
        const decompressed = await decompressZlib(compressedData);

        // Parse: "<type> <size>\0<content>"
        const nullIndex = decompressed.indexOf(0);
        if (nullIndex !== -1) {
          const header = new TextDecoder().decode(decompressed.slice(0, nullIndex));
          const [type] = header.split(' ');
          const content = decompressed.slice(nullIndex + 1);
          return { type, content };
        }
      }
    }
  } catch {
    // Loose object not found, try pack files
  }

  // Try pack files
  const packInfo = await findInPack(tree, gitDirCid, sha);
  if (packInfo) {
    return readFromPack(tree, gitDirCid, packInfo.packName, packInfo.offset, depth);
  }

  return null;
}

/**
 * Read and parse a git object from hashtree (loose or packed)
 */
async function readGitObject(
  tree: ReturnType<typeof getTree>,
  gitDirCid: CID,
  sha: string
): Promise<{ type: string; content: Uint8Array } | null> {
  return readGitObjectInternal(tree, gitDirCid, sha, 0);
}

/**
 * Parse a git commit object
 */
function parseCommit(content: Uint8Array): {
  tree: string;
  parents: string[];
  author: string;
  email: string;
  timestamp: number;
  message: string;
} | null {
  const text = new TextDecoder().decode(content);
  const lines = text.split('\n');

  let tree = '';
  const parents: string[] = [];
  let author = '';
  let email = '';
  let timestamp = 0;
  let messageStart = -1;

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];

    if (line === '') {
      messageStart = i + 1;
      break;
    }

    if (line.startsWith('tree ')) {
      tree = line.slice(5);
    } else if (line.startsWith('parent ')) {
      parents.push(line.slice(7));
    } else if (line.startsWith('author ')) {
      // Format: "author Name <email> timestamp timezone"
      const match = line.match(/^author (.+) <(.+)> (\d+)/);
      if (match) {
        author = match[1];
        email = match[2];
        timestamp = parseInt(match[3], 10);
      }
    }
  }

  const message = messageStart >= 0 ? lines.slice(messageStart).join('\n').trim() : '';

  return { tree, parents, author, email, timestamp, message };
}

// Cache for preloaded commit objects (sha -> parsed commit)
const commitCache = new Map<string, Map<string, { type: string; content: Uint8Array }>>();

/**
 * Preload all commit objects from pack files in a single pass
 * Much faster than fetching commits one by one
 */
async function preloadCommitsFromPacks(
  tree: ReturnType<typeof getTree>,
  gitDirCid: CID
): Promise<Map<string, { type: string; content: Uint8Array }>> {
  const cacheKey = gitDirCacheKey(gitDirCid);
  if (commitCache.has(cacheKey)) {
    return commitCache.get(cacheKey)!;
  }

  const objects = new Map<string, { type: string; content: Uint8Array }>();

  try {
    const packDirResult = await tree.resolvePath(gitDirCid, 'objects/pack');
    if (!packDirResult || packDirResult.type !== LinkType.Dir) {
      commitCache.set(cacheKey, objects);
      return objects;
    }

    const entries = await tree.listDirectory(packDirResult.cid);
    const idxFiles = entries.filter(e => e.name.endsWith('.idx'));

    for (const idxFile of idxFiles) {
      const packName = idxFile.name.replace('.idx', '.pack');

      const idx = await loadPackIndex(tree, gitDirCid, idxFile.name);
      if (!idx) continue;

      const packData = await loadPackData(tree, gitDirCid, packName);
      if (!packData) continue;

      // Read all objects from pack (commits, trees, etc.)
      for (let i = 0; i < idx.shas.length; i++) {
        const sha = idx.shas[i];
        const offset = idx.offsets[i];

        try {
          const obj = await readPackObjectFull(packData, offset, idx, tree, gitDirCid, packName);
          if (obj) {
            objects.set(sha, obj);
          }
        } catch {
          // Skip objects that fail to parse
        }
      }
    }
  } catch (err) {
    console.error('[git] preloadCommitsFromPacks error:', err);
  }

  commitCache.set(cacheKey, objects);
  return objects;
}

/**
 * Read a full object from pack data (with delta resolution)
 */
async function readPackObjectFull(
  packData: Uint8Array,
  offset: number,
  idx: { shas: string[]; offsets: number[]; shaToOffset: Map<string, number> },
  tree: ReturnType<typeof getTree>,
  gitDirCid: CID,
  packName: string,
  depth = 0
): Promise<{ type: string; content: Uint8Array } | null> {
  if (depth > 50) return null;

  let pos = offset;
  let byte = packData[pos++];
  const type = (byte >> 4) & 7;
  let size = byte & 15;
  let shift = 4;

  while (byte & 0x80) {
    byte = packData[pos++];
    size |= (byte & 0x7f) << shift;
    shift += 7;
  }

  const typeNames = ['', 'commit', 'tree', 'blob', 'tag', '', 'ofs_delta', 'ref_delta'];

  if (type === 6) {
    // OFS_DELTA
    let negOffset = 0;
    byte = packData[pos++];
    negOffset = byte & 0x7f;
    while (byte & 0x80) {
      byte = packData[pos++];
      negOffset = ((negOffset + 1) << 7) | (byte & 0x7f);
    }
    const baseOffset = offset - negOffset;

    const compressedDelta = packData.slice(pos);
    const deltaData = await decompressZlib(compressedDelta);
    const baseObj = await readPackObjectFull(packData, baseOffset, idx, tree, gitDirCid, packName, depth + 1);
    if (!baseObj) return null;

    const content = applyDelta(baseObj.content, deltaData);
    return { type: baseObj.type, content };
  }

  if (type === 7) {
    // REF_DELTA
    const baseShaBytes = packData.slice(pos, pos + 20);
    pos += 20;
    const baseSha = Array.from(baseShaBytes).map(b => b.toString(16).padStart(2, '0')).join('');

    const compressedDelta = packData.slice(pos);
    const deltaData = await decompressZlib(compressedDelta);

    // Look for base in same pack first
    const baseOffset = idx.shaToOffset.get(baseSha);
    let baseObj: { type: string; content: Uint8Array } | null = null;
    if (baseOffset !== undefined) {
      baseObj = await readPackObjectFull(packData, baseOffset, idx, tree, gitDirCid, packName, depth + 1);
    } else {
      // Fall back to full object lookup
      baseObj = await readGitObjectInternal(tree, gitDirCid, baseSha, depth + 1);
    }
    if (!baseObj) return null;

    const content = applyDelta(baseObj.content, deltaData);
    return { type: baseObj.type, content };
  }

  // Regular object
  const compressedData = packData.slice(pos);
  const decompressed = await decompressZlib(compressedData);
  return { type: typeNames[type], content: decompressed.slice(0, size) };
}

/**
 * Get commit log by reading git objects directly from hashtree
 * No wasm-git needed - much faster for large repos
 * Uses parallel fetching for better performance
 */
export async function getLog(
  rootCid: CID,
  options?: { depth?: number }
): Promise<CommitInfo[]> {
  const tree = getTree();
  const depth = options?.depth ?? 20;

  // Check for .git directory
  const gitDirResult = await tree.resolvePath(rootCid, '.git');
  if (!gitDirResult || gitDirResult.type !== LinkType.Dir) {
    return [];
  }

  try {
    // Get HEAD commit SHA
    const headSha = await getHead(rootCid);
    if (!headSha) {
      return [];
    }

    // Preload all objects from pack files (single network fetch)
    const preloadedObjects = await preloadCommitsFromPacks(tree, gitDirResult.cid);

    const commits: CommitInfo[] = [];
    const visited = new Set<string>();
    const queue = [headSha];

    // Helper to get object from cache or fetch
    const getObject = async (sha: string): Promise<{ type: string; content: Uint8Array } | null> => {
      // Try preloaded cache first (fast path)
      const cached = preloadedObjects.get(sha);
      if (cached) return cached;
      // Fall back to individual fetch (loose objects or missing from pack)
      return readGitObject(tree, gitDirResult.cid, sha);
    };

    while (queue.length > 0 && commits.length < depth) {
      const sha = queue.shift()!;
      if (visited.has(sha)) continue;
      visited.add(sha);

      const obj = await getObject(sha);
      if (!obj || obj.type !== 'commit') continue;

      const parsed = parseCommit(obj.content);
      if (!parsed) continue;

      commits.push({
        oid: sha,
        message: parsed.message,
        author: parsed.author,
        email: parsed.email,
        timestamp: parsed.timestamp,
        parent: parsed.parents,
      });

      // Add parents to queue
      for (const parent of parsed.parents) {
        if (!visited.has(parent)) {
          queue.push(parent);
        }
      }
    }

    // Sort by timestamp (newest first)
    commits.sort((a, b) => b.timestamp - a.timestamp);

    return commits;
  } catch (err) {
    console.error('[git] getLog failed:', err);
    return [];
  }
}

/**
 * Fast commit count - traverses parent pointers without parsing full commit data
 */
export async function getCommitCount(
  rootCid: CID,
  options?: { maxCount?: number }
): Promise<number> {
  const tree = getTree();
  const maxCount = options?.maxCount ?? 10000;

  // Check for .git directory
  const gitDirResult = await tree.resolvePath(rootCid, '.git');
  if (!gitDirResult || gitDirResult.type !== LinkType.Dir) {
    return 0;
  }

  try {
    const headSha = await getHead(rootCid);
    if (!headSha) {
      return 0;
    }

    const visited = new Set<string>();
    const queue = [headSha];
    const BATCH_SIZE = 50;

    while (queue.length > 0 && visited.size < maxCount) {
      const batch = queue.splice(0, Math.min(BATCH_SIZE, maxCount - visited.size));
      const newShas = batch.filter(sha => !visited.has(sha));

      if (newShas.length === 0) continue;

      for (const sha of newShas) {
        visited.add(sha);
      }

      const results = await Promise.all(
        newShas.map(async (sha) => {
          const obj = await readGitObject(tree, gitDirResult.cid, sha);
          if (!obj || obj.type !== 'commit') return [];
          return extractParentShas(obj.content);
        })
      );

      for (const parents of results) {
        for (const parent of parents) {
          if (!visited.has(parent)) {
            queue.push(parent);
          }
        }
      }
    }

    return visited.size;
  } catch (err) {
    console.error('[git] getCommitCount failed:', err);
    return 0;
  }
}

/**
 * Fast parent SHA extraction - doesn't parse full commit
 */
function extractParentShas(content: Uint8Array): string[] {
  const text = new TextDecoder().decode(content);
  const parents: string[] = [];
  const lines = text.split('\n');

  for (const line of lines) {
    if (line === '') break;
    if (line.startsWith('parent ')) {
      parents.push(line.slice(7));
    }
  }

  return parents;
}

// Cache for commit counts (gitDirCid -> count)
const commitCountCache = new Map<string, number>();

/**
 * FAST commit count
 * - For pack files: scan type bytes directly (no decompression)
 * - For loose objects: walk commit graph from HEAD (only fetches commits, not all objects)
 * - Results are cached per git directory
 */
export async function getCommitCountFast(rootCid: CID): Promise<number> {
  const startTime = performance.now();
  const tree = getTree();

  const gitDirResult = await tree.resolvePath(rootCid, '.git');
  if (!gitDirResult || gitDirResult.type !== LinkType.Dir) {
    return 0;
  }

  // Check cache
  const cacheKey = gitDirResult.cid.key
    ? `${toHex(gitDirResult.cid.hash)}:${toHex(gitDirResult.cid.key)}`
    : toHex(gitDirResult.cid.hash);
  if (commitCountCache.has(cacheKey)) {
    const cached = commitCountCache.get(cacheKey)!;
    console.log(`[git perf] getCommitCountFast cache hit: ${cached} commits`);
    return cached;
  }

  let commitCount = 0;
  let hasPackFiles = false;
  let hasLooseObjects = false;

  try {
    const objectsDirResult = await tree.resolvePath(gitDirResult.cid, 'objects');
    if (objectsDirResult && objectsDirResult.type === LinkType.Dir) {
      const objectEntries = await tree.listDirectory(objectsDirResult.cid);
      hasLooseObjects = objectEntries.some(entry =>
        entry.type === LinkType.Dir && entry.name !== 'pack' && entry.name !== 'info'
      );
    }

    // 1. Count commits in pack files (most commits are packed)
    const packDirResult = await tree.resolvePath(gitDirResult.cid, 'objects/pack');
    if (packDirResult && packDirResult.type === LinkType.Dir) {
      const entries = await tree.listDirectory(packDirResult.cid);
      const idxFiles = entries.filter(e => e.name.endsWith('.idx'));

      for (const idxFile of idxFiles) {
        hasPackFiles = true;
        const packName = idxFile.name.replace('.idx', '.pack');

        // Load index to get offsets (uses cache)
        const idx = await loadPackIndex(tree, gitDirResult.cid, idxFile.name);
        if (!idx) continue;

        // Load pack file (uses cache)
        const packData = await loadPackData(tree, gitDirResult.cid, packName);
        if (!packData) continue;

        // Scan through all offsets, reading only type bytes (no decompression!)
        for (const offset of idx.offsets) {
          const type = readPackObjectType(packData, offset);
          if (type === 1) { // 1 = commit
            commitCount++;
          }
        }
      }
    }

    // 2. For loose objects: walk commit graph from HEAD (much faster than scanning all objects)
    if (!hasPackFiles) {
      const headSha = await getHead(rootCid);
      if (headSha) {
        const visited = new Set<string>();
        const queue = [headSha];
        const BATCH_SIZE = 50;

        while (queue.length > 0) {
          const batch = queue.splice(0, Math.min(BATCH_SIZE, queue.length));
          const newShas = batch.filter(sha => !visited.has(sha));

          if (newShas.length === 0) continue;

          for (const sha of newShas) {
            visited.add(sha);
          }

          // Fetch commits in parallel
          const results = await Promise.all(
            newShas.map(async (sha) => {
              const obj = await readGitObject(tree, gitDirResult.cid, sha);
              if (!obj || obj.type !== 'commit') return [];
              return extractParentShas(obj.content);
            })
          );

          for (const parents of results) {
            for (const parent of parents) {
              if (!visited.has(parent)) {
                queue.push(parent);
              }
            }
          }
        }

        commitCount = visited.size;
      }
    } else if (hasLooseObjects || commitCount < 2) {
      // Pack-only counting misses loose commits; fall back to graph traversal when needed.
      commitCount = await getCommitCount(rootCid);
    }

    // Cache the result
    commitCountCache.set(cacheKey, commitCount);
    console.log(`[git perf] getCommitCountFast completed in ${(performance.now() - startTime).toFixed(0)} ms, count: ${commitCount}`);
    return commitCount;
  } catch (err) {
    console.error('[git] getCommitCountFast failed:', err);
    return 0;
  }
}

/**
 * Read object type from pack data at given offset (no decompression)
 * Returns: 1=commit, 2=tree, 3=blob, 4=tag, 6=ofs_delta, 7=ref_delta
 */
function readPackObjectType(packData: Uint8Array, offset: number): number {
  const byte = packData[offset];
  return (byte >> 4) & 7;
}

// Use wasm-git for commit log (slow - copies entire .git)
export async function getLogWasm(
  rootCid: CID,
  options?: { depth?: number }
): Promise<CommitInfo[]> {
  return withWasmGitLock(async () => {
    const tree = getTree();
    const depth = options?.depth ?? 20;

    // Check for .git directory
    const gitDirResult = await tree.resolvePath(rootCid, '.git');
    if (!gitDirResult || gitDirResult.type !== LinkType.Dir) {
      return [];
    }

    const module = await loadWasmGit();

    // Use a unique path for each call to avoid conflicts
    const repoPath = createRepoPath();
    const originalCwd = module.FS.cwd();

    try {
      // Create and mount a fresh working directory
      module.FS.mkdir(repoPath);

      // Write .gitconfig so git doesn't complain about missing user
      try {
        module.FS.writeFile('/home/web_user/.gitconfig', '[user]\nname = Reader\nemail = reader@example.com\n');
      } catch {
        // May already exist
      }

      // Change to repo directory
      module.FS.chdir(repoPath);

      // Only copy .git directory - much faster for read-only operations
      await copyGitDirToWasmFS(module, rootCid, '.');

      // Run git log from HEAD
      const output = module.callWithOutput(['log']);

      if (!output || output.trim() === '') {
        return [];
      }

      // Parse the default git log format:
      // commit <sha>
      // Author: <name> <email>
      // Date:   <date>
      //
      //     <message>
      //
      const commits: CommitInfo[] = [];

      const commitBlocks = output.split(/^commit /m).filter(Boolean);

      for (const block of commitBlocks) {
        if (commits.length >= depth) break;

        const lines = block.split('\n');
        const oid = lines[0]?.trim();
        if (!oid || oid.length !== 40) continue;

        let author = '';
        let email = '';
        let timestamp = 0;
        const messageLines: string[] = [];
        let inMessage = false;

        for (let i = 1; i < lines.length; i++) {
          const line = lines[i];

          if (line.startsWith('Author: ')) {
            const authorMatch = line.match(/^Author:\s*(.+?)\s*<(.+?)>/);
            if (authorMatch) {
              author = authorMatch[1].trim();
              email = authorMatch[2];
            }
          } else if (line.startsWith('Date: ')) {
            // Parse date like "Thu Dec 11 15:05:31 2025 +0000"
            const dateStr = line.substring(6).trim();
            const date = new Date(dateStr);
            if (!isNaN(date.getTime())) {
              timestamp = Math.floor(date.getTime() / 1000);
            }
          } else if (line === '') {
            if (author && !inMessage) {
              inMessage = true;
            }
          } else if (inMessage) {
            // Message lines are indented with 4 spaces
            messageLines.push(line.replace(/^    /, ''));
          }
        }

        const message = messageLines.join('\n').trim();

        commits.push({
          oid,
          message,
          author,
          email,
          timestamp,
          parent: [], // wasm-git default format doesn't include parent info
        });
      }

      return commits;
    } catch (err) {
      console.error('[wasm-git] git log failed:', err);
      return [];
    } finally {
      // Restore original working directory
      try {
        module.FS.chdir(originalCwd);
        rmRf(module, repoPath);
      } catch {
        // Ignore errors
      }
    }
  });
}

/**
 * Get last commit info for files in a directory
 * Returns a map of filename -> commit info
 * @param rootCid - The root CID of the git repository
 * @param filenames - Array of filenames (base names only, not full paths)
 * @param subpath - Optional subdirectory path relative to git root (e.g., 'src' or 'src/utils')
 */
export async function getFileLastCommitsWasm(
  rootCid: CID,
  filenames: string[],
  subpath?: string
): Promise<Map<string, { oid: string; message: string; timestamp: number }>> {
  return withWasmGitLock(async () => {
    const tree = getTree();
    const result = new Map<string, { oid: string; message: string; timestamp: number }>();

    if (filenames.length === 0) return result;

    // Check for .git directory
    const gitDirResult = await tree.resolvePath(rootCid, '.git');
    if (!gitDirResult || gitDirResult.type !== LinkType.Dir) {
      return result;
    }

    const module = await loadWasmGit();
    const repoPath = createRepoPath();
    const originalCwd = module.FS.cwd();

    try {
      module.FS.mkdir(repoPath);

      try {
        module.FS.writeFile('/home/web_user/.gitconfig', '[user]\nname = Reader\nemail = reader@example.com\n');
      } catch {
        // May already exist
      }

      module.FS.chdir(repoPath);

      // Only copy .git directory - git log only needs history, not working tree
      await copyGitDirToWasmFS(module, rootCid, '.');

      // For each file, get the last commit that touched it
      for (const filename of filenames) {
        // Skip .git directory
        if (filename === '.git') continue;

        try {
          // Build the full path relative to git root
          const fullPath = subpath ? `${subpath}/${filename}` : filename;
          // Run git log -1 -- <fullPath> to get last commit for this file
          const output = module.callWithOutput(['log', '-1', '--', fullPath]);

          if (!output || output.trim() === '') continue;

          // Parse same format as getLog
          const lines = output.split('\n');
          let oid = '';
          let timestamp = 0;
          const messageLines: string[] = [];
          let inMessage = false;

          for (const line of lines) {
            if (line.startsWith('commit ')) {
              oid = line.substring(7).trim();
            } else if (line.startsWith('Date: ')) {
              const dateStr = line.substring(6).trim();
              const date = new Date(dateStr);
              if (!isNaN(date.getTime())) {
                timestamp = Math.floor(date.getTime() / 1000);
              }
            } else if (line === '') {
              if (oid && !inMessage) {
                inMessage = true;
              }
            } else if (inMessage) {
              messageLines.push(line.replace(/^    /, ''));
            }
          }

          if (oid) {
            result.set(filename, {
              oid,
              message: messageLines.join('\n').trim(),
              timestamp,
            });
          }
        } catch {
          // Skip files with errors
        }
      }

      return result;
    } catch (err) {
      console.error('[wasm-git] getFileLastCommits failed:', err);
      return result;
    } finally {
      try {
        module.FS.chdir(originalCwd);
        rmRf(module, repoPath);
      } catch {
        // Ignore
      }
    }
  });
}

/**
 * Parse a git tree object
 * Returns array of { mode, name, hash } entries
 */
function parseGitTree(content: Uint8Array): Array<{ mode: string; name: string; hash: string }> {
  const entries: Array<{ mode: string; name: string; hash: string }> = [];
  let pos = 0;

  while (pos < content.length) {
    // Find space (separates mode from name)
    let spacePos = pos;
    while (spacePos < content.length && content[spacePos] !== 0x20) spacePos++;
    const mode = new TextDecoder().decode(content.slice(pos, spacePos));

    // Find null (separates name from hash)
    let nullPos = spacePos + 1;
    while (nullPos < content.length && content[nullPos] !== 0) nullPos++;
    const name = new TextDecoder().decode(content.slice(spacePos + 1, nullPos));

    // Hash is 20 bytes after null
    const hashBytes = content.slice(nullPos + 1, nullPos + 21);
    const hash = Array.from(hashBytes).map(b => b.toString(16).padStart(2, '0')).join('');

    entries.push({ mode, name, hash });
    pos = nullPos + 21;
  }

  return entries;
}

/**
 * Get tree entries for a git tree object
 */
// Global cache for tree entries by sha (trees are immutable)
const treeEntriesCache = new Map<string, Map<string, { hash: string; mode: string }>>();

// Cache for individual path lookups in a tree
const treePathCache = new Map<string, Map<string, { hash: string; mode: string } | 'dir' | null>>();

/**
 * Get a specific entry from a git tree by path (much faster than walking entire tree)
 */
async function getTreeEntryAtPath(
  tree: ReturnType<typeof getTree>,
  gitDirCid: CID,
  treeSha: string,
  path: string,
  preloadedObjects?: Map<string, { type: string; content: Uint8Array }>
): Promise<{ hash: string; mode: string } | 'dir' | null> {
  const pathCache = treePathCache.get(treeSha) || new Map();

  if (pathCache.has(path)) {
    return pathCache.get(path)!;
  }

  const getObject = async (sha: string) => {
    if (preloadedObjects) {
      const obj = preloadedObjects.get(sha);
      if (obj) return obj;
    }
    return readGitObject(tree, gitDirCid, sha);
  };

  const parts = path.split('/');
  let currentSha = treeSha;

  for (let i = 0; i < parts.length; i++) {
    const part = parts[i];
    const obj = await getObject(currentSha);
    if (!obj || obj.type !== 'tree') {
      pathCache.set(path, null);
      treePathCache.set(treeSha, pathCache);
      return null;
    }

    const entries = parseGitTree(obj.content);
    const entry = entries.find(e => e.name === part);
    if (!entry) {
      pathCache.set(path, null);
      treePathCache.set(treeSha, pathCache);
      return null;
    }

    if (i === parts.length - 1) {
      // Found the target
      const result = entry.mode === '40000' ? 'dir' : { hash: entry.hash, mode: entry.mode };
      pathCache.set(path, result);
      treePathCache.set(treeSha, pathCache);
      return result;
    }

    // Must be a directory to continue
    if (entry.mode !== '40000') {
      pathCache.set(path, null);
      treePathCache.set(treeSha, pathCache);
      return null;
    }
    currentSha = entry.hash;
  }

  pathCache.set(path, null);
  treePathCache.set(treeSha, pathCache);
  return null;
}

/**
 * Get the tree SHA for a subtree at a given path
 */
async function getSubtreeSha(
  tree: ReturnType<typeof getTree>,
  gitDirCid: CID,
  treeSha: string,
  path: string,
  preloadedObjects?: Map<string, { type: string; content: Uint8Array }>
): Promise<string | null> {
  if (!path) return treeSha;

  const getObject = async (sha: string) => {
    if (preloadedObjects) {
      const obj = preloadedObjects.get(sha);
      if (obj) return obj;
    }
    return readGitObject(tree, gitDirCid, sha);
  };

  const parts = path.split('/');
  let currentSha = treeSha;

  for (const part of parts) {
    const obj = await getObject(currentSha);
    if (!obj || obj.type !== 'tree') return null;

    const entries = parseGitTree(obj.content);
    const entry = entries.find(e => e.name === part);
    if (!entry || entry.mode !== '40000') return null;
    currentSha = entry.hash;
  }

  return currentSha;
}

async function getGitTreeEntries(
  tree: ReturnType<typeof getTree>,
  gitDirCid: CID,
  treeSha: string,
  preloadedObjects?: Map<string, { type: string; content: Uint8Array }>
): Promise<Map<string, { hash: string; mode: string }>> {
  // Check cache first
  const cached = treeEntriesCache.get(treeSha);
  if (cached) return cached;

  const result = new Map<string, { hash: string; mode: string }>();

  const getObject = async (sha: string) => {
    if (preloadedObjects) {
      const obj = preloadedObjects.get(sha);
      if (obj) return obj;
    }
    return readGitObject(tree, gitDirCid, sha);
  };

  const walkGitTree = async (sha: string, prefix: string): Promise<void> => {
    const obj = await getObject(sha);
    if (!obj || obj.type !== 'tree') return;

    const entries = parseGitTree(obj.content);
    for (const entry of entries) {
      const path = prefix ? `${prefix}/${entry.name}` : entry.name;

      // Mode 40000 = directory, 100644/100755 = file, 120000 = symlink, 160000 = submodule
      if (entry.mode === '40000') {
        await walkGitTree(entry.hash, path);
      } else {
        result.set(path, { hash: entry.hash, mode: entry.mode });
      }
    }
  };

  await walkGitTree(treeSha, '');

  // Cache the result
  treeEntriesCache.set(treeSha, result);
  return result;
}

/**
 * Get last commit info for each file/directory by tracing through commit history
 * Native implementation - no wasm-git needed
 * Uses path-based lookups to avoid walking entire trees (O(depth) vs O(files) per path)
 */
export async function getFileLastCommits(
  rootCid: CID,
  filenames: string[],
  subpath?: string
): Promise<Map<string, { oid: string; message: string; timestamp: number }>> {
  const startTime = performance.now();
  const htree = getTree();
  const result = new Map<string, { oid: string; message: string; timestamp: number }>();

  if (filenames.length === 0) {
    return result;
  }

  // Check for .git directory
  const gitDirResult = await htree.resolvePath(rootCid, '.git');
  if (!gitDirResult || gitDirResult.type !== LinkType.Dir) {
    return result;
  }

  try {
    // Preload all objects from pack files (single network fetch)
    const preloadedObjects = await preloadCommitsFromPacks(htree, gitDirResult.cid);

    // Build full paths to search for (files and directories)
    const targetNames = filenames.filter(f => f !== '.git');
    const targetPaths = new Map<string, string>(); // fullPath -> filename
    for (const f of targetNames) {
      const fullPath = subpath ? `${subpath}/${f}` : f;
      targetPaths.set(fullPath, f);
    }

    // Get commit history
    const headSha = await getHead(rootCid);
    if (!headSha) return result;

    // Helper to get object from cache or fetch
    const getObject = async (sha: string) => {
      const cached = preloadedObjects.get(sha);
      if (cached) return cached;
      return readGitObject(htree, gitDirResult.cid, sha);
    };

    // Walk through commits, comparing each with its parent to find when files changed
    const visited = new Set<string>();
    const queue = [headSha];
    const foundEntries = new Set<string>();

    // Cache for path lookups: commitTreeSha:path -> hash or 'dir' or null
    const pathLookupCache = new Map<string, { hash: string; mode: string } | 'dir' | null>();

    // Helper to get path entry with caching
    const getPathEntry = async (treeSha: string, path: string) => {
      const cacheKey = `${treeSha}:${path}`;
      if (pathLookupCache.has(cacheKey)) {
        return pathLookupCache.get(cacheKey)!;
      }
      const entry = await getTreeEntryAtPath(htree, gitDirResult.cid, treeSha, path, preloadedObjects);
      pathLookupCache.set(cacheKey, entry);
      return entry;
    };

    // Helper to get subtree SHA with caching
    const subtreeShaCache = new Map<string, string | null>();
    const getSubtree = async (treeSha: string, path: string) => {
      const cacheKey = `${treeSha}:${path}`;
      if (subtreeShaCache.has(cacheKey)) {
        return subtreeShaCache.get(cacheKey)!;
      }
      const sha = await getSubtreeSha(htree, gitDirResult.cid, treeSha, path, preloadedObjects);
      subtreeShaCache.set(cacheKey, sha);
      return sha;
    };

    // For loose object repos, batch load commits in parallel
    const BATCH_SIZE = 20;

    while (queue.length > 0 && foundEntries.size < targetPaths.size) {
      // Take a batch of commits to process
      const batch: string[] = [];
      while (batch.length < BATCH_SIZE && queue.length > 0 && foundEntries.size < targetPaths.size) {
        const sha = queue.shift()!;
        if (!visited.has(sha)) {
          visited.add(sha);
          batch.push(sha);
        }
      }
      if (batch.length === 0) break;

      // Load all commits in parallel
      const commitObjs = await Promise.all(batch.map(sha => getObject(sha)));
      const commits = batch.map((sha, i) => {
        const obj = commitObjs[i];
        if (!obj || obj.type !== 'commit') return null;
        const parsed = parseCommit(obj.content);
        if (!parsed) return null;
        return { sha, commit: parsed };
      }).filter((c): c is { sha: string; commit: ReturnType<typeof parseCommit> & {} } => c !== null);

      // Load parent commits to get their tree SHAs
      const parentShas = commits.flatMap(c => c.commit.parents).filter(p => !visited.has(p));
      const parentObjs = await Promise.all(parentShas.map(sha => getObject(sha)));
      const parentCommitMap = new Map<string, ReturnType<typeof parseCommit>>();
      parentShas.forEach((sha, i) => {
        const obj = parentObjs[i];
        if (obj && obj.type === 'commit') {
          const parsed = parseCommit(obj.content);
          if (parsed) {
            parentCommitMap.set(sha, parsed);
          }
        }
      });

      // Now process each commit
      for (const { sha, commit } of commits) {
        if (foundEntries.size >= targetPaths.size) break;

        let parentTreeSha: string | null = null;
        if (commit.parents.length > 0) {
          const parentCommit = parentCommitMap.get(commit.parents[0]);
          if (parentCommit) {
            parentTreeSha = parentCommit.tree;
          }
        }

        // Compare each target path using path-based lookups (not full tree walks)
        for (const [targetPath, filename] of targetPaths) {
          if (foundEntries.has(targetPath)) continue;

          // Get entry at this path in current commit
          const currentEntry = await getPathEntry(commit.tree, targetPath);

          if (currentEntry === null) {
            // Path doesn't exist in current commit, skip
            continue;
          }

          if (currentEntry === 'dir') {
            // It's a directory - compare subtree SHAs instead of walking all files
            const currentSubtreeSha = await getSubtree(commit.tree, targetPath);
            const parentSubtreeSha = parentTreeSha ? await getSubtree(parentTreeSha, targetPath) : null;

            if (currentSubtreeSha && currentSubtreeSha !== parentSubtreeSha) {
              // Directory was added or modified
              result.set(filename, {
                oid: sha,
                message: commit.message,
                timestamp: commit.timestamp,
              });
              foundEntries.add(targetPath);
            }
          } else {
            // It's a file - compare file hashes
            const parentEntry = parentTreeSha ? await getPathEntry(parentTreeSha, targetPath) : null;

            if (!parentEntry || parentEntry === 'dir' || currentEntry.hash !== parentEntry.hash) {
              // File was added or modified (or was a dir before, now a file)
              result.set(filename, {
                oid: sha,
                message: commit.message,
                timestamp: commit.timestamp,
              });
              foundEntries.add(targetPath);
            }
          }
        }

        // Add parents to queue
        for (const parent of commit.parents) {
          if (!visited.has(parent)) {
            queue.push(parent);
          }
        }
      } // end for commit in commits
    } // end while queue

    console.log(`[git perf] getFileLastCommits completed in ${(performance.now() - startTime).toFixed(0)} ms`);
    return result;
  } catch (err) {
    console.error('[git] getFileLastCommits failed:', err);
    return result;
  }
}

export interface DiffEntry {
  path: string;
  status: 'added' | 'deleted' | 'modified';
  oldHash?: string;
  newHash?: string;
}

/**
 * Get diff between two commits
 * Native implementation - no wasm-git needed
 */
export async function getDiff(
  rootCid: CID,
  fromCommit: string,
  toCommit: string
): Promise<DiffEntry[]> {
  const htree = getTree();
  const result: DiffEntry[] = [];

  // Check for .git directory
  const gitDirResult = await htree.resolvePath(rootCid, '.git');
  if (!gitDirResult || gitDirResult.type !== LinkType.Dir) {
    return result;
  }

  try {
    // Get tree for "from" commit
    const fromObj = await readGitObject(htree, gitDirResult.cid, fromCommit);
    if (!fromObj || fromObj.type !== 'commit') return result;
    const fromParsed = parseCommit(fromObj.content);
    if (!fromParsed) return result;

    // Get tree for "to" commit
    const toObj = await readGitObject(htree, gitDirResult.cid, toCommit);
    if (!toObj || toObj.type !== 'commit') return result;
    const toParsed = parseCommit(toObj.content);
    if (!toParsed) return result;

    // Get all files in both trees
    const fromTree = await getGitTreeEntries(htree, gitDirResult.cid, fromParsed.tree);
    const toTree = await getGitTreeEntries(htree, gitDirResult.cid, toParsed.tree);

    // Find deleted files (in from but not in to)
    for (const [path, file] of fromTree) {
      if (!toTree.has(path)) {
        result.push({ path, status: 'deleted', oldHash: file.hash });
      }
    }

    // Find added and modified files
    for (const [path, file] of toTree) {
      const fromFile = fromTree.get(path);
      if (!fromFile) {
        result.push({ path, status: 'added', newHash: file.hash });
      } else if (fromFile.hash !== file.hash) {
        result.push({ path, status: 'modified', oldHash: fromFile.hash, newHash: file.hash });
      }
    }

    // Sort by path for consistent output
    result.sort((a, b) => a.path.localeCompare(b.path));

    return result;
  } catch (err) {
    console.error('[git] getDiff failed:', err);
    return result;
  }
}

/**
 * Get file content at a specific commit
 * Native implementation - no wasm-git needed
 */
export async function getFileAtCommit(
  rootCid: CID,
  commitSha: string,
  filePath: string
): Promise<Uint8Array | null> {
  const htree = getTree();

  // Check for .git directory
  const gitDirResult = await htree.resolvePath(rootCid, '.git');
  if (!gitDirResult || gitDirResult.type !== LinkType.Dir) {
    return null;
  }

  try {
    // Get commit
    const commitObj = await readGitObject(htree, gitDirResult.cid, commitSha);
    if (!commitObj || commitObj.type !== 'commit') return null;

    const commit = parseCommit(commitObj.content);
    if (!commit) return null;

    // Walk tree to find file
    const parts = filePath.split('/').filter(p => p);
    let currentTreeSha = commit.tree;

    for (let i = 0; i < parts.length; i++) {
      const part = parts[i];
      const treeObj = await readGitObject(htree, gitDirResult.cid, currentTreeSha);
      if (!treeObj || treeObj.type !== 'tree') return null;

      const entries = parseGitTree(treeObj.content);
      const entry = entries.find(e => e.name === part);
      if (!entry) return null;

      if (i === parts.length - 1) {
        // Last part - should be a blob
        const blobObj = await readGitObject(htree, gitDirResult.cid, entry.hash);
        if (!blobObj || blobObj.type !== 'blob') return null;
        return blobObj.content;
      } else {
        // Not last part - should be a tree
        if (entry.mode !== '40000') return null;
        currentTreeSha = entry.hash;
      }
    }

    return null;
  } catch (err) {
    console.error('[git] getFileAtCommit failed:', err);
    return null;
  }
}
