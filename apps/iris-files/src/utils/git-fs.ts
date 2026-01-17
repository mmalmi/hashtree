/**
 * Filesystem adapter for isomorphic-git that uses hashtree as storage
 * Implements the fs interface expected by isomorphic-git
 */
import type { CID, HashTree } from 'hashtree';
import { LinkType } from 'hashtree';
import { getTree } from '../store';

interface Stats {
  type: 'file' | 'dir';
  mode: number;
  size: number;
  ino: number;
  mtimeMs: number;
  ctimeMs: number;
  uid: number;
  gid: number;
  dev: number;
  isFile(): boolean;
  isDirectory(): boolean;
  isSymbolicLink(): boolean;
}

function createStats(isDir: boolean, size: number): Stats {
  const now = Date.now();
  return {
    type: isDir ? 'dir' : 'file',
    mode: isDir ? 0o40755 : 0o100644,
    size,
    ino: 0,
    mtimeMs: now,
    ctimeMs: now,
    uid: 1000,
    gid: 1000,
    dev: 0,
    isFile: () => !isDir,
    isDirectory: () => isDir,
    isSymbolicLink: () => false,
  };
}

/**
 * In-memory filesystem layer on top of hashtree
 * Maintains a working tree that can be committed back to hashtree
 */
export class HashTreeFS {
  private tree: HashTree;
  private rootCid: CID | null = null;

  // In-memory cache for pending writes (before commit)
  private pendingFiles = new Map<string, Uint8Array>();
  private pendingDirs = new Set<string>();
  private deletedPaths = new Set<string>();

  constructor(rootCid?: CID) {
    this.tree = getTree();
    this.rootCid = rootCid ?? null;
  }

  /**
   * Set the root CID (after clone or checkout)
   */
  setRoot(cid: CID) {
    this.rootCid = cid;
    this.pendingFiles.clear();
    this.pendingDirs.clear();
    this.deletedPaths.clear();
  }

  /**
   * Get current root CID
   */
  getRoot(): CID | null {
    return this.rootCid;
  }

  /**
   * Commit pending changes to hashtree and return new root CID
   */
  async commit(): Promise<CID> {
    let currentRoot = this.rootCid;

    // If no root, create empty directory
    if (!currentRoot) {
      const { cid } = await this.tree.putDirectory([]);
      currentRoot = cid;
    }

    // Apply deletions first
    for (const path of this.deletedPaths) {
      const parts = this.parsePath(path);
      const name = parts.pop()!;
      try {
        currentRoot = await this.tree.deleteEntry(currentRoot, parts, name);
      } catch {
        // Ignore deletion errors (path may not exist)
      }
    }

    // Create directories
    for (const path of this.pendingDirs) {
      const parts = this.parsePath(path);
      const name = parts.pop()!;
      const { cid: emptyDir } = await this.tree.putDirectory([]);
      currentRoot = await this.tree.setEntry(currentRoot, parts, name, emptyDir, 0, LinkType.Dir);
    }

    // Write files
    for (const [path, data] of this.pendingFiles) {
      const parts = this.parsePath(path);
      const name = parts.pop()!;
      const { cid: fileCid, size } = await this.tree.putFile(data);
      currentRoot = await this.tree.setEntry(currentRoot, parts, name, fileCid, size, LinkType.Blob);
    }

    this.rootCid = currentRoot;
    this.pendingFiles.clear();
    this.pendingDirs.clear();
    this.deletedPaths.clear();

    return currentRoot;
  }

  private parsePath(filepath: string): string[] {
    return filepath.split('/').filter(Boolean);
  }

  // ============ isomorphic-git fs interface ============

  /**
   * Read file contents
   */
  async readFile(filepath: string, options?: { encoding?: string }): Promise<Uint8Array | string> {
    // Check pending writes first
    if (this.pendingFiles.has(filepath)) {
      const data = this.pendingFiles.get(filepath)!;
      if (options?.encoding === 'utf8') {
        return new TextDecoder().decode(data);
      }
      return data;
    }

    if (!this.rootCid) {
      throw new Error(`ENOENT: no such file or directory, open '${filepath}'`);
    }

    const parts = this.parsePath(filepath);
    let cid = this.rootCid;

    for (const part of parts.slice(0, -1)) {
      const resolved = await this.tree.resolvePath(cid, part);
      if (!resolved) {
        throw new Error(`ENOENT: no such file or directory, open '${filepath}'`);
      }
      cid = resolved.cid;
    }

    const fileName = parts[parts.length - 1];
    const resolved = await this.tree.resolvePath(cid, fileName);
    if (!resolved) {
      throw new Error(`ENOENT: no such file or directory, open '${filepath}'`);
    }

    const data = await this.tree.readFile(resolved.cid);
    if (!data) {
      throw new Error(`ENOENT: no such file or directory, open '${filepath}'`);
    }

    if (options?.encoding === 'utf8') {
      return new TextDecoder().decode(data);
    }
    return data;
  }

  /**
   * Write file contents
   */
  async writeFile(filepath: string, data: Uint8Array | string): Promise<void> {
    const bytes = typeof data === 'string' ? new TextEncoder().encode(data) : data;
    this.pendingFiles.set(filepath, bytes);
    this.deletedPaths.delete(filepath);

    // Ensure parent directories exist
    const parts = this.parsePath(filepath);
    for (let i = 1; i < parts.length; i++) {
      const dirPath = parts.slice(0, i).join('/');
      if (!this.pendingDirs.has(dirPath)) {
        // Check if dir exists in tree
        const exists = await this.exists(dirPath);
        if (!exists) {
          this.pendingDirs.add(dirPath);
        }
      }
    }
  }

  /**
   * Delete a file
   */
  async unlink(filepath: string): Promise<void> {
    this.pendingFiles.delete(filepath);
    this.deletedPaths.add(filepath);
  }

  /**
   * Read directory contents
   */
  async readdir(filepath: string): Promise<string[]> {
    const parts = this.parsePath(filepath);
    let cid = this.rootCid;

    if (!cid) {
      // Return pending dirs at root level
      const pending = new Set<string>();
      for (const dir of this.pendingDirs) {
        const dirParts = this.parsePath(dir);
        if (dirParts.length === 1) pending.add(dirParts[0]);
      }
      for (const file of this.pendingFiles.keys()) {
        const fileParts = this.parsePath(file);
        if (fileParts.length === 1) pending.add(fileParts[0]);
      }
      return Array.from(pending);
    }

    // Navigate to directory
    for (const part of parts) {
      const resolved = await this.tree.resolvePath(cid, part);
      if (!resolved) {
        throw new Error(`ENOENT: no such file or directory, scandir '${filepath}'`);
      }
      cid = resolved.cid;
    }

    const entries = await this.tree.listDirectory(cid);
    const names = entries.map(e => e.name);

    // Add pending items in this directory
    const prefix = filepath ? filepath + '/' : '';
    for (const dir of this.pendingDirs) {
      if (dir.startsWith(prefix)) {
        const rest = dir.slice(prefix.length);
        const nextPart = rest.split('/')[0];
        if (nextPart && !names.includes(nextPart)) {
          names.push(nextPart);
        }
      }
    }
    for (const file of this.pendingFiles.keys()) {
      if (file.startsWith(prefix)) {
        const rest = file.slice(prefix.length);
        const nextPart = rest.split('/')[0];
        if (nextPart && !names.includes(nextPart)) {
          names.push(nextPart);
        }
      }
    }

    // Remove deleted items
    return names.filter(n => !this.deletedPaths.has(prefix + n));
  }

  /**
   * Create a directory
   */
  async mkdir(filepath: string): Promise<void> {
    this.pendingDirs.add(filepath);
    this.deletedPaths.delete(filepath);
  }

  /**
   * Remove a directory
   */
  async rmdir(filepath: string): Promise<void> {
    this.pendingDirs.delete(filepath);
    this.deletedPaths.add(filepath);
  }

  /**
   * Get file/directory stats
   */
  async stat(filepath: string): Promise<Stats> {
    // Check pending
    if (this.pendingFiles.has(filepath)) {
      return createStats(false, this.pendingFiles.get(filepath)!.length);
    }
    if (this.pendingDirs.has(filepath)) {
      return createStats(true, 0);
    }

    if (!this.rootCid) {
      throw new Error(`ENOENT: no such file or directory, stat '${filepath}'`);
    }

    const parts = this.parsePath(filepath);
    let cid = this.rootCid;

    for (const part of parts.slice(0, -1)) {
      const resolved = await this.tree.resolvePath(cid, part);
      if (!resolved) {
        throw new Error(`ENOENT: no such file or directory, stat '${filepath}'`);
      }
      cid = resolved.cid;
    }

    const name = parts[parts.length - 1];
    if (!name) {
      // Root directory
      return createStats(true, 0);
    }

    const resolved = await this.tree.resolvePath(cid, name);
    if (!resolved) {
      throw new Error(`ENOENT: no such file or directory, stat '${filepath}'`);
    }

    return createStats(resolved.type === LinkType.Dir, resolved.size);
  }

  /**
   * Get file/directory stats (same as stat for our purposes)
   */
  async lstat(filepath: string): Promise<Stats> {
    return this.stat(filepath);
  }

  /**
   * Check if path exists
   */
  private async exists(filepath: string): Promise<boolean> {
    try {
      await this.stat(filepath);
      return true;
    } catch {
      return false;
    }
  }

  /**
   * Read symbolic link (not supported, but required by interface)
   */
  async readlink(filepath: string): Promise<string> {
    throw new Error(`EINVAL: invalid argument, readlink '${filepath}'`);
  }

  /**
   * Create symbolic link (not supported)
   */
  async symlink(target: string, filepath: string): Promise<void> {
    throw new Error(`EPERM: operation not permitted, symlink '${target}' -> '${filepath}'`);
  }

  /**
   * Change file mode (no-op for hashtree)
   */
  async chmod(_filepath: string, _mode: number): Promise<void> {
    // No-op - hashtree doesn't support file modes
  }

  /**
   * Rename/move a file
   */
  async rename(oldPath: string, newPath: string): Promise<void> {
    const data = await this.readFile(oldPath);
    await this.writeFile(newPath, data as Uint8Array);
    await this.unlink(oldPath);
  }
}

/**
 * Create an fs-like object compatible with isomorphic-git's promises API
 */
export function createGitFS(rootCid?: CID) {
  const htfs = new HashTreeFS(rootCid);

  return {
    promises: {
      readFile: htfs.readFile.bind(htfs),
      writeFile: htfs.writeFile.bind(htfs),
      unlink: htfs.unlink.bind(htfs),
      readdir: htfs.readdir.bind(htfs),
      mkdir: htfs.mkdir.bind(htfs),
      rmdir: htfs.rmdir.bind(htfs),
      stat: htfs.stat.bind(htfs),
      lstat: htfs.lstat.bind(htfs),
      readlink: htfs.readlink.bind(htfs),
      symlink: htfs.symlink.bind(htfs),
      chmod: htfs.chmod.bind(htfs),
      rename: htfs.rename.bind(htfs),
    },
    _htfs: htfs, // Expose for commit/getRoot access
  };
}
