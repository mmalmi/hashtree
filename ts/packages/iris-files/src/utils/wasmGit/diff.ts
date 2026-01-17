/**
 * Branch diff operations using wasm-git
 */
import type { CID } from 'hashtree';
import { LinkType } from 'hashtree';
import { getTree } from '../../store';
import { withWasmGitLock, loadWasmGit, copyToWasmFS, createRepoPath } from './core';
import { getErrorMessage } from '../errorMessage';

export interface BranchDiffStats {
  additions: number;
  deletions: number;
  files: string[];
}

export interface BranchDiffResult {
  diff: string;
  stats: BranchDiffStats;
  canFastForward: boolean;
  error?: string;
}

/**
 * Parse diff output to extract stats
 */
function parseDiffStats(diff: string): BranchDiffStats {
  const stats: BranchDiffStats = {
    additions: 0,
    deletions: 0,
    files: [],
  };

  const lines = diff.split('\n');
  const filesSet = new Set<string>();

  for (const line of lines) {
    if (line.startsWith('diff --git')) {
      const match = line.match(/diff --git a\/(.*) b\/(.*)/);
      if (match) filesSet.add(match[2]);
    } else if (line.startsWith('+') && !line.startsWith('+++')) {
      stats.additions++;
    } else if (line.startsWith('-') && !line.startsWith('---')) {
      stats.deletions++;
    }
  }

  stats.files = Array.from(filesSet);
  return stats;
}

/**
 * Get diff between two branches
 */
export async function diffBranchesWithWasmGit(
  rootCid: CID,
  baseBranch: string,
  headBranch: string
): Promise<BranchDiffResult> {
  return withWasmGitLock(async () => {
    const tree = getTree();

    // Check for .git directory
    const gitDirResult = await tree.resolvePath(rootCid, '.git');
    if (!gitDirResult || gitDirResult.type !== LinkType.Dir) {
      return { diff: '', stats: { additions: 0, deletions: 0, files: [] }, canFastForward: false, error: 'Not a git repository' };
    }

    const module = await loadWasmGit();
    const repoPath = createRepoPath();
    const originalCwd = module.FS.cwd();

    try {
      module.FS.mkdir(repoPath);
      module.FS.chdir(repoPath);

      await copyToWasmFS(module, rootCid, '.');

      // Get diff between branches using git diff base head
      // wasm-git doesn't support the ... syntax, so we use two-dot diff
      let diff = '';
      try {
        diff = module.callWithOutput(['diff', baseBranch, headBranch]) || '';
      } catch (_err) {
        const errorMsg = _err instanceof Error ? _err.message : String(_err);
        return { diff: '', stats: { additions: 0, deletions: 0, files: [] }, canFastForward: false, error: `Failed to diff branches: ${errorMsg}` };
      }

      const stats = parseDiffStats(diff);

      // Check if this can be a fast-forward merge
      // Fast-forward is possible if base is an ancestor of head
      let canFastForward = false;
      try {
        const mergeBaseOutput = module.callWithOutput(['merge-base', baseBranch, headBranch]) || '';
        const mergeBase = mergeBaseOutput.trim();

        // Get the commit hash of the base branch
        const baseRefOutput = module.callWithOutput(['rev-parse', baseBranch]) || '';
        const baseCommit = baseRefOutput.trim();

        // If merge-base equals base branch, it's a fast-forward
        canFastForward = mergeBase === baseCommit;
      } catch {
        // If merge-base fails, assume not fast-forward
        canFastForward = false;
      }

      return { diff, stats, canFastForward };
    } catch (err) {
      console.error('[wasm-git] diffBranches failed:', err);
      return { diff: '', stats: { additions: 0, deletions: 0, files: [] }, canFastForward: false, error: getErrorMessage(err) };
    } finally {
      try {
        module.FS.chdir(originalCwd);
      } catch {
        // Ignore
      }
    }
  });
}

/**
 * Check if branches can be merged without conflicts
 */
export async function canMergeWithWasmGit(
  rootCid: CID,
  baseBranch: string,
  headBranch: string
): Promise<{ canMerge: boolean; conflicts: string[]; isFastForward: boolean; error?: string }> {
  return withWasmGitLock(async () => {
    const tree = getTree();

    // Check for .git directory
    const gitDirResult = await tree.resolvePath(rootCid, '.git');
    if (!gitDirResult || gitDirResult.type !== LinkType.Dir) {
      return { canMerge: false, conflicts: [], isFastForward: false, error: 'Not a git repository' };
    }

    const module = await loadWasmGit();
    const repoPath = createRepoPath();
    const originalCwd = module.FS.cwd();

    try {
      module.FS.mkdir(repoPath);
      module.FS.chdir(repoPath);

      await copyToWasmFS(module, rootCid, '.');

      // Check for fast-forward possibility
      let isFastForward = false;
      try {
        const mergeBaseOutput = module.callWithOutput(['merge-base', baseBranch, headBranch]) || '';
        const mergeBase = mergeBaseOutput.trim();

        const baseRefOutput = module.callWithOutput(['rev-parse', baseBranch]) || '';
        const baseCommit = baseRefOutput.trim();

        isFastForward = mergeBase === baseCommit;
      } catch {
        isFastForward = false;
      }

      // If fast-forward, no conflicts possible
      if (isFastForward) {
        return { canMerge: true, conflicts: [], isFastForward: true };
      }

      // Checkout base branch first
      try {
        module.callWithOutput(['checkout', baseBranch]);
      } catch (_err) {
        const errorMsg = _err instanceof Error ? _err.message : String(_err);
        return { canMerge: false, conflicts: [], isFastForward: false, error: `Failed to checkout ${baseBranch}: ${errorMsg}` };
      }

      // Try merge with --no-commit to check for conflicts
      try {
        module.callWithOutput(['merge', '--no-commit', '--no-ff', headBranch]);
        // If we get here, merge is possible
        // Abort the merge to clean up
        try {
          module.callWithOutput(['merge', '--abort']);
        } catch {
          // Ignore abort errors
        }
        return { canMerge: true, conflicts: [], isFastForward: false };
      } catch {
        // Merge failed, check for conflicts
        const conflicts: string[] = [];
        try {
          const statusOutput = module.callWithOutput(['status', '--porcelain']) || '';
          const lines = statusOutput.split('\n');
          for (const line of lines) {
            // UU = both modified (conflict)
            // AA = both added
            // DD = both deleted
            if (line.match(/^(UU|AA|DD|AU|UA|DU|UD)/)) {
              const file = line.slice(3).trim();
              if (file) conflicts.push(file);
            }
          }
        } catch {
          // Can't get status
        }

        // Abort the merge
        try {
          module.callWithOutput(['merge', '--abort']);
        } catch {
          // Ignore abort errors
        }

        return { canMerge: conflicts.length === 0, conflicts, isFastForward: false };
      }
    } catch (_err) {
      console.error('[wasm-git] canMerge failed:', _err);
      return { canMerge: false, conflicts: [], isFastForward: false, error: _err instanceof Error ? _err.message : String(_err) };
    } finally {
      try {
        module.FS.chdir(originalCwd);
      } catch {
        // Ignore
      }
    }
  });
}
