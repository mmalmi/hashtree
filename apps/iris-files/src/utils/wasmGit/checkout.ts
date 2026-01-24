/**
 * Git checkout operations
 */
import type { CID } from '@hashtree/core';
import { LinkType } from '@hashtree/core';
import { getTree } from '../../store';
import { withWasmGitLock, loadWasmGit, copyToWasmFS, runSilent, rmRf, createRepoPath } from './core';
import { getHead } from './log';

/**
 * Checkout a specific commit using wasm-git
 * Returns files from that commit as a directory listing, plus the updated .git directory
 */
export async function checkoutWasm(
  rootCid: CID,
  commitSha: string,
  onProgress?: (file: string) => void
): Promise<{ files: Array<{ name: string; data: Uint8Array; isDir: boolean }>; gitFiles: Array<{ name: string; data: Uint8Array; isDir: boolean }> }> {
  return withWasmGitLock(async () => {
    const tree = getTree();

    // Check for .git directory
    const gitDirResult = await tree.resolvePath(rootCid, '.git');
    if (!gitDirResult || gitDirResult.type !== LinkType.Dir) {
      throw new Error('Not a git repository');
    }

    const isCommitSha = /^[0-9a-f]{40}$/i.test(commitSha);
    let originalHeadRef: string | null = null;
    let originalHeadCommit: string | null = null;

    if (isCommitSha) {
      try {
        const headResult = await tree.resolvePath(gitDirResult.cid, 'HEAD');
        if (headResult && headResult.type !== LinkType.Dir) {
          const headData = await tree.readFile(headResult.cid);
          if (headData) {
            const headContent = new TextDecoder().decode(headData).trim();
            const refMatch = headContent.match(/^ref:\s+(.+)$/);
            if (refMatch) {
              originalHeadRef = refMatch[1];
              originalHeadCommit = await getHead(rootCid);
            }
          }
        }
      } catch {
        // Ignore - we'll proceed without restoring branch refs.
      }
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

      // Checkout the commit
      try {
        runSilent(module, ['checkout', '--force', commitSha]);
      } catch (err) {
        console.error('[wasm-git] checkout error:', err);
        throw new Error(`Failed to checkout ${commitSha}: ${err}`);
      }

      // Read all files from the working directory (excluding .git)
      const files: Array<{ name: string; data: Uint8Array; isDir: boolean }> = [];

      function readDir(path: string, prefix: string, skipGit: boolean): void {
        const entries = module.FS.readdir(path);
        for (const entry of entries) {
          if (entry === '.' || entry === '..') continue;
          if (skipGit && entry === '.git') continue;

          const fullPath = path === '.' ? entry : `${path}/${entry}`;
          const relativePath = prefix ? `${prefix}/${entry}` : entry;

          try {
            const stat = module.FS.stat(fullPath);
            // Emscripten's stat returns mode as number, check S_IFDIR (0o40000)
            const isDir = (stat.mode & 0o170000) === 0o040000;
            if (isDir) {
              files.push({ name: relativePath, data: new Uint8Array(0), isDir: true });
              readDir(fullPath, relativePath, skipGit);
            } else {
              if (onProgress) onProgress(relativePath);
              const data = module.FS.readFile(fullPath) as Uint8Array;
              files.push({ name: relativePath, data, isDir: false });
            }
          } catch {
            // Skip files we can't read
          }
        }
      }

      readDir('.', '', true);

      // Also read the updated .git directory
      const gitFiles: Array<{ name: string; data: Uint8Array; isDir: boolean }> = [];

      function readGitDir(path: string, prefix: string): void {
        const entries = module.FS.readdir(path);
        for (const entry of entries) {
          if (entry === '.' || entry === '..') continue;

          const fullPath = `${path}/${entry}`;
          const relativePath = prefix ? `${prefix}/${entry}` : entry;

          try {
            const stat = module.FS.stat(fullPath);
            const isDir = (stat.mode & 0o170000) === 0o040000;
            if (isDir) {
              gitFiles.push({ name: relativePath, data: new Uint8Array(0), isDir: true });
              readGitDir(fullPath, relativePath);
            } else {
              const data = module.FS.readFile(fullPath) as Uint8Array;
              gitFiles.push({ name: relativePath, data, isDir: false });
            }
          } catch {
            // Skip files we can't read
          }
        }
      }

      readGitDir('.git', '.git');

      // Ensure detached HEAD when checking out a specific commit SHA.
      if (isCommitSha) {
        const headEntry = gitFiles.find(file => file.name === '.git/HEAD' && !file.isDir);
        const headContent = headEntry ? new TextDecoder().decode(headEntry.data).trim() : '';
        if (!/^[0-9a-f]{40}$/i.test(headContent) || headContent.toLowerCase() !== commitSha.toLowerCase()) {
          const data = new TextEncoder().encode(`${commitSha}\n`);
          if (headEntry) {
            headEntry.data = data;
          } else {
            gitFiles.push({ name: '.git/HEAD', data, isDir: false });
          }
        }

        if (originalHeadRef && originalHeadCommit) {
          const refPath = `.git/${originalHeadRef}`;
          const refEntry = gitFiles.find(file => file.name === refPath && !file.isDir);
          const refData = new TextEncoder().encode(`${originalHeadCommit}\n`);
          if (refEntry) {
            refEntry.data = refData;
          } else {
            gitFiles.push({ name: refPath, data: refData, isDir: false });
          }
        }
      }

      return { files, gitFiles };
    } catch (err) {
      console.error('[wasm-git] checkout failed:', err);
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
