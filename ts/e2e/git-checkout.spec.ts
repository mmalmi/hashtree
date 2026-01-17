import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool } from './test-utils.js';

test.describe('Git checkout features', () => {
  // Disable "others pool" to prevent WebRTC cross-talk from parallel tests
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
  });

  test('checkout commit should return a valid directory CID that can be listed', { timeout: 60000 }, async ({ page }) => {
    test.slow();

    // Capture wasm-git logs
    const wasmGitLogs: string[] = [];
    page.on('console', msg => {
      const text = msg.text();
      if (text.includes('[wasm-git]')) {
        wasmGitLogs.push(text);
      }
    });

    await navigateToPublicFolder(page);

    // Import Node.js modules
    const fs = await import('fs/promises');
    const path = await import('path');
    const { execSync } = await import('child_process');
    const os = await import('os');

    // Create a git repo with two commits
    const tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), 'git-checkout-cid-test-'));

    try {
      // Initialize git repo
      execSync('git init', { cwd: tmpDir });
      execSync('git config user.email "test@example.com"', { cwd: tmpDir });
      execSync('git config user.name "Test User"', { cwd: tmpDir });

      // First commit
      await fs.writeFile(path.join(tmpDir, 'file.txt'), 'Version 1\n');
      execSync('git add .', { cwd: tmpDir });
      execSync('git commit -m "Initial commit"', { cwd: tmpDir });

      // Get first commit SHA
      const firstCommit = execSync('git rev-parse HEAD', { cwd: tmpDir }).toString().trim();

      // Second commit
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

      // Upload and test checkoutCommit returns a listable directory
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

        // Test checkoutCommit
        const { checkoutCommit } = await import('/src/utils/git.ts');

        try {
          const newRootCid = await checkoutCommit(rootCid, commitSha);

          // CRITICAL: Verify the returned CID is a valid directory that can be listed
          let canListDirectory = false;
          let entries: string[] = [];
          let listError: string | null = null;

          try {
            const dirEntries = await tree.listDirectory(newRootCid);
            canListDirectory = true;
            entries = dirEntries.map(e => `${e.name}${e.type === LinkType.Dir ? '/' : ''}`);
          } catch (err) {
            canListDirectory = false;
            listError = err instanceof Error ? err.message : String(err);
          }

          // Also verify the entries are correct (should have file.txt and .git, but NOT file2.txt)
          const hasFileTxt = entries.includes('file.txt');
          const hasFile2Txt = entries.includes('file2.txt');
          const hasGitDir = entries.includes('.git/');

          return {
            success: true,
            canListDirectory,
            listError,
            entries,
            hasFileTxt,
            hasFile2Txt,
            hasGitDir,
            error: null
          };
        } catch (err) {
          return {
            success: false,
            canListDirectory: false,
            listError: null,
            entries: [],
            hasFileTxt: false,
            hasFile2Txt: false,
            hasGitDir: false,
            error: err instanceof Error ? err.message : String(err)
          };
        }
      }, { files: allFiles, dirs: allDirs, commitSha: firstCommit });

      console.log('Checkout CID test result:', JSON.stringify(result, null, 2));
      console.log('Wasm-git logs:', wasmGitLogs);
      expect(result.success).toBe(true);
      expect(result.error).toBeNull();

      // The key assertion: the returned CID MUST be a listable directory
      expect(result.canListDirectory).toBe(true);
      expect(result.listError).toBeNull();

      // Verify correct entries
      expect(result.hasFileTxt).toBe(true);
      expect(result.hasFile2Txt).toBe(false); // file2.txt was added in second commit
      expect(result.hasGitDir).toBe(true);

    } finally {
      await fs.rm(tmpDir, { recursive: true, force: true });
    }
  });

  test('checkout previous revision removes files that were added later', { timeout: 120000 }, async ({ page }) => {
    test.slow();
    await navigateToPublicFolder(page);

    // Create a folder for our test repo
    await page.getByRole('button', { name: 'New Folder' }).click();
    const folderInput = page.locator('input[placeholder="Folder name..."]');
    await folderInput.waitFor({ timeout: 5000 });
    await folderInput.fill('checkout-test');
    await page.click('button:has-text("Create")');
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Navigate into the folder
    const folderLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'checkout-test' }).first();
    await expect(folderLink).toBeVisible({ timeout: 15000 });
    await folderLink.click();
    await page.waitForURL(/checkout-test/, { timeout: 10000 });

    // Create initial file via the tree API
    await page.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const { autosaveIfOwn } = await import('/src/nostr.ts');
      const { getCurrentRootCid } = await import('/src/actions/route.ts');
      const { getRouteSync } = await import('/src/stores/index.ts');
      const route = getRouteSync();

      const tree = getTree();
      let rootCid = getCurrentRootCid();
      if (!rootCid) return;

      // Create initial.txt
      const content = new TextEncoder().encode('Initial content');
      const { cid, size } = await tree.putFile(content);
      rootCid = await tree.setEntry(rootCid, route.path, 'initial.txt', cid, size, LinkType.Blob);

      autosaveIfOwn(rootCid);
    });

    // Wait for file to appear
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'initial.txt' })).toBeVisible({ timeout: 15000 });

    // Initialize git repo (creates first commit with initial.txt)
    const gitInitBtn = page.getByRole('button', { name: 'Git Init' });
    await expect(gitInitBtn).toBeVisible({ timeout: 15000 });
    await gitInitBtn.click();
    await expect(gitInitBtn).not.toBeVisible({ timeout: 30000 });

    // Wait for git features
    const commitsBtn = page.getByRole('button', { name: /commits/i });
    await expect(commitsBtn).toBeVisible({ timeout: 20000 });
    await expect(commitsBtn).toContainText(/1/, { timeout: 20000 });
    const cleanIndicator = page.locator('[title="No uncommitted changes"]');
    await expect(cleanIndicator).toBeVisible({ timeout: 15000 });

    // Add a second file
    await page.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const { autosaveIfOwn } = await import('/src/nostr.ts');
      const { getCurrentRootCid } = await import('/src/actions/route.ts');
      const { getRouteSync } = await import('/src/stores/index.ts');
      const route = getRouteSync();

      const tree = getTree();
      let rootCid = getCurrentRootCid();
      if (!rootCid) return;

      // Create added-later.txt
      const content = new TextEncoder().encode('Added in second commit');
      const { cid, size } = await tree.putFile(content);
      rootCid = await tree.setEntry(rootCid, route.path, 'added-later.txt', cid, size, LinkType.Blob);

      autosaveIfOwn(rootCid);
    });

    // Wait for the new file to appear
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'added-later.txt' })).toBeVisible({ timeout: 15000 });

    // Wait for uncommitted changes indicator and commit
    const uncommittedBtn = page.locator('button').filter({ hasText: /uncommitted/i });
    await expect(cleanIndicator).not.toBeVisible({ timeout: 30000 });
    await expect(uncommittedBtn).toBeVisible({ timeout: 30000 });

    await uncommittedBtn.click();
    const commitModal = page.locator('.fixed.inset-0').filter({ hasText: 'Commit Changes' });
    await expect(commitModal).toBeVisible({ timeout: 5000 });

    const commitMessageInput = commitModal.locator('textarea[placeholder*="Describe"]');
    await commitMessageInput.fill('Add added-later.txt');
    const commitBtn = commitModal.getByRole('button', { name: /Commit/ });
    await commitBtn.click();
    await expect(commitModal).not.toBeVisible({ timeout: 30000 });

    // Verify we now have 2 commits and both files visible
    await expect(commitsBtn).toContainText(/2/, { timeout: 20000 });
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'initial.txt' })).toBeVisible();
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'added-later.txt' })).toBeVisible();

    // Open commit history
    await commitsBtn.click();
    const historyModal = page.locator('.fixed.inset-0').filter({ hasText: 'Commit History' });
    await expect(historyModal).toBeVisible({ timeout: 5000 });

    // Verify HEAD badge is shown on first commit
    await expect(historyModal.locator('text=HEAD')).toBeVisible({ timeout: 5000 });

    // Click checkout on the older commit (non-HEAD, should have Checkout button)
    const checkoutBtns = historyModal.locator('button').filter({ hasText: 'Checkout' });
    await expect(checkoutBtns.first()).toBeVisible({ timeout: 5000 });
    await checkoutBtns.first().click();

    // Wait for history modal to close
    await expect(historyModal).not.toBeVisible({ timeout: 30000 });

    // After checkout to initial commit:
    // - initial.txt should still be visible (was in first commit)
    // - added-later.txt should NOT be visible (was added in second commit)
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'initial.txt' })).toBeVisible({ timeout: 15000 });
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'added-later.txt' })).not.toBeVisible({ timeout: 5000 });

    // After checkout to older commit, git log from HEAD only shows ancestors
    // So we expect 1 commit (the initial commit we checked out)
    const updatedCommitsBtn = page.getByRole('button', { name: /commits/i });
    await expect(updatedCommitsBtn).toContainText(/1/, { timeout: 10000 });
    await updatedCommitsBtn.click();

    const historyModal2 = page.locator('.fixed.inset-0').filter({ hasText: 'Commit History' });
    await expect(historyModal2).toBeVisible({ timeout: 5000 });

    // The "Initial commit" should be visible and marked as HEAD (the green badge, not "Detached HEAD" warning)
    await expect(historyModal2.locator('text=Initial commit')).toBeVisible({ timeout: 5000 });
    await expect(historyModal2.locator('.bg-success\\/20:has-text("HEAD")')).toBeVisible({ timeout: 5000 });

    // The "Add added-later.txt" commit won't be visible because it's not an ancestor of HEAD
    // (This is correct git behavior - git log shows only ancestors of HEAD)

    // The current HEAD commit should show "Current" not a Checkout button
    await expect(historyModal2.locator('text=Current')).toBeVisible({ timeout: 5000 });

    // Close modal
    await page.keyboard.press('Escape');
    await expect(historyModal2).not.toBeVisible({ timeout: 5000 });
  });

});
