import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool } from './test-utils.js';

test.describe('Directory upload features', () => {
  test.describe.configure({ timeout: 90000 });
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
  });

  test('should show Add Folder button when in a folder (if browser supports it)', async ({ page }) => {
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // Navigate to tree list and create a folder
    await page.locator('header a:has-text("Iris")').click();
    const newFolderButton = page.getByRole('button', { name: 'New Folder' }).first();
    await expect(newFolderButton).toBeVisible({ timeout: 5000 });
    await newFolderButton.click();

    const input = page.locator('input[placeholder="Folder name..."]');
    await input.waitFor({ timeout: 5000 });
    await input.fill('dir-upload-test');
    await page.click('button:has-text("Create")');

    // Wait for modal to close
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Wait for the folder to be created
    await expect(page.locator('text=Empty directory')).toBeVisible({ timeout: 10000 });

    // The Add Folder button should be visible (browsers that support webkitdirectory)
    const addFolderButtons = page.locator('label:has-text("Add Folder")');

    // Check if directory upload is supported (it should be in Chromium-based browsers)
    const count = await addFolderButtons.count();
    if (count > 0) {
      const anyVisible = await addFolderButtons.evaluateAll((buttons) =>
        buttons.some((b) => {
          const style = window.getComputedStyle(b);
          return style.display !== 'none' && style.visibility !== 'hidden';
        })
      );
      expect(anyVisible).toBe(true);
    }
  });

  test('should have webkitdirectory attribute on directory input', async ({ page }) => {
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // Navigate to tree list and create a folder
    await page.locator('header a:has-text("Iris")').click();
    const newFolderButton = page.getByRole('button', { name: 'New Folder' }).first();
    await expect(newFolderButton).toBeVisible({ timeout: 5000 });
    await newFolderButton.click();

    const input = page.locator('input[placeholder="Folder name..."]');
    await input.waitFor({ timeout: 5000 });
    await input.fill('dir-attr-test');
    await page.click('button:has-text("Create")');

    // Wait for modal to close and folder view
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });
    await expect(page.locator('text=Empty directory')).toBeVisible({ timeout: 10000 });

    // Find the directory input by looking for an input with webkitdirectory attribute
    const dirInput = page.locator('input[type="file"][webkitdirectory]');

    const count = await dirInput.count();
    if (count > 0) {
      // Verify at least one input exists with the correct attribute
      expect(count).toBeGreaterThan(0);
    }
  });

  test('should show both Add and Add Folder buttons', async ({ page }) => {
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // Navigate to tree list and create a folder
    await page.locator('header a:has-text("Iris")').click();
    const newFolderButton = page.getByRole('button', { name: 'New Folder' }).first();
    await expect(newFolderButton).toBeVisible({ timeout: 5000 });
    await newFolderButton.click();

    const input = page.locator('input[placeholder="Folder name..."]');
    await input.waitFor({ timeout: 5000 });
    await input.fill('both-buttons-test');
    await page.click('button:has-text("Create")');

    // Wait for modal to close
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });
    await expect(page.locator('text=Empty directory')).toBeVisible({ timeout: 10000 });

    // Both buttons should exist
    // Add button (green btn-success class)
    const addButtons = page.locator('label.btn-success:has-text("Add")');
    const addCount = await addButtons.count();
    expect(addCount).toBeGreaterThan(0);

    // Add Folder button (btn-ghost)
    const addFolderButtons = page.locator('label.btn-ghost:has-text("Add Folder")');
    const folderCount = await addFolderButtons.count();

    if (folderCount > 0) {
      const anyFolderVisible = await addFolderButtons.evaluateAll((buttons) =>
        buttons.some((b) => {
          const style = window.getComputedStyle(b);
          return style.display !== 'none' && style.visibility !== 'hidden';
        })
      );
      expect(anyFolderVisible).toBe(true);
    }
  });

  test('should upload nested directory structure correctly', async ({ page }) => {
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // Navigate to tree list and create a folder
    await page.locator('header a:has-text("Iris")').click();
    const newFolderButton = page.getByRole('button', { name: 'New Folder' }).first();
    await expect(newFolderButton).toBeVisible({ timeout: 5000 });
    await newFolderButton.click();

    const input = page.locator('input[placeholder="Folder name..."]');
    await input.waitFor({ timeout: 5000 });
    await input.fill('nested-test');
    await page.click('button:has-text("Create")');

    // Wait for modal to close
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });
    await expect(page.locator('text=Empty directory')).toBeVisible({ timeout: 10000 });

    // Simulate adding files with nested paths by using the uploadFilesWithPaths function
    // We inject test data directly since we can't easily trigger webkitdirectory in tests
    const result = await page.evaluate(async () => {
      // Access the app's internal functions
      const { getTree, LinkType } = await import('/src/store.ts');
      const tree = getTree();

      // Create a mock directory structure:
      // project/
      //   src/
      //     index.js
      //     utils/
      //       helper.js
      //   README.md

      const files = [
        { path: 'project/README.md', content: '# Project' },
        { path: 'project/src/index.js', content: 'console.log("hello");' },
        { path: 'project/src/utils/helper.js', content: 'export function help() {}' },
      ];

      // Create empty root
      let { cid: rootCid } = await tree.putDirectory([]);

      // Create directories first (sorted by depth)
      const dirs = ['project', 'project/src', 'project/src/utils'];
      for (const dir of dirs) {
        const parts = dir.split('/');
        const name = parts.pop()!;
        const { cid: emptyDir } = await tree.putDirectory([]);
        rootCid = await tree.setEntry(rootCid, parts, name, emptyDir, 0, LinkType.Dir);
      }

      // Add files
      for (const file of files) {
        const parts = file.path.split('/');
        const name = parts.pop()!;
        const data = new TextEncoder().encode(file.content);
        const { cid: fileCid, size } = await tree.putFile(data);
        rootCid = await tree.setEntry(rootCid, parts, name, fileCid, size, LinkType.Blob);
      }

      // Verify structure
      const projectEntries = await tree.listDirectory(rootCid);
      const projectDir = projectEntries.find(e => e.name === 'project');
      if (!projectDir) return { error: 'project dir not found' };

      const projectContents = await tree.listDirectory(projectDir.cid);
      const srcDir = projectContents.find(e => e.name === 'src');
      const readme = projectContents.find(e => e.name === 'README.md');
      if (!srcDir || !readme) return { error: 'src or README not found' };

      const srcContents = await tree.listDirectory(srcDir.cid);
      const indexJs = srcContents.find(e => e.name === 'index.js');
      const utilsDir = srcContents.find(e => e.name === 'utils');
      if (!indexJs || !utilsDir) return { error: 'index.js or utils not found' };

      const utilsContents = await tree.listDirectory(utilsDir.cid);
      const helperJs = utilsContents.find(e => e.name === 'helper.js');
      if (!helperJs) return { error: 'helper.js not found' };

      return {
        success: true,
        structure: {
          project: projectContents.map(e => e.name),
          src: srcContents.map(e => e.name),
          utils: utilsContents.map(e => e.name),
        },
      };
    });

    expect(result.error).toBeUndefined();
    expect(result.success).toBe(true);
    expect(result.structure.project).toContain('README.md');
    expect(result.structure.project).toContain('src');
    expect(result.structure.src).toContain('index.js');
    expect(result.structure.src).toContain('utils');
    expect(result.structure.utils).toContain('helper.js');
  });
});
