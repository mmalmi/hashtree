/**
 * Git status operations
 */
import type { CID } from 'hashtree';
import { LinkType } from 'hashtree';
import { getTree } from '../../store';
import { withWasmGitLock, loadWasmGit, copyToWasmFS, rmRf, createRepoPath } from './core';

/**
 * Git status entry from porcelain format
 */
export interface GitStatusEntry {
  /** Two-character status code (XY) */
  status: string;
  /** File path */
  path: string;
  /** Original path (for renames) */
  origPath?: string;
}

/**
 * Git status result
 */
export interface GitStatusResult {
  /** Staged files (to be committed) */
  staged: GitStatusEntry[];
  /** Modified but not staged */
  unstaged: GitStatusEntry[];
  /** Untracked files */
  untracked: GitStatusEntry[];
  /** Whether there are any changes */
  hasChanges: boolean;
}

/**
 * Get git status using wasm-git
 * Returns parsed status with staged, unstaged, and untracked files
 */
export async function getStatusWithWasmGit(
  rootCid: CID
): Promise<GitStatusResult> {
  return withWasmGitLock(async () => {
    const tree = getTree();

    // Check for .git directory
    const gitDirResult = await tree.resolvePath(rootCid, '.git');
    if (!gitDirResult || gitDirResult.type !== LinkType.Dir) {
      return { staged: [], unstaged: [], untracked: [], hasChanges: false };
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

      await copyToWasmFS(module, rootCid, '.');

      // Fix bare=true in config - git-remote-htree creates repos with bare=true
      // but we have a working tree, so we need bare=false for status to work
      try {
        const configPath = '.git/config';
        const configContent = module.FS.readFile(configPath, { encoding: 'utf8' }) as string;
        if (configContent.includes('bare = true')) {
          const fixedConfig = configContent.replace('bare = true', 'bare = false');
          module.FS.writeFile(configPath, fixedConfig);
        }
      } catch {
        // Config read/write failed - continue anyway
      }

      // Check if index file exists
      // git-remote-htree now generates index files during push, but legacy repos may lack them
      try {
        module.FS.stat('.git/index');
      } catch {
        // No index file - can't determine status accurately
        // This happens with repos pushed before git-remote-htree added index generation
        console.warn('[wasm-git] No .git/index found - repo needs to be re-pushed with updated git-remote-htree');
        return { staged: [], unstaged: [], untracked: [], hasChanges: false };
      }

      // Run git status --porcelain
      let output = '';
      try {
        output = module.callWithOutput(['status', '--porcelain']);
      } catch (e) {
        console.error('[wasm-git] git status failed:', e);
        return { staged: [], unstaged: [], untracked: [], hasChanges: false };
      }

      if (!output || output.trim() === '') {
        return { staged: [], unstaged: [], untracked: [], hasChanges: false };
      }

      // Parse porcelain format:
      // XY PATH (with space between XY and PATH)
      // X = index status, Y = working tree status
      // ?? = untracked, A = added, M = modified, D = deleted, R = renamed
      const staged: GitStatusEntry[] = [];
      const unstaged: GitStatusEntry[] = [];
      const untracked: GitStatusEntry[] = [];

      const lines = output.trim().split('\n');
      for (const line of lines) {
        if (line.length < 3) continue;

        const x = line[0]; // Index status
        const y = line[1]; // Working tree status
        // Path starts after XY and optional space (some git versions don't include space)
        const rest = line[2] === ' ' ? line.slice(3) : line.slice(2);

        // Handle renames: "R  old -> new"
        let path = rest;
        let origPath: string | undefined;
        if (rest.includes(' -> ')) {
          const parts = rest.split(' -> ');
          origPath = parts[0];
          path = parts[1];
        }

        const status = x + y;

        if (status === '??') {
          untracked.push({ status, path });
        } else {
          // X indicates staged changes
          if (x !== ' ' && x !== '?') {
            staged.push({ status, path, origPath });
          }
          // Y indicates unstaged changes
          if (y !== ' ' && y !== '?') {
            unstaged.push({ status, path, origPath });
          }
        }
      }

      return {
        staged,
        unstaged,
        untracked,
        hasChanges: staged.length > 0 || unstaged.length > 0 || untracked.length > 0,
      };
    } catch (err) {
      console.error('[wasm-git] getStatus failed:', err);
      return { staged: [], unstaged: [], untracked: [], hasChanges: false };
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
