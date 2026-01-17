import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool } from './test-utils.js';

test.describe('Git basic features', () => {
  test.describe.configure({ timeout: 90000 });
  // Disable "others pool" to prevent WebRTC cross-talk from parallel tests
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
  });

  test('navigating to .git directory should show directory view not file download', { timeout: 30000 }, async ({ page }) => {
    test.slow(); // File operations can be slow under parallel load
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // Navigate to tree list and create a folder
    await page.locator('header a:has-text("Iris")').click();
    const newFolderButton = page.getByRole('button', { name: 'New Folder' }).first();
    await expect(newFolderButton).toBeVisible({ timeout: 5000 });
    await newFolderButton.click();

    const input = page.locator('input[placeholder="Folder name..."]');
    await input.waitFor({ timeout: 5000 });
    await input.fill('nav-dotfile-test');
    await page.click('button:has-text("Create")');

    // Wait for modal to close
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Create .git directory
    await page.getByRole('button', { name: 'New Folder' }).click();
    const folderInput = page.locator('input[placeholder="Folder name..."]');
    await folderInput.waitFor({ timeout: 5000 });
    await folderInput.fill('.git');
    await page.click('button:has-text("Create")');
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Wait for .git to appear in the file list and click it
    // The entry is a Link (<a>) with a child span containing the folder name
    const gitEntry = page.locator('[data-testid="file-list"] a').filter({ hasText: '.git' }).first();
    await expect(gitEntry).toBeVisible({ timeout: 15000 });

    // Click on .git to navigate into it
    await gitEntry.click();
    await expect(page).toHaveURL(/\.git/, { timeout: 10000 });

    // Check URL has .git in path
    const url = page.url();
    expect(url).toContain('.git');

    // Should see "Empty directory" message since we're viewing it as a directory
    // NOT a download button for binary file
    const emptyDir = page.locator('text=Empty directory');
    const downloadButton = page.locator('button:has-text("Download")');

    // At least one of these should be true:
    // 1. We see "Empty directory" (correct - viewing as directory)
    // 2. We don't see a Download button (correct - not treating as file)
    const emptyVisible = await emptyDir.isVisible().catch(() => false);
    const downloadVisible = await downloadButton.isVisible().catch(() => false);

    // If we see Download button, we're incorrectly treating .git as a file
    expect(downloadVisible).toBe(false);
  });

  test('dotfiles like .git and .claude should be treated as directories', async ({ page }) => {
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // Navigate to tree list and create a folder
    await page.locator('header a:has-text("Iris")').click();
    const newFolderButton = page.getByRole('button', { name: 'New Folder' }).first();
    await expect(newFolderButton).toBeVisible({ timeout: 5000 });
    await newFolderButton.click();

    const input = page.locator('input[placeholder="Folder name..."]');
    await input.waitFor({ timeout: 5000 });
    await input.fill('dotfile-test');
    await page.click('button:has-text("Create")');

    // Wait for modal to close
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });
    await expect(page.locator('text=Empty directory')).toBeVisible({ timeout: 10000 });

    // Create .git and .claude directories via the tree API
    const result = await page.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const tree = getTree();

      // Create root with .git and .claude directories and a regular file
      const { cid: emptyDir } = await tree.putDirectory([]);
      const { cid: fileCid, size } = await tree.putFile(new TextEncoder().encode('test content'));

      let { cid: rootCid } = await tree.putDirectory([]);

      // Add .git directory
      rootCid = await tree.setEntry(rootCid, [], '.git', emptyDir, 0, LinkType.Dir);

      // Add .claude directory
      rootCid = await tree.setEntry(rootCid, [], '.claude', emptyDir, 0, LinkType.Dir);

      // Add a regular file with extension
      rootCid = await tree.setEntry(rootCid, [], 'readme.txt', fileCid, size, LinkType.Blob);

      // List the entries
      const entries = await tree.listDirectory(rootCid);

      return {
        entries: entries.map(e => ({ name: e.name, isDir: e.type === LinkType.Dir })),
      };
    });

    // Verify .git and .claude are directories, readme.txt is a file
    expect(result.entries).toContainEqual({ name: '.git', isDir: true });
    expect(result.entries).toContainEqual({ name: '.claude', isDir: true });
    expect(result.entries).toContainEqual({ name: 'readme.txt', isDir: false });
  });

  test('should detect git repo and show git features when .git directory exists', async ({ page }) => {
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // Navigate to tree list and create a folder
    await page.locator('header a:has-text("Iris")').click();
    const newFolderButton = page.getByRole('button', { name: 'New Folder' }).first();
    await expect(newFolderButton).toBeVisible({ timeout: 5000 });
    await newFolderButton.click();

    const input = page.locator('input[placeholder="Folder name..."]');
    await input.waitFor({ timeout: 5000 });
    await input.fill('git-repo-test');
    await page.click('button:has-text("Create")');

    // Wait for modal to close
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Create a minimal git repo structure via the tree API
    const result = await page.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const tree = getTree();

      // Create minimal .git structure
      // .git/HEAD - contains ref to current branch
      // .git/config - basic config
      // .git/refs/heads/main - branch ref
      // .git/objects/ - object store (empty for now)

      const headContent = new TextEncoder().encode('ref: refs/heads/main\n');
      const configContent = new TextEncoder().encode('[core]\n\trepositoryformatversion = 0\n');
      const mainRefContent = new TextEncoder().encode('0000000000000000000000000000000000000000\n');

      const { cid: headCid } = await tree.putFile(headContent);
      const { cid: configCid } = await tree.putFile(configContent);
      const { cid: mainRefCid } = await tree.putFile(mainRefContent);
      const { cid: emptyDir } = await tree.putDirectory([]);

      // Build .git/refs/heads directory with main branch
      let { cid: headsDir } = await tree.putDirectory([]);
      headsDir = await tree.setEntry(headsDir, [], 'main', mainRefCid, mainRefContent.length, LinkType.Blob);

      // Build .git/refs directory
      let { cid: refsDir } = await tree.putDirectory([]);
      refsDir = await tree.setEntry(refsDir, [], 'heads', headsDir, 0, LinkType.Dir);

      // Build .git directory
      let { cid: gitDir } = await tree.putDirectory([]);
      gitDir = await tree.setEntry(gitDir, [], 'HEAD', headCid, headContent.length, LinkType.Blob);
      gitDir = await tree.setEntry(gitDir, [], 'config', configCid, configContent.length, LinkType.Blob);
      gitDir = await tree.setEntry(gitDir, [], 'refs', refsDir, 0, LinkType.Dir);
      gitDir = await tree.setEntry(gitDir, [], 'objects', emptyDir, 0, LinkType.Dir);

      // Build root with .git directory
      let { cid: rootCid } = await tree.putDirectory([]);
      rootCid = await tree.setEntry(rootCid, [], '.git', gitDir, 0, LinkType.Dir);

      // Check if it's detected as a git repo
      const { isGitRepo } = await import('/src/utils/git.ts');
      const isRepo = await isGitRepo(rootCid);

      return { isRepo };
    });

    expect(result.isRepo).toBe(true);
  });

  test('.git directory should be uploaded when adding a folder', async ({ page }) => {
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // Check that .git is NOT in the default ignore patterns
    const result = await page.evaluate(async () => {
      const { DEFAULT_IGNORE_PATTERNS } = await import('/src/utils/gitignore.ts');

      // Check if any pattern matches .git
      const hasGitIgnore = DEFAULT_IGNORE_PATTERNS.some(p =>
        p.pattern.includes('.git') || p.regex.test('.git')
      );

      return {
        hasGitIgnore,
        patterns: DEFAULT_IGNORE_PATTERNS.map(p => p.pattern)
      };
    });

    // .git should NOT be in default ignore patterns
    expect(result.hasGitIgnore).toBe(false);
    expect(result.patterns).not.toContain('.git/');
    expect(result.patterns).not.toContain('.git');
  });

  test('git repo structure is preserved when uploading .git directory', { timeout: 30000 }, async ({ page }) => {
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // Create a real git repo with commits using CLI
    const fs = await import('fs/promises');
    const path = await import('path');
    const { execSync } = await import('child_process');
    const os = await import('os');

    const tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), 'git-test-'));

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

      // Read all files from the git repo to inject via page.evaluate
      const getAllFiles = async (dir: string, base = ''): Promise<Array<{path: string, content: number[]}>> => {
        const entries = await fs.readdir(dir, { withFileTypes: true });
        const files: Array<{path: string, content: number[]}> = [];
        for (const entry of entries) {
          const fullPath = path.join(dir, entry.name);
          const relativePath = base ? `${base}/${entry.name}` : entry.name;
          if (entry.isDirectory()) {
            files.push(...await getAllFiles(fullPath, relativePath));
          } else {
            const content = await fs.readFile(fullPath);
            files.push({ path: relativePath, content: Array.from(content) });
          }
        }
        return files;
      };

      const allFiles = await getAllFiles(tmpDir);

      // Inject files directly via tree API
      const result = await page.evaluate(async (files) => {
        const { getTree, LinkType } = await import('/src/store.ts');
        const tree = getTree();

        // Create root directory
        let { cid: rootCid } = await tree.putDirectory([]);

        // Collect all directory paths and sort by depth
        const dirPaths = new Set<string>();
        for (const file of files) {
          const parts = file.path.split('/');
          for (let i = 1; i < parts.length; i++) {
            dirPaths.add(parts.slice(0, i).join('/'));
          }
        }
        const sortedDirs = Array.from(dirPaths).sort((a, b) =>
          a.split('/').length - b.split('/').length
        );

        // Create directories
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

        // Verify .git structure was preserved
        const entries = await tree.listDirectory(rootCid);
        const hasGit = entries.some(e => e.name === '.git' && e.type === LinkType.Dir);

        if (!hasGit) {
          return { error: 'No .git directory found', hasGit: false };
        }

        // Check HEAD points to a valid ref
        const headRes = await tree.resolvePath(rootCid, '.git/HEAD');
        const headContent = headRes ? new TextDecoder().decode((await tree.readFile(headRes.cid))!) : '';

        // Check refs directory exists
        const refsRes = await tree.resolvePath(rootCid, '.git/refs/heads');
        const refEntries = refsRes ? (await tree.listDirectory(refsRes.cid)).map(e => e.name) : [];

        // Check objects directory has content (2-char subdirs like 30, a8, etc)
        const objectsRes = await tree.resolvePath(rootCid, '.git/objects');
        const objectDirs = objectsRes
          ? (await tree.listDirectory(objectsRes.cid))
              .filter(e => e.type === LinkType.Dir && e.name.length === 2)
              .map(e => e.name)
          : [];

        return {
          error: null,
          hasGit,
          headContent: headContent.trim(),
          refEntries,
          objectDirCount: objectDirs.length,
          fileCount: files.length
        };
      }, allFiles);

      // Verify git structure is intact
      expect(result.error).toBeNull();
      expect(result.hasGit).toBe(true);
      expect(result.headContent).toMatch(/^ref: refs\/heads\//); // HEAD points to a branch
      expect(result.refEntries.length).toBeGreaterThan(0); // At least one branch
      expect(result.objectDirCount).toBeGreaterThan(0); // Objects were stored
      expect(result.fileCount).toBeGreaterThan(10); // A real git repo has many files

    } finally {
      await fs.rm(tmpDir, { recursive: true, force: true });
    }
  });

});
