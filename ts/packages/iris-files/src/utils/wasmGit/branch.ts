/**
 * Git branch operations
 */
import type { CID } from 'hashtree';
import { LinkType } from 'hashtree';
import { getTree } from '../../store';
import { withWasmGitLock, loadWasmGit, copyToWasmFS, runSilent, rmRf, readGitDirectory, createRepoPath } from './core';
import { getErrorMessage } from '../errorMessage';

/**
 * Get list of branches by reading directly from hashtree
 * No wasm-git needed - just reads .git/HEAD and .git/refs/heads/
 */
export async function getBranchesWithWasmGit(
  rootCid: CID
): Promise<{ branches: string[]; currentBranch: string | null }> {
  const tree = getTree();

  // Check for .git directory
  const gitDirResult = await tree.resolvePath(rootCid, '.git');
  if (!gitDirResult || gitDirResult.type !== LinkType.Dir) {
    return { branches: [], currentBranch: null };
  }

  // Read HEAD file to get current branch
  let currentBranch: string | null = null;
  let headIsDetached = false;
  try {
    const headResult = await tree.resolvePath(gitDirResult.cid, 'HEAD');
    if (headResult && headResult.type !== LinkType.Dir) {
      const headData = await tree.readFile(headResult.cid);
      if (headData) {
        const headContent = new TextDecoder().decode(headData).trim();
        const refMatch = headContent.match(/^ref: refs\/heads\/(\S+)/);
        if (refMatch) {
          currentBranch = refMatch[1];
        } else if (/^[0-9a-f]{40}$/i.test(headContent)) {
          headIsDetached = true;
        }
        // If no match, HEAD is a direct SHA (detached state) - currentBranch stays null
      }
    }
  } catch {
    // HEAD file not found or unreadable
  }

  // Read refs/heads directory to get branch list
  const branches: string[] = [];
  try {
    const refsResult = await tree.resolvePath(gitDirResult.cid, 'refs/heads');
    if (refsResult && refsResult.type === LinkType.Dir) {
      const entries = await tree.listDirectory(refsResult.cid);
      for (const entry of entries) {
        if (entry.type !== LinkType.Dir) {
          branches.push(entry.name);
        }
      }
    }
  } catch {
    // refs/heads may not exist
  }

  if (branches.length === 0) {
    try {
      const packedRefsResult = await tree.resolvePath(gitDirResult.cid, 'packed-refs');
      if (packedRefsResult && packedRefsResult.type !== LinkType.Dir) {
        const packedRefsData = await tree.readFile(packedRefsResult.cid);
        if (packedRefsData) {
          const lines = new TextDecoder().decode(packedRefsData).split('\n');
          for (const rawLine of lines) {
            const line = rawLine.trim();
            if (!line || line.startsWith('#') || line.startsWith('^')) continue;
            const [, ref] = line.split(' ');
            if (ref?.startsWith('refs/heads/')) {
              branches.push(ref.slice('refs/heads/'.length));
            }
          }
        }
      }
    } catch {
      // packed-refs may not exist
    }
  }

  if (!currentBranch && !headIsDetached && branches.length > 0) {
    if (branches.includes('main')) {
      currentBranch = 'main';
    } else if (branches.includes('master')) {
      currentBranch = 'master';
    } else {
      currentBranch = branches[0];
    }
  }

  return { branches, currentBranch };
}

/**
 * Create a new branch using wasm-git
 * Returns the updated .git files that must be persisted to hashtree
 */
export async function createBranchWithWasmGit(
  rootCid: CID,
  branchName: string,
  checkout: boolean = true
): Promise<{ success: boolean; error?: string; gitFiles?: Array<{ name: string; data: Uint8Array; isDir: boolean }> }> {
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

      try {
        module.FS.writeFile('/home/web_user/.gitconfig', '[user]\nname = User\nemail = user@example.com\n');
      } catch {
        // May already exist
      }

      module.FS.chdir(repoPath);

      await copyToWasmFS(module, rootCid, '.');

      // Create the branch
      try {
        if (checkout) {
          runSilent(module, ['checkout', '-b', branchName]);
        } else {
          runSilent(module, ['branch', branchName]);
        }

        // Read updated .git files to return for persistence
        const gitFiles = readGitDirectory(module);
        return { success: true, gitFiles };
      } catch (err) {
        const message = getErrorMessage(err);
        return { success: false, error: message };
      }
    } catch (err) {
      console.error('[wasm-git] createBranch failed:', err);
      return { success: false, error: 'Failed to create branch' };
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
