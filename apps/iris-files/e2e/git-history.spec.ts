import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool, ensureLoggedIn, waitForAppReady } from './test-utils.js';

test.describe('Git history features', () => {
  // Disable "others pool" to prevent WebRTC cross-talk from parallel tests
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
  });

  test('git history should return commits from uploaded git repo', async ({ page }) => {
    test.setTimeout(120000);
    page.setDefaultTimeout(60000);

    // Capture wasm-git debug logs
    const wasmGitLogs: string[] = [];
    page.on('console', msg => {
      const text = msg.text();
      if (text.includes('[wasm-git]')) {
        wasmGitLogs.push(text);
      }
    });

    await navigateToPublicFolder(page);

    // Create a real git repo with commits using CLI
    const fs = await import('fs/promises');
    const path = await import('path');
    const { execSync } = await import('child_process');
    const os = await import('os');

    const tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), 'git-history-test-'));

    try {
      // Initialize git repo and create commits
      execSync('git init', { cwd: tmpDir });
      execSync('git config user.email "test@example.com"', { cwd: tmpDir });
      execSync('git config user.name "Test User"', { cwd: tmpDir });

      // Create first commit
      await fs.writeFile(path.join(tmpDir, 'README.md'), '# Test Repo\n');
      execSync('git add .', { cwd: tmpDir });
      execSync('git commit -m "Initial commit"', { cwd: tmpDir });

      // Create second commit
      await fs.writeFile(path.join(tmpDir, 'file.txt'), 'Hello World\n');
      execSync('git add .', { cwd: tmpDir });
      execSync('git commit -m "Add file.txt"', { cwd: tmpDir });

      // Read all files and directories from the git repo
      interface FileEntry { type: 'file'; path: string; content: number[]; }
      interface DirEntry { type: 'dir'; path: string; }
      type Entry = FileEntry | DirEntry;

      const getAllEntries = async (dir: string, base = ''): Promise<Entry[]> => {
        const entries = await fs.readdir(dir, { withFileTypes: true });
        const result: Entry[] = [];
        for (const entry of entries) {
          const fullPath = path.join(dir, entry.name);
          const relativePath = base ? `${base}/${entry.name}` : entry.name;
          if (entry.isDirectory()) {
            // Add the directory itself
            result.push({ type: 'dir', path: relativePath });
            // Recursively get contents
            result.push(...await getAllEntries(fullPath, relativePath));
          } else {
            const content = await fs.readFile(fullPath);
            result.push({ type: 'file', path: relativePath, content: Array.from(content) });
          }
        }
        return result;
      };

      const shouldIncludePath = (entryPath: string) => {
        if (!entryPath.startsWith('.git/')) return true;
        if (entryPath === '.git/HEAD' || entryPath === '.git/config' || entryPath === '.git/packed-refs') return true;
        if (entryPath.startsWith('.git/objects/')) return true;
        if (entryPath.startsWith('.git/refs/')) return true;
        return false;
      };

      const allEntries = (await getAllEntries(tmpDir)).filter((entry) => shouldIncludePath(entry.path));
      const allFiles = allEntries.filter((e): e is FileEntry => e.type === 'file');
      const allDirs = allEntries.filter((e): e is DirEntry => e.type === 'dir').map(d => d.path);

      // Upload and test getLog
      const result = await page.evaluate(async ({ files, dirs }) => {
        const { getTree, LinkType } = await import('/src/store.ts');
        const tree = getTree();

        // Create root directory
        let { cid: rootCid } = await tree.putDirectory([]);

        // Collect all directory paths from both explicit dirs and parent dirs of files
        const dirPaths = new Set<string>(dirs);
        for (const file of files) {
          const parts = file.path.split('/');
          for (let i = 1; i < parts.length; i++) {
            dirPaths.add(parts.slice(0, i).join('/'));
          }
        }
        const sortedDirs = Array.from(dirPaths).sort((a, b) =>
          a.split('/').length - b.split('/').length
        );

        // Create directories (including empty ones)
        for (const dir of sortedDirs) {
          const parts = dir.split('/');
          const name = parts.pop()!;
          const { cid: emptyDir } = await tree.putDirectory([]);
          rootCid = await tree.setEntry(rootCid, parts, name, emptyDir, 0, LinkType.Dir);
        }

        // Add files
        let objectFilePath = '';
        let objectFileOriginalSize = 0;
        for (const file of files) {
          const parts = file.path.split('/');
          const name = parts.pop()!;
          const data = new Uint8Array(file.content);
          const { cid: fileCid, size } = await tree.putFile(data);
          rootCid = await tree.setEntry(rootCid, parts, name, fileCid, size, LinkType.Blob);
          // Track first git object file for verification
          if (file.path.includes('.git/objects/') && !file.path.endsWith('/info') && !file.path.endsWith('/pack')) {
            if (!objectFilePath) {
              objectFilePath = file.path;
              objectFileOriginalSize = data.length;
            }
          }
        }

        // Verify round-trip of a git object file
        let verifyInfo = '';
        if (objectFilePath) {
          const result = await tree.resolvePath(rootCid, objectFilePath);
          if (result) {
            const readBack = await tree.readFile(result.cid);
            if (readBack) {
              verifyInfo = `Object file ${objectFilePath}: original=${objectFileOriginalSize} bytes, readBack=${readBack.length} bytes`;
              if (objectFileOriginalSize !== readBack.length) {
                verifyInfo += ` MISMATCH!`;
              }
            } else {
              verifyInfo = `Object file ${objectFilePath}: readFile returned null`;
            }
          } else {
            verifyInfo = `Object file ${objectFilePath}: resolvePath returned null`;
          }
        }

        // Test getLog
        const { getLog } = await import('/src/utils/git.ts');

        try {
          const result = await getLog(rootCid, { debug: true }) as any;
          const commits = result.commits || result;
          const debug = result.debug || [];
          return {
            success: true,
            commitCount: Array.isArray(commits) ? commits.length : 0,
            commits: Array.isArray(commits) ? commits.map((c: any) => ({
              message: c.message?.trim() || '',
              author: c.author || ''
            })) : [],
            error: null,
            debug,
            verifyInfo
          };
        } catch (err) {
          return {
            success: false,
            commitCount: 0,
            commits: [],
            error: err instanceof Error ? err.message : String(err),
            debug: []
          };
        }
      }, { files: allFiles, dirs: allDirs });

      // Verify we got commits
      console.log('Git history result:', JSON.stringify(result, null, 2));
      console.log('Wasm-git logs:', wasmGitLogs);
      expect(result.success).toBe(true);
      expect(result.error).toBeNull();
      expect(result.commitCount).toBeGreaterThanOrEqual(2);
      expect(result.commits.some((c: {message: string}) => c.message.includes('Initial commit'))).toBe(true);
      expect(result.commits.some((c: {message: string}) => c.message.includes('Add file.txt'))).toBe(true);

    } finally {
      await fs.rm(tmpDir, { recursive: true, force: true });
    }
  });

  test('git history modal should handle repos without commits gracefully', async ({ page }) => {
    test.setTimeout(60000);
    page.setDefaultTimeout(60000);

    await waitForAppReady(page, 60000);
    await ensureLoggedIn(page, 60000);

    // Test getLog with a minimal .git structure that has no actual commits
    const result = await page.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const tree = getTree();

      // Create minimal .git structure with HEAD pointing to non-existent ref
      const headContent = new TextEncoder().encode('ref: refs/heads/main\n');
      const { cid: headCid } = await tree.putFile(headContent);
      const { cid: emptyDir } = await tree.putDirectory([]);

      // Build .git/refs/heads (empty - no actual branch files)
      let { cid: headsDir } = await tree.putDirectory([]);

      // Build .git/refs directory
      let { cid: refsDir } = await tree.putDirectory([]);
      refsDir = await tree.setEntry(refsDir, [], 'heads', headsDir, 0, LinkType.Dir);

      // Build .git directory
      let { cid: gitDir } = await tree.putDirectory([]);
      gitDir = await tree.setEntry(gitDir, [], 'HEAD', headCid, headContent.length, LinkType.Blob);
      gitDir = await tree.setEntry(gitDir, [], 'refs', refsDir, 0, LinkType.Dir);
      gitDir = await tree.setEntry(gitDir, [], 'objects', emptyDir, 0, LinkType.Dir);

      // Build root with .git directory
      let { cid: rootCid } = await tree.putDirectory([]);
      rootCid = await tree.setEntry(rootCid, [], '.git', gitDir, 0, LinkType.Dir);

      // Try to get log - should not throw, should return empty array
      const { getLog } = await import('/src/utils/git.ts');

      try {
        const commits = await getLog(rootCid);
        return {
          success: true,
          commits,
          error: null
        };
      } catch (err) {
        return {
          success: false,
          commits: null,
          error: err instanceof Error ? err.message : String(err)
        };
      }
    });

    // getLog should succeed and return empty array (not throw)
    expect(result.success).toBe(true);
    expect(result.commits).toEqual([]);
    expect(result.error).toBeNull();
  });

  test('checkout commit should restore files from that commit', async ({ page }) => {
    test.setTimeout(120000);
    page.setDefaultTimeout(60000);

    await navigateToPublicFolder(page);

    // Import Node.js modules
    const fs = await import('fs/promises');
    const path = await import('path');
    const { execSync } = await import('child_process');
    const os = await import('os');

    // Create a git repo with two commits, checkout the first commit
    const tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), 'git-checkout-test-'));

    try {
      // Initialize git repo
      execSync('git init', { cwd: tmpDir });
      execSync('git config user.email "test@example.com"', { cwd: tmpDir });
      execSync('git config user.name "Test User"', { cwd: tmpDir });

      // First commit: create initial file
      await fs.writeFile(path.join(tmpDir, 'file.txt'), 'Version 1\n');
      execSync('git add .', { cwd: tmpDir });
      execSync('git commit -m "Initial commit"', { cwd: tmpDir });

      // Get first commit SHA
      const firstCommit = execSync('git rev-parse HEAD', { cwd: tmpDir }).toString().trim();

      // Second commit: modify file and add another
      await fs.writeFile(path.join(tmpDir, 'file.txt'), 'Version 2\n');
      await fs.writeFile(path.join(tmpDir, 'file2.txt'), 'New file\n');
      execSync('git add .', { cwd: tmpDir });
      execSync('git commit -m "Second commit"', { cwd: tmpDir });

      // Read all files
      interface FileEntry { type: 'file'; path: string; content: number[]; }
      interface DirEntry { type: 'dir'; path: string; }
      type Entry = FileEntry | DirEntry;

      const getAllEntries = async (dir: string, base = ''): Promise<Entry[]> => {
        const entries = await fs.readdir(dir, { withFileTypes: true });
        const result: Entry[] = [];
        for (const entry of entries) {
          const fullPath = path.join(dir, entry.name);
          const relativePath = base ? `${base}/${entry.name}` : entry.name;
          if (entry.isDirectory()) {
            result.push({ type: 'dir', path: relativePath });
            result.push(...await getAllEntries(fullPath, relativePath));
          } else {
            const content = await fs.readFile(fullPath);
            result.push({ type: 'file', path: relativePath, content: Array.from(content) });
          }
        }
        return result;
      };

      const allEntries = await getAllEntries(tmpDir);
      const allFiles = allEntries.filter((e): e is FileEntry => e.type === 'file');
      const allDirs = allEntries.filter((e): e is DirEntry => e.type === 'dir').map(d => d.path);

      // Upload repo and test checkoutCommit
      const result = await page.evaluate(async ({ files, dirs, commitSha }) => {
        const { getTree, LinkType } = await import('/src/store.ts');
        const tree = getTree();

        // Create root directory and upload all files
        let { cid: rootCid } = await tree.putDirectory([]);

        // Create directories
        const dirPaths = new Set<string>(dirs);
        for (const file of files) {
          const parts = file.path.split('/');
          for (let i = 1; i < parts.length; i++) {
            dirPaths.add(parts.slice(0, i).join('/'));
          }
        }
        const sortedDirs = Array.from(dirPaths).sort((a, b) =>
          a.split('/').length - b.split('/').length
        );

        for (const dir of sortedDirs) {
          const parts = dir.split('/');
          const name = parts.pop()!;
          const { cid: emptyDir } = await tree.putDirectory([]);
          rootCid = await tree.setEntry(rootCid, parts, name, emptyDir, 0, LinkType.Dir);
        }

        // Add files
        for (const file of files) {
          const parts = file.path.split('/');
          const name = parts.pop()!;
          const data = new Uint8Array(file.content);
          const { cid: fileCid, size } = await tree.putFile(data);
          rootCid = await tree.setEntry(rootCid, parts, name, fileCid, size, LinkType.Blob);
        }

        // List files before checkout (should have both file.txt and file2.txt)
        const entriesBefore = await tree.listDirectory(rootCid);
        const filesBefore = entriesBefore.filter(e => e.type !== LinkType.Dir && e.name !== '.git').map(e => e.name);

        // Read file.txt content before checkout
        const file1Before = await tree.resolvePath(rootCid, 'file.txt');
        const file1ContentBefore = file1Before ? new TextDecoder().decode(await tree.readFile(file1Before.cid) || new Uint8Array()) : '';

        // Test checkoutCommit
        const { checkoutCommit } = await import('/src/utils/git.ts');

        try {
          const newRootCid = await checkoutCommit(rootCid, commitSha);

          // List files after checkout (should only have file.txt, no file2.txt)
          const entriesAfter = await tree.listDirectory(newRootCid);
          const filesAfter = entriesAfter.filter(e => e.type !== LinkType.Dir && e.name !== '.git').map(e => e.name);

          // Read file.txt content after checkout
          const file1After = await tree.resolvePath(newRootCid, 'file.txt');
          const file1ContentAfter = file1After ? new TextDecoder().decode(await tree.readFile(file1After.cid) || new Uint8Array()) : '';

          // Check if file2.txt exists
          const file2After = await tree.resolvePath(newRootCid, 'file2.txt');

          return {
            success: true,
            filesBefore: filesBefore.sort(),
            filesAfter: filesAfter.sort(),
            file1ContentBefore,
            file1ContentAfter,
            file2ExistsAfter: file2After !== null,
            error: null
          };
        } catch (err) {
          return {
            success: false,
            filesBefore: [],
            filesAfter: [],
            file1ContentBefore: '',
            file1ContentAfter: '',
            file2ExistsAfter: true,
            error: err instanceof Error ? err.message : String(err)
          };
        }
      }, { files: allFiles, dirs: allDirs, commitSha: firstCommit });

      console.log('Checkout result:', JSON.stringify(result, null, 2));
      expect(result.success).toBe(true);
      expect(result.error).toBeNull();

      // Before checkout: both files should exist, file.txt should be "Version 2"
      expect(result.filesBefore).toContain('file.txt');
      expect(result.filesBefore).toContain('file2.txt');
      expect(result.file1ContentBefore.trim()).toBe('Version 2');

      // After checkout to first commit: only file.txt should exist, content should be "Version 1"
      expect(result.filesAfter).toContain('file.txt');
      expect(result.file2ExistsAfter).toBe(false);
      expect(result.file1ContentAfter.trim()).toBe('Version 1');

    } finally {
      await fs.rm(tmpDir, { recursive: true, force: true });
    }
  });
  test('git directory listing should show last commit info for files', async ({ page }) => {
    test.slow(); // This test involves git operations that take time
    // Capture console logs for debugging
    const logs: string[] = [];
    page.on('console', msg => {
      if (msg.text().includes('[wasm-git]') || msg.text().includes('fileCommits')) {
        logs.push(msg.text());
      }
    });

    await navigateToPublicFolder(page);

    // Create a real git repo with commits using CLI
    const fs = await import('fs/promises');
    const path = await import('path');
    const { execSync } = await import('child_process');
    const os = await import('os');

    const tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), 'git-file-commits-'));

    try {
      // Initialize git repo and create commits
      execSync('git init', { cwd: tmpDir });
      execSync('git config user.email "test@example.com"', { cwd: tmpDir });
      execSync('git config user.name "Test User"', { cwd: tmpDir });

      // Create first commit with README
      await fs.writeFile(path.join(tmpDir, 'README.md'), '# Test Repo\n');
      execSync('git add .', { cwd: tmpDir });
      execSync('git commit -m "Add README"', { cwd: tmpDir });

      // Create second commit with src directory
      await fs.mkdir(path.join(tmpDir, 'src'));
      await fs.writeFile(path.join(tmpDir, 'src', 'index.ts'), 'export const x = 1;\n');
      execSync('git add .', { cwd: tmpDir });
      execSync('git commit -m "Add src directory"', { cwd: tmpDir });

      // Read all files and directories
      interface FileEntry { type: 'file'; path: string; content: number[]; }
      interface DirEntry { type: 'dir'; path: string; }
      type Entry = FileEntry | DirEntry;

      const getAllEntries = async (dir: string, base = ''): Promise<Entry[]> => {
        const entries = await fs.readdir(dir, { withFileTypes: true });
        const result: Entry[] = [];
        for (const entry of entries) {
          const fullPath = path.join(dir, entry.name);
          const relativePath = base ? `${base}/${entry.name}` : entry.name;
          if (entry.isDirectory()) {
            result.push({ type: 'dir', path: relativePath });
            result.push(...await getAllEntries(fullPath, relativePath));
          } else {
            const content = await fs.readFile(fullPath);
            result.push({ type: 'file', path: relativePath, content: Array.from(content) });
          }
        }
        return result;
      };

      const allEntries = await getAllEntries(tmpDir);
      const allFiles = allEntries.filter((e): e is FileEntry => e.type === 'file');
      const allDirs = allEntries.filter((e): e is DirEntry => e.type === 'dir').map(d => d.path);

      // Upload and test getFileLastCommits
      const result = await page.evaluate(async ({ files, dirs }) => {
        const { getTree, LinkType } = await import('/src/store.ts');
        const tree = getTree();

        // Create root directory and upload all files
        let { cid: rootCid } = await tree.putDirectory([]);

        // Create directories
        const dirPaths = new Set<string>(dirs);
        for (const file of files) {
          const parts = file.path.split('/');
          for (let i = 1; i < parts.length; i++) {
            dirPaths.add(parts.slice(0, i).join('/'));
          }
        }
        const sortedDirs = Array.from(dirPaths).sort((a, b) =>
          a.split('/').length - b.split('/').length
        );

        for (const dir of sortedDirs) {
          const parts = dir.split('/');
          const name = parts.pop()!;
          const { cid: emptyDir } = await tree.putDirectory([]);
          rootCid = await tree.setEntry(rootCid, parts, name, emptyDir, 0, LinkType.Dir);
        }

        // Add files
        for (const file of files) {
          const parts = file.path.split('/');
          const name = parts.pop()!;
          const data = new Uint8Array(file.content);
          const { cid: fileCid, size } = await tree.putFile(data);
          rootCid = await tree.setEntry(rootCid, parts, name, fileCid, size, LinkType.Blob);
        }

        // List root directory entries
        const entries = await tree.listDirectory(rootCid);
        const entryNames = entries.map(e => e.name);

        // Test getFileLastCommits
        const { getFileLastCommits } = await import('/src/utils/git.ts');
        const fileCommits = await getFileLastCommits(rootCid, entryNames);

        return {
          entryNames,
          fileCommitsSize: fileCommits.size,
          fileCommitsKeys: Array.from(fileCommits.keys()),
          readmeCommit: fileCommits.get('README.md'),
          srcCommit: fileCommits.get('src'),
        };
      }, { files: allFiles, dirs: allDirs });

      console.log('File commits result:', JSON.stringify(result, null, 2));
      console.log('Console logs:', logs);

      // Verify we got commit info for files and directories
      expect(result.fileCommitsSize).toBeGreaterThan(0);
      expect(result.fileCommitsKeys).toContain('README.md');
      expect(result.fileCommitsKeys).toContain('src'); // Directory should also have commit info
      expect(result.readmeCommit?.message).toContain('Add README');
      expect(result.srcCommit?.message).toContain('Add src');

      // Now test the UI - save as a tree and navigate to it
      await page.evaluate(async ({ files, dirs }) => {
        const { getTree, LinkType } = await import('/src/store.ts');
        const { autosaveIfOwn } = await import('/src/nostr.ts');
        const { getCurrentRootCid } = await import('/src/actions/route.ts');
        const { getRouteSync } = await import('/src/stores/index.ts');

        const tree = getTree();
        const route = getRouteSync();
        const rootCid = getCurrentRootCid();
        if (!rootCid) return;

        // Create the git repo directory
        let { cid: repoCid } = await tree.putDirectory([]);

        // Create directories
        const dirPaths = new Set<string>(dirs);
        for (const file of files) {
          const parts = file.path.split('/');
          for (let i = 1; i < parts.length; i++) {
            dirPaths.add(parts.slice(0, i).join('/'));
          }
        }
        const sortedDirs = Array.from(dirPaths).sort((a, b) =>
          a.split('/').length - b.split('/').length
        );

        for (const dir of sortedDirs) {
          const parts = dir.split('/');
          const name = parts.pop()!;
          const { cid: emptyDir } = await tree.putDirectory([]);
          repoCid = await tree.setEntry(repoCid, parts, name, emptyDir, 0, LinkType.Dir);
        }

        // Add files
        for (const file of files) {
          const parts = file.path.split('/');
          const name = parts.pop()!;
          const data = new Uint8Array(file.content);
          const { cid: fileCid, size } = await tree.putFile(data);
          repoCid = await tree.setEntry(repoCid, parts, name, fileCid, size, LinkType.Blob);
        }

        // Add to current directory as "test-git-repo"
        const newRootCid = await tree.setEntry(rootCid, route.path, 'test-git-repo', repoCid, 0, LinkType.Dir);
        autosaveIfOwn(newRootCid);
      }, { files: allFiles, dirs: allDirs });

      // Click into the git repo folder (wait for folder to appear in list)
      const repoLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'test-git-repo' }).first();
      await expect(repoLink).toBeVisible({ timeout: 15000 });
      await repoLink.click();

      // Wait for navigation
      await page.waitForURL(/test-git-repo/, { timeout: 10000 });

      // Set larger viewport to see commit message column
      await page.setViewportSize({ width: 1200, height: 800 });

      // Check that the README row exists
      const readmeRow = page.locator('tr').filter({ hasText: 'README.md' });
      await expect(readmeRow).toBeVisible({ timeout: 15000 });

      // The commit message or relative time should appear in the table
      // Look for "Add README" text or time like "just now" or "ago"
      const commitCell = page.locator('td').filter({ hasText: /Add README|just now|ago/ });
      await expect(commitCell.first()).toBeVisible({ timeout: 15000 });

    } finally {
      await fs.rm(tmpDir, { recursive: true, force: true });
    }
  });

  test('getFileLastCommits should work in subdirectories', { timeout: 30000 }, async ({ page }) => {
    test.slow(); // This test involves git operations that take time

    await navigateToPublicFolder(page);

    // Create a real git repo with commits in subdirectories
    const fs = await import('fs/promises');
    const path = await import('path');
    const { execSync } = await import('child_process');
    const os = await import('os');

    const tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), 'git-subdir-commits-'));

    try {
      // Initialize git repo
      execSync('git init', { cwd: tmpDir });
      execSync('git config user.email "test@example.com"', { cwd: tmpDir });
      execSync('git config user.name "Test User"', { cwd: tmpDir });

      // Create first commit with README
      await fs.writeFile(path.join(tmpDir, 'README.md'), '# Test Repo\n');
      execSync('git add .', { cwd: tmpDir });
      execSync('git commit -m "Add README"', { cwd: tmpDir });

      // Create second commit with src/index.ts
      await fs.mkdir(path.join(tmpDir, 'src'));
      await fs.writeFile(path.join(tmpDir, 'src', 'index.ts'), 'export const x = 1;\n');
      execSync('git add .', { cwd: tmpDir });
      execSync('git commit -m "Add src/index.ts"', { cwd: tmpDir });

      // Create third commit with src/utils/helper.ts
      await fs.mkdir(path.join(tmpDir, 'src', 'utils'));
      await fs.writeFile(path.join(tmpDir, 'src', 'utils', 'helper.ts'), 'export const helper = () => {};\n');
      execSync('git add .', { cwd: tmpDir });
      execSync('git commit -m "Add src/utils/helper.ts"', { cwd: tmpDir });

      // Read all files and directories
      interface FileEntry { type: 'file'; path: string; content: number[]; }
      interface DirEntry { type: 'dir'; path: string; }
      type Entry = FileEntry | DirEntry;

      const getAllEntries = async (dir: string, base = ''): Promise<Entry[]> => {
        const entries = await fs.readdir(dir, { withFileTypes: true });
        const result: Entry[] = [];
        for (const entry of entries) {
          const fullPath = path.join(dir, entry.name);
          const relativePath = base ? `${base}/${entry.name}` : entry.name;
          if (entry.isDirectory()) {
            result.push({ type: 'dir', path: relativePath });
            result.push(...await getAllEntries(fullPath, relativePath));
          } else {
            const content = await fs.readFile(fullPath);
            result.push({ type: 'file', path: relativePath, content: Array.from(content) });
          }
        }
        return result;
      };

      const allEntries = await getAllEntries(tmpDir);
      const allFiles = allEntries.filter((e): e is FileEntry => e.type === 'file');
      const allDirs = allEntries.filter((e): e is DirEntry => e.type === 'dir').map(d => d.path);

      // Upload and test getFileLastCommits in subdirectory
      const result = await page.evaluate(async ({ files, dirs }) => {
        const { getTree, LinkType } = await import('/src/store.ts');
        const tree = getTree();

        // Create root directory and upload all files
        let { cid: rootCid } = await tree.putDirectory([]);

        // Create directories
        const dirPaths = new Set<string>(dirs);
        for (const file of files) {
          const parts = file.path.split('/');
          for (let i = 1; i < parts.length; i++) {
            dirPaths.add(parts.slice(0, i).join('/'));
          }
        }
        const sortedDirs = Array.from(dirPaths).sort((a, b) =>
          a.split('/').length - b.split('/').length
        );

        for (const dir of sortedDirs) {
          const parts = dir.split('/');
          const name = parts.pop()!;
          const { cid: emptyDir } = await tree.putDirectory([]);
          rootCid = await tree.setEntry(rootCid, parts, name, emptyDir, 0, LinkType.Dir);
        }

        // Add files
        for (const file of files) {
          const parts = file.path.split('/');
          const name = parts.pop()!;
          const data = new Uint8Array(file.content);
          const { cid: fileCid, size } = await tree.putFile(data);
          rootCid = await tree.setEntry(rootCid, parts, name, fileCid, size, LinkType.Blob);
        }

        // Test getFileLastCommits in root - should work
        const { getFileLastCommits } = await import('/src/utils/git.ts');
        const rootCommits = await getFileLastCommits(rootCid, ['README.md', 'src']);

        // Test getFileLastCommits in src subdirectory - this is what's broken
        // We need to pass the subpath to the function
        const srcCommits = await getFileLastCommits(rootCid, ['index.ts', 'utils'], 'src');

        // Test getFileLastCommits in src/utils subdirectory
        const utilsCommits = await getFileLastCommits(rootCid, ['helper.ts'], 'src/utils');

        return {
          // Root level
          rootCommitsSize: rootCommits.size,
          rootReadmeCommit: rootCommits.get('README.md'),
          rootSrcCommit: rootCommits.get('src'),
          // src subdirectory level
          srcCommitsSize: srcCommits.size,
          srcIndexCommit: srcCommits.get('index.ts'),
          srcUtilsCommit: srcCommits.get('utils'),
          // src/utils subdirectory level
          utilsCommitsSize: utilsCommits.size,
          utilsHelperCommit: utilsCommits.get('helper.ts'),
        };
      }, { files: allFiles, dirs: allDirs });

      console.log('Subdirectory commits result:', JSON.stringify(result, null, 2));

      // Verify root level works
      expect(result.rootCommitsSize).toBeGreaterThan(0);
      expect(result.rootReadmeCommit?.message).toContain('Add README');
      expect(result.rootSrcCommit?.message).toContain('Add src');

      // Verify src subdirectory commits work
      expect(result.srcCommitsSize).toBeGreaterThan(0);
      expect(result.srcIndexCommit?.message).toContain('Add src/index.ts');
      expect(result.srcUtilsCommit?.message).toContain('Add src/utils');

      // Verify src/utils subdirectory commits work
      expect(result.utilsCommitsSize).toBeGreaterThan(0);
      expect(result.utilsHelperCommit?.message).toContain('Add src/utils/helper.ts');

    } finally {
      await fs.rm(tmpDir, { recursive: true, force: true });
    }
  });

  test('getFileLastCommits should use cache for repeated calls', { timeout: 60000 }, async ({ page }) => {
    await navigateToPublicFolder(page);

    // Create a real git repo with commits
    const fs = await import('fs/promises');
    const path = await import('path');
    const { execSync } = await import('child_process');
    const os = await import('os');

    const tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), 'git-cache-test-'));

    try {
      // Initialize git repo
      execSync('git init', { cwd: tmpDir });
      execSync('git config user.email "test@example.com"', { cwd: tmpDir });
      execSync('git config user.name "Test User"', { cwd: tmpDir });

      // Create commit with files
      await fs.writeFile(path.join(tmpDir, 'file1.txt'), 'content1\n');
      await fs.writeFile(path.join(tmpDir, 'file2.txt'), 'content2\n');
      execSync('git add .', { cwd: tmpDir });
      execSync('git commit -m "Add files"', { cwd: tmpDir });

      // Read all files
      interface FileEntry { type: 'file'; path: string; content: number[]; }
      interface DirEntry { type: 'dir'; path: string; }
      type Entry = FileEntry | DirEntry;

      const getAllEntries = async (dir: string, base = ''): Promise<Entry[]> => {
        const entries = await fs.readdir(dir, { withFileTypes: true });
        const result: Entry[] = [];
        for (const entry of entries) {
          const fullPath = path.join(dir, entry.name);
          const relativePath = base ? `${base}/${entry.name}` : entry.name;
          if (entry.isDirectory()) {
            result.push({ type: 'dir', path: relativePath });
            result.push(...await getAllEntries(fullPath, relativePath));
          } else {
            const content = await fs.readFile(fullPath);
            result.push({ type: 'file', path: relativePath, content: Array.from(content) });
          }
        }
        return result;
      };

      const allEntries = await getAllEntries(tmpDir);
      const allFiles = allEntries.filter((e): e is FileEntry => e.type === 'file');
      const allDirs = allEntries.filter((e): e is DirEntry => e.type === 'dir').map(d => d.path);

      // Test caching behavior
      const result = await page.evaluate(async ({ files, dirs }) => {
        const { getTree, LinkType } = await import('/src/store.ts');
        const tree = getTree();

        let { cid: rootCid } = await tree.putDirectory([]);

        const dirPaths = new Set<string>(dirs);
        for (const file of files) {
          const parts = file.path.split('/');
          for (let i = 1; i < parts.length; i++) {
            dirPaths.add(parts.slice(0, i).join('/'));
          }
        }
        const sortedDirs = Array.from(dirPaths).sort((a, b) =>
          a.split('/').length - b.split('/').length
        );

        for (const dir of sortedDirs) {
          const parts = dir.split('/');
          const name = parts.pop()!;
          const { cid: emptyDir } = await tree.putDirectory([]);
          rootCid = await tree.setEntry(rootCid, parts, name, emptyDir, 0, LinkType.Dir);
        }

        for (const file of files) {
          const parts = file.path.split('/');
          const name = parts.pop()!;
          const data = new Uint8Array(file.content);
          const { cid: fileCid, size } = await tree.putFile(data);
          rootCid = await tree.setEntry(rootCid, parts, name, fileCid, size, LinkType.Blob);
        }

        const { getFileLastCommits } = await import('/src/utils/git.ts');

        // First call - should hit wasm-git
        const start1 = performance.now();
        const commits1 = await getFileLastCommits(rootCid, ['file1.txt', 'file2.txt']);
        const time1 = performance.now() - start1;

        // Second call with same files - should be cached (much faster)
        const start2 = performance.now();
        const commits2 = await getFileLastCommits(rootCid, ['file1.txt', 'file2.txt']);
        const time2 = performance.now() - start2;

        // Third call with just one file - should be cached
        const start3 = performance.now();
        const commits3 = await getFileLastCommits(rootCid, ['file1.txt']);
        const time3 = performance.now() - start3;

        return {
          commits1Size: commits1.size,
          commits2Size: commits2.size,
          commits3Size: commits3.size,
          time1,
          time2,
          time3,
          // Cache should make subsequent calls much faster
          cacheSpeedup: time1 > time2 * 2, // First call should be at least 2x slower
        };
      }, { files: allFiles, dirs: allDirs });

      console.log('Cache test result:', JSON.stringify(result, null, 2));

      // All calls should return correct data
      expect(result.commits1Size).toBe(2);
      expect(result.commits2Size).toBe(2);
      expect(result.commits3Size).toBe(1);

      // Cache should provide speedup (first call > second call)
      // Note: We check that caching returns immediately, but don't strictly enforce timing
      // as it can vary on different systems
      expect(result.time2).toBeLessThan(result.time1);

    } finally {
      await fs.rm(tmpDir, { recursive: true, force: true });
    }
  });

  test('git history modal should infinite scroll to load more commits', { timeout: 120000 }, async ({ page }) => {
    test.slow(); // Creating 60 commits and uploading large repo takes time
    page.setDefaultTimeout(60000);

    await navigateToPublicFolder(page);

    // Create a git repo with many commits (more than initial load of 50)
    const fs = await import('fs/promises');
    const path = await import('path');
    const { execSync } = await import('child_process');
    const os = await import('os');

    const tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), 'git-infinite-scroll-test-'));

    try {
      // Initialize git repo
      execSync('git init', { cwd: tmpDir });
      execSync('git config user.email "test@example.com"', { cwd: tmpDir });
      execSync('git config user.name "Test User"', { cwd: tmpDir });

      // Create 60 commits (more than initial load of 50)
      for (let i = 1; i <= 60; i++) {
        await fs.writeFile(path.join(tmpDir, `file${i}.txt`), `Content ${i}\n`);
        execSync('git add .', { cwd: tmpDir });
        execSync(`git commit -m "Commit ${i}"`, { cwd: tmpDir });
      }

      // Read all files
      interface FileEntry { type: 'file'; path: string; content: number[]; }
      interface DirEntry { type: 'dir'; path: string; }
      type Entry = FileEntry | DirEntry;

      const getAllEntries = async (dir: string, base = ''): Promise<Entry[]> => {
        const entries = await fs.readdir(dir, { withFileTypes: true });
        const result: Entry[] = [];
        for (const entry of entries) {
          const fullPath = path.join(dir, entry.name);
          const relativePath = base ? `${base}/${entry.name}` : entry.name;
          if (entry.isDirectory()) {
            result.push({ type: 'dir', path: relativePath });
            result.push(...await getAllEntries(fullPath, relativePath));
          } else {
            const content = await fs.readFile(fullPath);
            result.push({ type: 'file', path: relativePath, content: Array.from(content) });
          }
        }
        return result;
      };

      const allEntries = await getAllEntries(tmpDir);
      const allFiles = allEntries.filter((e): e is FileEntry => e.type === 'file');
      const allDirs = allEntries.filter((e): e is DirEntry => e.type === 'dir').map(d => d.path);

      // Upload repo to hashtree
      const rootCidHex = await page.evaluate(async ({ files, dirs }) => {
        const { getTree, LinkType } = await import('/src/store.ts');
        const { autosaveIfOwn } = await import('/src/nostr.ts');
        const { getCurrentRootCid } = await import('/src/actions/route.ts');
        const { getRouteSync } = await import('/src/stores/index.ts');

        const tree = getTree();
        const route = getRouteSync();
        const rootCid = getCurrentRootCid();
        if (!rootCid) return null;

        // Create the git repo directory
        let repoCid = (await tree.putDirectory([])).cid;

        // Create directories
        const dirPaths = new Set<string>(dirs);
        for (const file of files) {
          const parts = file.path.split('/');
          for (let i = 1; i < parts.length; i++) {
            dirPaths.add(parts.slice(0, i).join('/'));
          }
        }
        const sortedDirs = Array.from(dirPaths).sort((a, b) =>
          a.split('/').length - b.split('/').length
        );

        for (const dir of sortedDirs) {
          const parts = dir.split('/');
          const name = parts.pop()!;
          const { cid: emptyDir } = await tree.putDirectory([]);
          repoCid = await tree.setEntry(repoCid, parts, name, emptyDir, 0, LinkType.Dir);
        }

        // Add files
        for (const file of files) {
          const parts = file.path.split('/');
          const name = parts.pop()!;
          const data = new Uint8Array(file.content);
          const { cid: fileCid, size } = await tree.putFile(data);
          repoCid = await tree.setEntry(repoCid, parts, name, fileCid, size, LinkType.Blob);
        }

        // Add to current directory
        const newRootCid = await tree.setEntry(rootCid, route.path, 'infinite-scroll-repo', repoCid, 0, LinkType.Dir);
        autosaveIfOwn(newRootCid);

        // Return repoCid hash for verification
        return Array.from(repoCid.hash).map(b => b.toString(16).padStart(2, '0')).join('');
      }, { files: allFiles, dirs: allDirs });

      expect(rootCidHex).not.toBeNull();

      // Navigate to the repo folder
      const repoLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'infinite-scroll-repo' }).first();
      await expect(repoLink).toBeVisible({ timeout: 15000 });
      await repoLink.click();
      await page.waitForURL(/infinite-scroll-repo/, { timeout: 10000 });

      // Click the commits button to open the git history modal
      const commitsButton = page.locator('button:has-text("commits"), button:has(.i-lucide-history)').first();
      await expect(commitsButton).toBeVisible({ timeout: 15000 });
      await commitsButton.click();

      // Wait for the modal to appear
      const modal = page.locator('[data-testid="git-history-modal"]');
      await expect(modal).toBeVisible({ timeout: 10000 });

      // Wait for initial commits to load
      await page.waitForFunction(() => {
        const commits = document.querySelectorAll('[data-testid="git-history-modal"] [class*="timeline"]');
        // Or count commit items by looking for commit hashes (7 char hex)
        const commitItems = document.querySelectorAll('[data-testid="git-history-modal"] .font-mono');
        return commitItems.length > 0;
      }, { timeout: 15000 });

      // Get initial commit count (should be 50)
      const initialCount = await page.evaluate(() => {
        // Count timeline dots which represent commits
        const dots = document.querySelectorAll('[data-testid="git-history-modal"] .rounded-full.bg-accent, [data-testid="git-history-modal"] .rounded-full.bg-success');
        return dots.length;
      });

      // Initial load should be 50 commits
      expect(initialCount).toBe(50);

      // Scroll to the bottom of the modal content
      const scrollContainer = modal.locator('.overflow-auto');
      await scrollContainer.evaluate((el) => {
        el.scrollTo({ top: el.scrollHeight, behavior: 'instant' });
      });

      // Wait for more commits to load automatically (infinite scroll behavior)
      // This should trigger without clicking a "Load more" button
      await page.waitForFunction(() => {
        const dots = document.querySelectorAll('[data-testid="git-history-modal"] .rounded-full.bg-accent, [data-testid="git-history-modal"] .rounded-full.bg-success');
        return dots.length > 50;
      }, { timeout: 15000 });

      // Verify we now have all 60 commits loaded
      const finalCount = await page.evaluate(() => {
        const dots = document.querySelectorAll('[data-testid="git-history-modal"] .rounded-full.bg-accent, [data-testid="git-history-modal"] .rounded-full.bg-success');
        return dots.length;
      });

      expect(finalCount).toBe(60);

      // Verify the "Load more" button is NOT visible (we're using infinite scroll, not button)
      const loadMoreButton = modal.locator('button:has-text("Load more")');
      await expect(loadMoreButton).not.toBeVisible();

    } finally {
      await fs.rm(tmpDir, { recursive: true, force: true });
    }
  });

  test('wasm-git should not spam console with git output', { timeout: 30000 }, async ({ page }) => {
    // Capture ALL console messages to check for git output spam
    const consoleLogs: string[] = [];
    page.on('console', msg => {
      consoleLogs.push(msg.text());
    });

    await navigateToPublicFolder(page);

    // Create a real git repo with commits
    const fs = await import('fs/promises');
    const path = await import('path');
    const { execSync } = await import('child_process');
    const os = await import('os');

    const tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), 'git-spam-test-'));

    try {
      // Initialize git repo and create commits
      execSync('git init', { cwd: tmpDir });
      execSync('git config user.email "test@example.com"', { cwd: tmpDir });
      execSync('git config user.name "Test User"', { cwd: tmpDir });

      await fs.writeFile(path.join(tmpDir, 'README.md'), '# Test\n');
      execSync('git add .', { cwd: tmpDir });
      execSync('git commit -m "Initial commit"', { cwd: tmpDir });

      // Read git repo files
      interface FileEntry { type: 'file'; path: string; content: number[]; }
      interface DirEntry { type: 'dir'; path: string; }
      type Entry = FileEntry | DirEntry;

      const getAllEntries = async (dir: string, base = ''): Promise<Entry[]> => {
        const entries = await fs.readdir(dir, { withFileTypes: true });
        const result: Entry[] = [];
        for (const entry of entries) {
          const fullPath = path.join(dir, entry.name);
          const relativePath = base ? `${base}/${entry.name}` : entry.name;
          if (entry.isDirectory()) {
            result.push({ type: 'dir', path: relativePath });
            result.push(...await getAllEntries(fullPath, relativePath));
          } else {
            const content = await fs.readFile(fullPath);
            result.push({ type: 'file', path: relativePath, content: Array.from(content) });
          }
        }
        return result;
      };

      const allEntries = await getAllEntries(tmpDir);
      const allFiles = allEntries.filter((e): e is FileEntry => e.type === 'file');
      const allDirs = allEntries.filter((e): e is DirEntry => e.type === 'dir').map(d => d.path);

      // Call git operations that would previously spam the console
      await page.evaluate(async ({ files, dirs }) => {
        const { getTree, LinkType } = await import('/src/store.ts');
        const tree = getTree();

        let { cid: rootCid } = await tree.putDirectory([]);

        const dirPaths = new Set<string>(dirs);
        for (const file of files) {
          const parts = file.path.split('/');
          for (let i = 1; i < parts.length; i++) {
            dirPaths.add(parts.slice(0, i).join('/'));
          }
        }
        const sortedDirs = Array.from(dirPaths).sort((a, b) =>
          a.split('/').length - b.split('/').length
        );

        for (const dir of sortedDirs) {
          const parts = dir.split('/');
          const name = parts.pop()!;
          const parentPath = parts;
          const { cid: emptyCid } = await tree.putDirectory([]);
          rootCid = await tree.setEntry(rootCid, parentPath, name, emptyCid, 0, LinkType.Dir);
        }

        for (const file of files) {
          const parts = file.path.split('/');
          const name = parts.pop()!;
          const parentPath = parts;
          const content = new Uint8Array(file.content);
          const { cid: fileCid } = await tree.putFile(content);
          rootCid = await tree.setEntry(rootCid, parentPath, name, fileCid, content.length, LinkType.Blob);
        }

        // Call getLog which uses wasm-git
        const { getLog, getHead } = await import('/src/utils/git.ts');
        const head = await getHead(rootCid);
        const commits = await getLog(rootCid, { depth: 10 });

        return { head, commitCount: commits.length };
      }, { files: allFiles, dirs: allDirs });

      // Check console logs for git command output spam
      // Git output typically contains "commit", "Author:", "Date:" lines
      const gitOutputSpam = consoleLogs.filter(log =>
        // Match typical git log/status output patterns that shouldn't appear
        (log.includes('Author:') && log.includes('<') && log.includes('>')) ||
        (log.match(/^commit [a-f0-9]{40}$/)) ||
        (log.startsWith('Date:') && log.includes('20')) ||
        (log.includes('Initialized empty Git repository')) ||
        (log.match(/^\s+\w.*commit/i) && !log.includes('['))  // Commit message lines (indented)
      );

      // Should not have any git output spam
      if (gitOutputSpam.length > 0) {
        console.log('Found git output spam:', gitOutputSpam);
      }
      expect(gitOutputSpam.length).toBe(0);

    } finally {
      await fs.rm(tmpDir, { recursive: true, force: true });
    }
  });

});
