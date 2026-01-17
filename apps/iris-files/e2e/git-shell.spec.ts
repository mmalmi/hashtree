import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool } from './test-utils.js';

test.describe('Git shell features', () => {
  // Disable "others pool" to prevent WebRTC cross-talk from parallel tests
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
  });

  test('git shell modal should run commands and display output', async ({ page }) => {
    test.slow(); // This test involves git operations that take time
    await navigateToPublicFolder(page);

    // Import Node.js modules
    const fs = await import('fs/promises');
    const path = await import('path');
    const { execSync } = await import('child_process');
    const os = await import('os');

    // Create a real git repo in temp directory
    const tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), 'git-shell-test-'));

    try {
      // Initialize git repo with a file
      execSync('git init', { cwd: tmpDir });
      execSync('git config user.email "test@example.com"', { cwd: tmpDir });
      execSync('git config user.name "Test User"', { cwd: tmpDir });
      await fs.writeFile(path.join(tmpDir, 'README.md'), '# Test Repo');
      execSync('git add .', { cwd: tmpDir });
      execSync('git commit -m "Initial commit"', { cwd: tmpDir });

      // Collect all files including .git
      const allFiles: Array<{ path: string; content: number[] }> = [];
      const allDirs: string[] = [];

      async function collectFiles(dirPath: string, prefix: string = ''): Promise<void> {
        const entries = await fs.readdir(dirPath, { withFileTypes: true });
        for (const entry of entries) {
          const fullPath = path.join(dirPath, entry.name);
          const relativePath = prefix ? `${prefix}/${entry.name}` : entry.name;
          if (entry.isDirectory()) {
            allDirs.push(relativePath);
            await collectFiles(fullPath, relativePath);
          } else {
            const content = await fs.readFile(fullPath);
            allFiles.push({ path: relativePath, content: Array.from(content) });
          }
        }
      }
      await collectFiles(tmpDir);

      // Upload to hashtree
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

        // Create directories first
        const sortedDirs = [...dirs].sort((a, b) => a.split('/').length - b.split('/').length);
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

        // Add to current directory as "shell-test-repo"
        const newRootCid = await tree.setEntry(rootCid, route.path, 'shell-test-repo', repoCid, 0, LinkType.Dir);
        autosaveIfOwn(newRootCid);
      }, { files: allFiles, dirs: allDirs });

      // Navigate to the repo (wait for folder to appear in list)
      const repoLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'shell-test-repo' }).first();
      await expect(repoLink).toBeVisible({ timeout: 15000 });
      await repoLink.click();
      await page.waitForURL(/shell-test-repo/, { timeout: 10000 });

      // Wait for git repo detection
      await expect(page.getByRole('button', { name: /commits/i })).toBeVisible({ timeout: 15000 });

      // Click the Git Shell button
      const shellBtn = page.getByRole('button', { name: /shell|terminal/i });
      await expect(shellBtn).toBeVisible({ timeout: 10000 });
      await shellBtn.click();

      // Modal should open
      const modal = page.locator('[data-testid="git-shell-modal"]');
      await expect(modal).toBeVisible({ timeout: 5000 });

      // Type a git command
      const input = modal.locator('input[type="text"]');
      await input.fill('status');
      await input.press('Enter');

      // Should see output containing "On branch"
      await expect(modal.locator('text=/On branch/')).toBeVisible({ timeout: 10000 });

      // Run another command - git log (plain format)
      await input.fill('log');
      await input.press('Enter');

      // Should see the commit
      await expect(modal.locator('text=/Initial commit/')).toBeVisible({ timeout: 10000 });

      // Close modal
      await page.keyboard.press('Escape');
      await expect(modal).not.toBeVisible({ timeout: 5000 });

    } finally {
      await fs.rm(tmpDir, { recursive: true, force: true });
    }
  });

  test('git shell should support write commands like add and commit', async ({ page }) => {
    test.slow(); // This test involves git operations that take time
    await navigateToPublicFolder(page);

    // Import Node.js modules
    const fs = await import('fs/promises');
    const path = await import('path');
    const { execSync } = await import('child_process');
    const os = await import('os');

    // Create a git repo with one file
    const tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), 'git-commit-test-'));

    try {
      // Initialize git repo with a file
      execSync('git init', { cwd: tmpDir });
      execSync('git config user.email "test@example.com"', { cwd: tmpDir });
      execSync('git config user.name "Test User"', { cwd: tmpDir });
      await fs.writeFile(path.join(tmpDir, 'README.md'), '# Test Repo');
      execSync('git add .', { cwd: tmpDir });
      execSync('git commit -m "Initial commit"', { cwd: tmpDir });

      // Collect all files including .git
      const allFiles: Array<{ path: string; content: number[] }> = [];
      const allDirs: string[] = [];

      async function collectFiles(dirPath: string, prefix: string = ''): Promise<void> {
        const entries = await fs.readdir(dirPath, { withFileTypes: true });
        for (const entry of entries) {
          const fullPath = path.join(dirPath, entry.name);
          const relativePath = prefix ? `${prefix}/${entry.name}` : entry.name;
          if (entry.isDirectory()) {
            allDirs.push(relativePath);
            await collectFiles(fullPath, relativePath);
          } else {
            const content = await fs.readFile(fullPath);
            allFiles.push({ path: relativePath, content: Array.from(content) });
          }
        }
      }
      await collectFiles(tmpDir);

      // Upload to hashtree
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

        // Create directories first
        const sortedDirs = [...dirs].sort((a, b) => a.split('/').length - b.split('/').length);
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

        // Add to current directory as "commit-test-repo"
        const newRootCid = await tree.setEntry(rootCid, route.path, 'commit-test-repo', repoCid, 0, LinkType.Dir);
        autosaveIfOwn(newRootCid);
      }, { files: allFiles, dirs: allDirs });

      // Navigate to the repo (wait for folder to appear in list)
      const repoLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'commit-test-repo' }).first();
      await expect(repoLink).toBeVisible({ timeout: 15000 });
      await repoLink.click();
      await page.waitForURL(/commit-test-repo/, { timeout: 10000 });

      // Wait for git repo detection and 1 commit to show
      await expect(page.getByRole('button', { name: /1 commits/i })).toBeVisible({ timeout: 15000 });

      // Click the Git Shell button
      const shellBtn = page.getByRole('button', { name: /shell|terminal/i });
      await expect(shellBtn).toBeVisible({ timeout: 10000 });
      await shellBtn.click();

      // Modal should open with write commands allowed
      const modal = page.locator('[data-testid="git-shell-modal"]');
      await expect(modal).toBeVisible({ timeout: 5000 });
      await expect(modal.locator('text=/Write commands.*are supported/')).toBeVisible({ timeout: 5000 });

      // First, add a new file via page.evaluate (simulate editing in hashtree)
      await page.evaluate(async () => {
        const { getTree, LinkType } = await import('/src/store.ts');
        const { autosaveIfOwn } = await import('/src/nostr.ts');
        const { getCurrentRootCid } = await import('/src/actions/route.ts');
        const { getRouteSync } = await import('/src/stores/index.ts');

        const tree = getTree();
        const route = getRouteSync();
        const rootCid = getCurrentRootCid();
        if (!rootCid) return;

        // Add a new file to the repo
        const newFileContent = new TextEncoder().encode('New file content');
        const { cid: fileCid, size } = await tree.putFile(newFileContent);

        // Get current repo path and add file to it
        const newRootCid = await tree.setEntry(rootCid, route.path, 'newfile.txt', fileCid, size, LinkType.Blob);
        autosaveIfOwn(newRootCid);
      });

      // Wait for the file to appear in the list
      await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'newfile.txt' })).toBeVisible({ timeout: 10000 });

      // Close and reopen modal to get fresh dirCid
      await page.keyboard.press('Escape');
      await expect(modal).not.toBeVisible({ timeout: 5000 });
      await shellBtn.click();
      await expect(modal).toBeVisible({ timeout: 5000 });

      // Run git add to stage the new file
      const input = modal.locator('input[type="text"]');

      // Count pre elements before add
      const preCountBeforeAdd = await modal.locator('pre').count();

      await input.fill('add newfile.txt');
      await input.press('Enter');

      // Wait for add output to appear (a new pre element)
      await expect(modal.locator('pre')).toHaveCount(preCountBeforeAdd + 1, { timeout: 15000 });

      // Count pre elements before commit
      const preCountBeforeCommit = await modal.locator('pre').count();

      // Run git commit
      await input.fill('commit -m "Add newfile.txt"');
      await input.press('Enter');

      // Wait for commit output to appear (a new pre element)
      await expect(modal.locator('pre')).toHaveCount(preCountBeforeCommit + 1, { timeout: 30000 });

      // Close modal and verify commit count increased
      await page.keyboard.press('Escape');
      await expect(modal).not.toBeVisible({ timeout: 5000 });

      // Wait for UI to show 2 commits (reactive update from the commit)
      await expect(page.getByRole('button', { name: /2 commits/i })).toBeVisible({ timeout: 15000 });

    } finally {
      await fs.rm(tmpDir, { recursive: true, force: true });
    }
  });

});
