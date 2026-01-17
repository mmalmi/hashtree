/**
 * Git commit and init operations
 */
import type { CID } from 'hashtree';
import { LinkType } from 'hashtree';
import { getTree } from '../../store';
import { withWasmGitLock, loadWasmGit, copyToWasmFS, readGitDirectory, runSilent, rmRf, createRepoPath } from './core';
import { getErrorMessage } from '../errorMessage';

/**
 * Initialize a git repository in a directory
 * Copies files to wasm-git, runs git init + add + commit, returns .git directory files
 */
export async function initGitRepoWithWasmGit(
  rootCid: CID,
  authorName: string,
  authorEmail: string,
  commitMessage: string = 'Initial commit'
): Promise<Array<{ name: string; data: Uint8Array; isDir: boolean }>> {
  return withWasmGitLock(async () => {
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

      // Copy all files from hashtree to wasm filesystem
      await copyToWasmFS(module, rootCid, '.');

      // Initialize git repo
      runSilent(module, ['init', '.']);

      // Add all files
      runSilent(module, ['add', '.']);

      // Create initial commit
      runSilent(module, ['commit', '-m', commitMessage]);

      // Read .git directory and return files
      return readGitDirectory(module);
    } catch (err) {
      console.error('[wasm-git] init failed:', err);
      throw err;
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
 * Stage files and create a commit using wasm-git
 * Returns the updated .git directory files to be saved back to hashtree
 */
export async function commitWithWasmGit(
  rootCid: CID,
  message: string,
  authorName: string,
  authorEmail: string,
  filesToStage?: string[] // If undefined, stages all changes
): Promise<{ success: boolean; gitFiles?: Array<{ name: string; data: Uint8Array; isDir: boolean }>; error?: string }> {
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

      // Stage files
      try {
        if (filesToStage && filesToStage.length > 0) {
          for (const file of filesToStage) {
            runSilent(module, ['add', file]);
          }
        } else {
          // Stage all changes
          runSilent(module, ['add', '-A']);
        }
      } catch (err) {
        const msg = getErrorMessage(err);
        return { success: false, error: `Failed to stage files: ${msg}` };
      }

      // Create commit
      try {
        runSilent(module, ['commit', '-m', message]);
      } catch (err) {
        const msg = getErrorMessage(err);
        return { success: false, error: `Failed to commit: ${msg}` };
      }

      // Read updated .git directory and return files
      const gitFiles = readGitDirectory(module);
      return { success: true, gitFiles };
    } catch (err) {
      console.error('[wasm-git] commit failed:', err);
      return { success: false, error: 'Failed to commit' };
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
