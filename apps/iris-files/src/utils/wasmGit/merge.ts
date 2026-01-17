/**
 * Git merge operations using wasm-git
 *
 * Note: wasm-git's merge command only works with remote-tracking branches (origin/master etc).
 * For local branch merges, we implement fast-forward merge manually by updating refs.
 */
import type { CID } from 'hashtree';
import { LinkType } from 'hashtree';
import { getTree } from '../../store';
import { withWasmGitLock, loadWasmGit, copyToWasmFS, readGitDirectory, runSilent, rmRf, createRepoPath } from './core';
import { getErrorMessage } from '../errorMessage';

export interface MergeResult {
  success: boolean;
  newRootCid?: CID;
  gitFiles?: Array<{ name: string; data: Uint8Array; isDir: boolean }>;
  workingFiles?: Array<{ name: string; data: Uint8Array; isDir: boolean }>;
  conflicts?: string[];
  error?: string;
  isFastForward?: boolean;
}

/**
 * Merge head branch into base branch
 *
 * Since wasm-git doesn't support merging local branches (only remote-tracking branches),
 * we implement fast-forward merge manually by:
 * 1. Checking if it's a fast-forward (base is ancestor of head)
 * 2. Updating the base branch ref to point to the head commit
 * 3. Checking out the head branch files
 * 4. Updating the index to match
 */
export async function mergeWithWasmGit(
  rootCid: CID,
  baseBranch: string,
  headBranch: string,
  commitMessage: string,
  authorName: string = 'User',
  authorEmail: string = 'user@example.com'
): Promise<MergeResult> {
  return withWasmGitLock(async () => {
    const tree = getTree();

    // Check for .git directory
    const gitDirResult = await tree.resolvePath(rootCid, '.git');
    if (!gitDirResult || gitDirResult.type !== LinkType.Dir) {
      return { success: false, error: 'Not a git repository' };
    }

    const module = await loadWasmGit();
    const repoPath = createRepoPath();
    const originalCwd = module.FS.cwd();

    try {
      module.FS.mkdir(repoPath);

      // Set up git config with user info
      try {
        module.FS.writeFile('/home/web_user/.gitconfig', `[user]\nname = ${authorName}\nemail = ${authorEmail}\n`);
      } catch {
        // May already exist
      }

      module.FS.chdir(repoPath);

      await copyToWasmFS(module, rootCid, '.');

      // Get commit hashes for both branches
      let baseCommit: string;
      let headCommit: string;
      try {
        baseCommit = (module.callWithOutput(['rev-parse', baseBranch]) || '').trim();
        headCommit = (module.callWithOutput(['rev-parse', headBranch]) || '').trim();
      } catch (err) {
        const errorMsg = getErrorMessage(err);
        return { success: false, error: `Failed to get branch commits: ${errorMsg}` };
      }

      // Check if this is a fast-forward merge (base is ancestor of head)
      // For fast-forward: base commit is an ancestor of head commit
      // To check: merge-base(base, head) should equal base commit
      let isFastForward = false;
      let mergeBase = '';
      try {
        mergeBase = (module.callWithOutput(['merge-base', baseBranch, headBranch]) || '').trim();
        isFastForward = mergeBase === baseCommit;
      } catch {
        // If merge-base fails, try an alternative: check if head's ancestor list includes base
        // This can happen if the repo was created without proper history
      }

      // If merge-base returned empty but commits are different, check if head is descendant of base
      // by seeing if we can rev-list from head back to base
      if (!mergeBase && baseCommit && headCommit && baseCommit !== headCommit) {
        try {
          // Check if base is reachable from head
          const ancestors = module.callWithOutput(['rev-list', headBranch]) || '';
          const ancestorList = ancestors.split('\n').map((s: string) => s.trim()).filter((s: string) => s);
          if (ancestorList.includes(baseCommit)) {
            isFastForward = true;
          }
        } catch {
          // rev-list failed, keep isFastForward as false
        }
      }

      if (!isFastForward) {
        // Non-fast-forward merge requires creating a merge commit
        // wasm-git doesn't support this for local branches
        // For now, return an error - we could implement this manually by creating a commit with two parents
        return {
          success: false,
          error: 'Non-fast-forward merge not yet supported. Please rebase or merge manually.',
          conflicts: []
        };
      }

      // Fast-forward merge: update the base branch ref to point to head commit
      const refPath = `.git/refs/heads/${baseBranch}`;
      module.FS.writeFile(refPath, headCommit + '\n');

      // Checkout the base branch to update the working tree and index
      try {
        runSilent(module, ['checkout', baseBranch]);
      } catch {
        // Continue anyway - the ref is updated
      }

      // Read the updated .git directory
      const gitFiles = readGitDirectory(module);

      // Read working directory files
      const workingFiles: Array<{ name: string; data: Uint8Array; isDir: boolean }> = [];

      function readWorkingDir(dirPath: string, prefix: string): void {
        try {
          const entries = module.FS.readdir(dirPath);
          for (const entry of entries) {
            if (entry === '.' || entry === '..' || entry === '.git') continue;

            const fullPath = `${dirPath}/${entry}`;
            const relativePath = prefix ? `${prefix}/${entry}` : entry;

            try {
              const stat = module.FS.stat(fullPath);
              const isDir = (stat.mode & 0o170000) === 0o040000;
              if (isDir) {
                workingFiles.push({ name: relativePath, data: new Uint8Array(0), isDir: true });
                readWorkingDir(fullPath, relativePath);
              } else {
                const data = module.FS.readFile(fullPath) as Uint8Array;
                workingFiles.push({ name: relativePath, data, isDir: false });
              }
            } catch {
              // Skip files we can't read
            }
          }
        } catch {
          // Skip directories we can't read
        }
      }

      readWorkingDir('.', '');

      return { success: true, gitFiles, workingFiles, isFastForward: true };
    } catch (err) {
      console.error('[wasm-git] merge failed:', err);
      return { success: false, error: getErrorMessage(err) };
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
