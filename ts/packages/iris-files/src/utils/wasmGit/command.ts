/**
 * Generic git command execution
 */
import type { CID } from 'hashtree';
import { LinkType } from 'hashtree';
import { getTree } from '../../store';
import { withWasmGitLock, loadWasmGit, copyToWasmFS, readGitDirectory, parseCommandArgs, rmRf, createRepoPath } from './core';
import { getErrorMessage } from '../errorMessage';

/**
 * Run an arbitrary git command in the repository
 * Returns the command output and optionally the updated .git files for write commands
 */
export async function runGitCommand(
  rootCid: CID,
  command: string,
  options?: {
    /** Author name for commits */
    authorName?: string;
    /** Author email for commits */
    authorEmail?: string;
  }
): Promise<{ output: string; error?: string; gitFiles?: Array<{ name: string; data: Uint8Array; isDir: boolean }> }> {
  return withWasmGitLock(async () => {
    const tree = getTree();

    // Check for .git directory
    const gitDirResult = await tree.resolvePath(rootCid, '.git');
    if (!gitDirResult || gitDirResult.type !== LinkType.Dir) {
      return { output: '', error: 'Not a git repository' };
    }

    const module = await loadWasmGit();
    const repoPath = createRepoPath();
    const originalCwd = module.FS.cwd();

    // Detect write commands that modify the repository
    const args = parseCommandArgs(command);
    const writeCommands = ['add', 'commit', 'reset', 'checkout', 'merge', 'rebase', 'cherry-pick', 'revert', 'tag', 'branch', 'rm', 'mv'];
    const isWriteCommand = args.length > 0 && writeCommands.includes(args[0]);

    try {
      module.FS.mkdir(repoPath);

      // Set up git config with user info
      const authorName = options?.authorName || 'User';
      const authorEmail = options?.authorEmail || 'user@example.com';
      try {
        module.FS.writeFile('/home/web_user/.gitconfig', `[user]\nname = ${authorName}\nemail = ${authorEmail}\n`);
      } catch {
        // May already exist
      }

      module.FS.chdir(repoPath);

      // Copy full working directory from hashtree (including .git)
      await copyToWasmFS(module, rootCid, '.');

      if (args.length === 0) {
        return { output: '', error: 'No command provided' };
      }

      // Run the git command
      let output = '';
      try {
        output = module.callWithOutput(args) || '';
      } catch (err) {
        const errorMsg = getErrorMessage(err);
        return { output: '', error: errorMsg };
      }

      // For write commands, read back the updated .git directory
      if (isWriteCommand) {
        const gitFiles = readGitDirectory(module);
        return { output, gitFiles };
      }

      return { output };
    } catch (err) {
      console.error('[wasm-git] runGitCommand failed:', err);
      return { output: '', error: getErrorMessage(err) };
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
