/**
 * Git utilities using wasm-git (libgit2 compiled to WebAssembly)
 */
import type { CID } from 'hashtree';
import { LinkType, toHex } from 'hashtree';
import { getTree } from '../store';
import { LRUCache } from './lruCache';

/**
 * Cache for file commit info, keyed by git repo hash
 * Each entry maps full file paths to their commit info
 */
type FileCommitInfo = { oid: string; message: string; timestamp: number };
const fileCommitsCache = new LRUCache<string, Map<string, FileCommitInfo>>(20);

/**
 * Cache for git log (commits), keyed by "hash:depth"
 */
type CommitLog = Array<{
  oid: string;
  message: string;
  author: string;
  email: string;
  timestamp: number;
  parent: string[];
}>;
const gitLogCache = new LRUCache<string, CommitLog>(20);

/**
 * Cache for HEAD commit SHA, keyed by git repo hash
 */
const gitHeadCache = new LRUCache<string, string | null>(20);

/**
 * Cache for branches info, keyed by git repo hash
 */
const gitBranchesCache = new LRUCache<string, { branches: string[]; currentBranch: string | null }>(20);

/**
 * Cache for git status, keyed by git repo hash
 */
type GitStatusResult = { staged: string[]; unstaged: string[]; untracked: string[]; hasChanges: boolean };
const gitStatusCache = new LRUCache<string, GitStatusResult>(20);

/**
 * Cache for read-only git commands (diff, diff-tree, show, etc.)
 * Keyed by "hash:command"
 */
const gitCommandCache = new LRUCache<string, { output: string; error?: string }>(50);

// Commands that are safe to cache (read-only)
const CACHEABLE_COMMANDS = ['diff', 'diff-tree', 'show', 'log', 'cat-file', 'ls-tree', 'rev-parse'];

export interface CloneOptions {
  url: string;
  /** Optional branch/ref to checkout (default: default branch) */
  ref?: string;
  /** Shallow clone depth (default: full clone) */
  depth?: number;
  /** Progress callback */
  onProgress?: (phase: string, loaded: number, total: number) => void;
}

export interface CloneResult {
  /** Root CID of the cloned repository */
  rootCid: CID;
  /** Current branch/ref */
  ref: string;
}

/**
 * Clone a git repository into hashtree storage
 * Note: Clone functionality requires network access and CORS proxy
 */
export async function cloneRepo(_options: CloneOptions): Promise<CloneResult> {
  // Clone is complex with wasm-git - requires CORS proxy setup
  // For now, throw not implemented
  throw new Error('Clone not yet implemented with wasm-git. Upload a git repo folder instead.');
}

/**
 * Get commit log for a repository
 * Results are cached by git repo hash for fast navigation
 * Uses wasm-git (libgit2)
 */
export async function getLog(rootCid: CID, options?: { depth?: number }): Promise<CommitLog>;
export async function getLog(rootCid: CID, options: { depth?: number; debug: true }): Promise<{ commits: CommitLog; debug: string[] }>;
export async function getLog(rootCid: CID, options?: { depth?: number; debug?: boolean }): Promise<CommitLog | { commits: CommitLog; debug: string[] }> {
  const debugInfo: string[] = [];
  const depth = options?.depth ?? 20;
  const cacheKey = `${toHex(rootCid.hash)}:${depth}`;

  // Check cache first (skip for debug mode)
  if (!options?.debug) {
    const cached = gitLogCache.get(cacheKey);
    if (cached) {
      return cached;
    }
  }

  try {
    const { getLogWithWasmGit, getLogWithWasmGitSlow, getHeadWithWasmGit } = await import('./wasmGit');
    debugInfo.push('Using wasm-git');
    let commits = await getLogWithWasmGit(rootCid, { depth });
    debugInfo.push(`Found ${commits.length} commits`);

    if (commits.length === 0) {
      const head = await getHeadWithWasmGit(rootCid);
      if (head) {
        debugInfo.push('Fast log empty with HEAD present, falling back to wasm-git slow path');
        commits = await getLogWithWasmGitSlow(rootCid, { depth });
        debugInfo.push(`Slow path found ${commits.length} commits`);
      }
    }

    if (commits.length > 0 && commits.length < depth) {
      const commitIds = new Set(commits.map(commit => commit.oid));
      const hasMissingParent = commits.some(commit => commit.parent.some(parent => !commitIds.has(parent)));
      if (hasMissingParent) {
        debugInfo.push('Fast log missing parent commits, falling back to wasm-git slow path');
        commits = await getLogWithWasmGitSlow(rootCid, { depth });
        debugInfo.push(`Slow path found ${commits.length} commits`);
      }
    }

    // Cache only non-empty results to avoid locking in empty logs
    if (commits.length > 0) {
      gitLogCache.set(cacheKey, commits);
    } else {
      gitLogCache.delete(cacheKey);
    }

    if (options?.debug) {
      return { commits, debug: debugInfo };
    }
    return commits;
  } catch (e) {
    const message = e instanceof Error ? e.message : String(e);
    debugInfo.push(`wasm-git failed: ${message}`);
    if (options?.debug) {
      return { commits: [], debug: debugInfo };
    }
    return [];
  }
}

/**
 * Get list of branches
 * Results are cached by git repo hash
 * Uses wasm-git (libgit2)
 */
export async function getBranches(rootCid: CID) {
  const cacheKey = toHex(rootCid.hash);

  // Check cache first
  const cached = gitBranchesCache.get(cacheKey);
  if (cached) {
    return cached;
  }

  try {
    const { getBranchesWithWasmGit } = await import('./wasmGit');
    const result = await getBranchesWithWasmGit(rootCid);
    if (result.branches.length > 0 || result.currentBranch) {
      gitBranchesCache.set(cacheKey, result);
    } else {
      gitBranchesCache.delete(cacheKey);
    }
    return result;
  } catch {
    return { branches: [], currentBranch: null };
  }
}

/**
 * Get current HEAD commit SHA
 * Results are cached by git repo hash
 * Uses wasm-git (libgit2)
 */
export async function getHead(rootCid: CID): Promise<string | null> {
  const cacheKey = toHex(rootCid.hash);

  // Check cache first
  if (gitHeadCache.has(cacheKey)) {
    return gitHeadCache.get(cacheKey) ?? null;
  }

  try {
    const { getHeadWithWasmGit } = await import('./wasmGit');
    const result = await getHeadWithWasmGit(rootCid);
    if (result) {
      gitHeadCache.set(cacheKey, result);
    } else {
      gitHeadCache.delete(cacheKey);
    }
    return result;
  } catch {
    return null;
  }
}

/**
 * Get git status (staged, unstaged, untracked files)
 * Results are cached by git repo hash
 * Uses wasm-git (libgit2)
 */
export async function getStatus(rootCid: CID) {
  const cacheKey = toHex(rootCid.hash);

  // Check cache first
  const cached = gitStatusCache.get(cacheKey);
  if (cached) {
    console.log('[git] getStatus CACHE HIT for:', cacheKey.slice(0, 16), 'hasChanges:', cached.hasChanges);
    return cached;
  }

  console.log('[git] getStatus CACHE MISS for:', cacheKey.slice(0, 16), '- calling wasm-git');
  try {
    const { getStatusWithWasmGit } = await import('./wasmGit');
    const result = await getStatusWithWasmGit(rootCid);
    console.log('[git] getStatus result: hasChanges:', result.hasChanges, 'staged:', result.staged.length, 'unstaged:', result.unstaged.length, 'untracked:', result.untracked.length);
    gitStatusCache.set(cacheKey, result);
    return result;
  } catch (e) {
    console.error('[git] getStatus error:', e);
    return { staged: [], unstaged: [], untracked: [], hasChanges: false };
  }
}

/**
 * Create a new branch
 * Uses wasm-git (libgit2)
 * Returns the updated .git files that must be persisted to hashtree
 */
export async function createBranch(rootCid: CID, branchName: string, checkout: boolean = true): Promise<{
  success: boolean;
  error?: string;
  gitFiles?: Array<{ name: string; data: Uint8Array; isDir: boolean }>;
}> {
  const { createBranchWithWasmGit } = await import('./wasmGit');
  return await createBranchWithWasmGit(rootCid, branchName, checkout);
}

/**
 * Stage files and create a commit
 * Returns the updated .git directory files to be saved back to hashtree
 */
export async function commit(
  rootCid: CID,
  message: string,
  authorName: string,
  authorEmail: string,
  filesToStage?: string[]
) {
  const { commitWithWasmGit } = await import('./wasmGit');
  return await commitWithWasmGit(rootCid, message, authorName, authorEmail, filesToStage);
}

/**
 * Get diff between two commits
 * Native implementation - reads git objects directly from hashtree
 */
export async function getDiff(rootCid: CID, fromCommit: string, toCommit: string): Promise<{
  entries: Array<{ path: string; status: 'added' | 'deleted' | 'modified'; oldHash?: string; newHash?: string }>;
}> {
  const { getDiffNative } = await import('./wasmGit');
  const entries = await getDiffNative(rootCid, fromCommit, toCommit);
  return { entries };
}

/**
 * Check if a directory contains a .git folder (is a git repo)
 * This check is lightweight - doesn't load wasm-git
 */
export async function isGitRepo(rootCid: CID): Promise<boolean> {
  const tree = getTree();

  try {
    // Check for .git directory
    const gitDirResult = await tree.resolvePath(rootCid, '.git');
    if (!gitDirResult || gitDirResult.type !== LinkType.Dir) {
      return false;
    }

    // Check for HEAD file inside .git
    const headResult = await tree.resolvePath(gitDirResult.cid, 'HEAD');
    return headResult !== null && headResult.type !== LinkType.Dir;
  } catch {
    return false;
  }
}

/**
 * Get file content at a specific commit
 * Native implementation - reads git objects directly from hashtree
 */
export async function getFileAtCommit(
  rootCid: CID,
  filepath: string,
  commitHash: string
): Promise<Uint8Array | null> {
  const { getFileAtCommitNative } = await import('./wasmGit');
  return getFileAtCommitNative(rootCid, commitHash, filepath);
}

/**
 * Get blame information for a file
 */
export async function getBlame(_rootCid: CID, _filepath: string) {
  // TODO: Implement with wasm-git blame command
  return null;
}

/**
 * Initialize a git repository in a directory
 * Returns files for the .git directory to be added to the tree
 */
export async function initGitRepo(
  rootCid: CID,
  authorName: string,
  authorEmail: string,
  commitMessage: string = 'Initial commit'
): Promise<Array<{ name: string; data: Uint8Array; isDir: boolean }>> {
  const { initGitRepoWithWasmGit } = await import('./wasmGit');
  return await initGitRepoWithWasmGit(rootCid, authorName, authorEmail, commitMessage);
}

/**
 * Get last commit info for files in a directory
 * Returns a map of filename -> commit info
 * Results are cached by git repo hash for fast navigation between subdirectories
 * @param rootCid - The root CID of the git repository
 * @param filenames - Array of filenames (base names only, not full paths)
 * @param subpath - Optional subdirectory path relative to git root (e.g., 'src' or 'src/utils')
 */
export async function getFileLastCommits(
  rootCid: CID,
  filenames: string[],
  subpath?: string
): Promise<Map<string, FileCommitInfo>> {
  const cacheKey = toHex(rootCid.hash);
  const result = new Map<string, FileCommitInfo>();

  // Check cache first
  let cachedData = fileCommitsCache.get(cacheKey);
  const uncachedFiles: string[] = [];

  for (const filename of filenames) {
    if (filename === '.git') continue;
    const fullPath = subpath ? `${subpath}/${filename}` : filename;
    const cached = cachedData?.get(fullPath);
    if (cached) {
      result.set(filename, cached);
    } else {
      uncachedFiles.push(filename);
    }
  }

  // If all files are cached, return early
  if (uncachedFiles.length === 0) {
    return result;
  }

  // Fetch uncached files using native implementation (no wasm needed)
  try {
    const { getFileLastCommitsNative } = await import('./wasmGit');
    const freshData = await getFileLastCommitsNative(rootCid, uncachedFiles, subpath);

    // Initialize cache map if needed
    if (!cachedData) {
      cachedData = new Map();
      fileCommitsCache.set(cacheKey, cachedData);
    }

    // Add fresh data to result and cache
    for (const [filename, commitInfo] of freshData) {
      result.set(filename, commitInfo);
      const fullPath = subpath ? `${subpath}/${filename}` : filename;
      cachedData.set(fullPath, commitInfo);
    }
  } catch {
    // Silently fail for uncached files
  }

  return result;
}

/**
 * Checkout a specific commit - builds a new hashtree directory from the commit's tree
 * Returns the new root CID containing the files at that commit
 * Uses wasm-git (libgit2)
 */
export async function checkoutCommit(
  rootCid: CID,
  commitSha: string,
  onProgress?: (file: string) => void
): Promise<CID> {
  const tree = getTree();

  // Get the .git directory to verify this is a git repo
  const gitDirResult = await tree.resolvePath(rootCid, '.git');
  if (!gitDirResult || gitDirResult.type !== LinkType.Dir) {
    throw new Error('Not a git repository');
  }

  // Use wasm-git to checkout and get files + updated .git
  const { checkoutWithWasmGit } = await import('./wasmGit');
  const { files, gitFiles } = await checkoutWithWasmGit(rootCid, commitSha, onProgress);

  // Build hashtree entries from checkout result
  // First, organize files into a tree structure
  const dirMap = new Map<string, Array<{ name: string; cid: CID; size: number; type: LinkType }>>();
  dirMap.set('', []); // Root directory

  // Process directories first
  for (const file of files) {
    if (file.isDir) {
      dirMap.set(file.name, []);
    }
  }

  // Process files and build from leaves up
  for (const file of files) {
    if (!file.isDir) {
      const { cid, size } = await tree.putFile(file.data);
      const parentDir = file.name.includes('/') ? file.name.substring(0, file.name.lastIndexOf('/')) : '';
      const fileName = file.name.includes('/') ? file.name.substring(file.name.lastIndexOf('/') + 1) : file.name;

      const entries = dirMap.get(parentDir);
      if (entries) {
        entries.push({ name: fileName, cid, size, type: LinkType.Blob });
      }
    }
  }

  // Build directories from deepest to root
  const sortedDirs = Array.from(dirMap.keys()).sort((a, b) => b.split('/').length - a.split('/').length);

  for (const dirPath of sortedDirs) {
    if (dirPath === '') continue; // Skip root for now

    const entries = dirMap.get(dirPath) || [];
    const { cid } = await tree.putDirectory(entries);

    const parentDir = dirPath.includes('/') ? dirPath.substring(0, dirPath.lastIndexOf('/')) : '';
    const dirName = dirPath.includes('/') ? dirPath.substring(dirPath.lastIndexOf('/') + 1) : dirPath;

    const parentEntries = dirMap.get(parentDir);
    if (parentEntries) {
      parentEntries.push({ name: dirName, cid, size: 0, type: LinkType.Dir });
    }
  }

  // Build root directory with updated .git
  const rootEntries = dirMap.get('') || [];

  // Build .git directory from checkout result (contains updated HEAD)
  const gitDirMap = new Map<string, Array<{ name: string; cid: CID; size: number; type: LinkType }>>();
  gitDirMap.set('.git', []);

  // Create directory entries for subdirectories
  for (const file of gitFiles) {
    if (file.isDir && file.name.startsWith('.git/')) {
      gitDirMap.set(file.name, []);
    }
  }

  // Process .git files
  for (const file of gitFiles) {
    if (!file.isDir && file.name.startsWith('.git/')) {
      const { cid, size } = await tree.putFile(file.data);
      const parentDir = file.name.substring(0, file.name.lastIndexOf('/'));
      const fileName = file.name.substring(file.name.lastIndexOf('/') + 1);

      const parentEntries = gitDirMap.get(parentDir);
      if (parentEntries) {
        parentEntries.push({ name: fileName, cid, size, type: LinkType.Blob });
      }
    }
  }

  // Build .git directories from deepest to root
  const sortedGitDirs = Array.from(gitDirMap.keys())
    .filter(d => d !== '.git')
    .sort((a, b) => b.split('/').length - a.split('/').length);

  for (const dirPathName of sortedGitDirs) {
    const dirEntries = gitDirMap.get(dirPathName) || [];
    const { cid } = await tree.putDirectory(dirEntries);

    const parentDir = dirPathName.substring(0, dirPathName.lastIndexOf('/'));
    const dirName = dirPathName.substring(dirPathName.lastIndexOf('/') + 1);

    const parentEntries = gitDirMap.get(parentDir);
    if (parentEntries) {
      parentEntries.push({ name: dirName, cid, size: 0, type: LinkType.Dir });
    }
  }

  // Build .git directory
  const gitEntries = gitDirMap.get('.git') || [];
  const { cid: gitCid } = await tree.putDirectory(gitEntries);

  rootEntries.push({ name: '.git', cid: gitCid, size: 0, type: LinkType.Dir });

  const { cid: finalCid } = await tree.putDirectory(rootEntries);
  return finalCid;
}

export interface RunGitCommandOptions {
  /** Author name for commits */
  authorName?: string;
  /** Author email for commits */
  authorEmail?: string;
}

export interface RunGitCommandResult {
  output: string;
  error?: string;
  /** Updated .git files for write commands - caller should persist these */
  gitFiles?: Array<{ name: string; data: Uint8Array; isDir: boolean }>;
}

/**
 * Run an arbitrary git command in a repository
 * Returns the command output and updated .git files for write commands
 * Read-only commands (diff, show, etc.) are cached by repo hash
 */
export async function runGitCommand(
  rootCid: CID,
  command: string,
  options?: RunGitCommandOptions
): Promise<RunGitCommandResult> {
  // Check if command is cacheable (read-only)
  const cmdName = command.trim().split(/\s+/)[0];
  const isCacheable = CACHEABLE_COMMANDS.includes(cmdName);

  if (isCacheable) {
    const cacheKey = `${toHex(rootCid.hash)}:${command}`;
    const cached = gitCommandCache.get(cacheKey);
    if (cached) {
      return cached;
    }

    const { runGitCommand: runGitCommandWasm } = await import('./wasmGit');
    const result = await runGitCommandWasm(rootCid, command, options);

    // Only cache if no gitFiles returned (pure read operation)
    if (!result.gitFiles) {
      gitCommandCache.set(cacheKey, { output: result.output, error: result.error });
    }

    return result;
  }

  // Non-cacheable command - run directly
  const { runGitCommand: runGitCommandWasm } = await import('./wasmGit');
  return runGitCommandWasm(rootCid, command, options);
}

/**
 * Get diff between two branches
 * Returns diff output, stats, and whether fast-forward is possible
 */
export async function diffBranches(rootCid: CID, baseBranch: string, headBranch: string) {
  const { diffBranchesWithWasmGit } = await import('./wasmGit');
  return await diffBranchesWithWasmGit(rootCid, baseBranch, headBranch);
}

/**
 * Check if branches can be merged without conflicts
 */
export async function canMerge(rootCid: CID, baseBranch: string, headBranch: string) {
  const { canMergeWithWasmGit } = await import('./wasmGit');
  return await canMergeWithWasmGit(rootCid, baseBranch, headBranch);
}

/**
 * Merge head branch into base branch
 * Returns updated .git files to be persisted
 */
export async function mergeBranches(
  rootCid: CID,
  baseBranch: string,
  headBranch: string,
  commitMessage: string,
  authorName: string = 'User',
  authorEmail: string = 'user@example.com'
) {
  const { mergeWithWasmGit } = await import('./wasmGit');
  return await mergeWithWasmGit(rootCid, baseBranch, headBranch, commitMessage, authorName, authorEmail);
}

/**
 * Apply updated .git files to a directory, returning the new root CID
 */
export async function applyGitChanges(
  rootCid: CID,
  gitFiles: Array<{ name: string; data: Uint8Array; isDir: boolean }>
): Promise<CID> {
  const tree = getTree();

  // Build the new .git directory from gitFiles
  // First, organize files into a tree structure
  const dirMap = new Map<string, Array<{ name: string; cid: CID; size: number; type: LinkType }>>();
  dirMap.set('.git', []); // Root .git directory

  // Process directories first (sorted by depth to ensure parents exist)
  const sortedDirs = gitFiles
    .filter(f => f.isDir)
    .sort((a, b) => a.name.split('/').length - b.name.split('/').length);

  for (const dir of sortedDirs) {
    dirMap.set(dir.name, []);
  }

  // Process files
  for (const file of gitFiles) {
    if (file.isDir) continue;

    const { cid, size } = await tree.putFile(file.data);
    const parentDir = file.name.includes('/') ? file.name.substring(0, file.name.lastIndexOf('/')) : '.git';
    const fileName = file.name.includes('/') ? file.name.substring(file.name.lastIndexOf('/') + 1) : file.name;

    const entries = dirMap.get(parentDir);
    if (entries) {
      entries.push({ name: fileName, cid, size, type: LinkType.Blob });
    }
  }

  // Build directories from deepest to root
  const sortedDirKeys = Array.from(dirMap.keys()).sort((a, b) => b.split('/').length - a.split('/').length);

  for (const dirPath of sortedDirKeys) {
    if (dirPath === '.git') continue; // Handle root .git last

    const entries = dirMap.get(dirPath) || [];
    const { cid } = await tree.putDirectory(entries);

    const parentDir = dirPath.includes('/') ? dirPath.substring(0, dirPath.lastIndexOf('/')) : '.git';
    const dirName = dirPath.includes('/') ? dirPath.substring(dirPath.lastIndexOf('/') + 1) : dirPath;

    const parentEntries = dirMap.get(parentDir);
    if (parentEntries) {
      parentEntries.push({ name: dirName, cid, size: 0, type: LinkType.Dir });
    }
  }

  // Build root .git directory
  const gitRootEntries = dirMap.get('.git') || [];
  const { cid: newGitCid } = await tree.putDirectory(gitRootEntries);

  // Replace .git in the root directory
  const newRootCid = await tree.setEntry(rootCid, [], '.git', newGitCid, 0, LinkType.Dir);

  return newRootCid;
}
