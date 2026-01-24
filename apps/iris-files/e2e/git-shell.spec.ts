import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool } from './test-utils.js';

async function assertCodeDropdown(page: any, repoName: string): Promise<void> {
  const codeBtn = page.locator('.code-dropdown > button');
  await expect(codeBtn).toBeVisible({ timeout: 15000 });
  await codeBtn.click();

  const url = page.url();
  const npubMatch = url.match(/npub1[0-9a-z]+/);
  expect(npubMatch).toBeTruthy();
  const expectedClone = `git clone htree://${npubMatch![0]}/public/${repoName}`;

  const inputs = page.locator('.code-dropdown input[type="text"]');
  await expect(inputs).toHaveCount(3, { timeout: 5000 });
  await expect(inputs.nth(0)).toHaveValue(expectedClone);
  await expect(inputs.nth(1)).toHaveValue(/https:\/\/sh\.rustup\.rs/);
  await expect(inputs.nth(2)).toHaveValue(/cargo install git-remote-htree/);
}

test.describe('Git code dropdown', () => {
  // Disable "others pool" to prevent WebRTC cross-talk from parallel tests
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
  });

  test('git code dropdown should show clone instructions', async ({ page }) => {
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

      await assertCodeDropdown(page, 'shell-test-repo');

    } finally {
      await fs.rm(tmpDir, { recursive: true, force: true });
    }
  });

  test('git code dropdown should include setup commands', async ({ page }) => {
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

      // Wait for git repo detection
      await expect(page.getByRole('button', { name: /commits/i })).toBeVisible({ timeout: 15000 });

      await assertCodeDropdown(page, 'commit-test-repo');

    } finally {
      await fs.rm(tmpDir, { recursive: true, force: true });
    }
  });

});
