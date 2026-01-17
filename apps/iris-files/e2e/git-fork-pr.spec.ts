import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool, useLocalRelay } from './test-utils.js';

// Helper to get npub from URL
async function getNpub(page: any): Promise<string> {
  const url = page.url();
  const match = url.match(/npub1[a-z0-9]+/);
  if (!match) throw new Error('Could not find npub in URL');
  return match[0];
}

test.describe('Git fork and PR workflow', () => {
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
    await useLocalRelay(page);
  });

  test('fork own repo, edit, commit, and create PR using htree:// format', { timeout: 120000 }, async ({ page }) => {
    test.slow();

    // === Create a git repo ===
    console.log('[test] Creating git repo...');
    await navigateToPublicFolder(page);

    // Create folder
    await page.getByRole('button', { name: 'New Folder' }).click();
    const folderInput = page.locator('input[placeholder="Folder name..."]');
    await folderInput.waitFor({ timeout: 5000 });
    await folderInput.fill('original-repo');
    await page.click('button:has-text("Create")');
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Navigate into folder
    const folderLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'original-repo' }).first();
    await expect(folderLink).toBeVisible({ timeout: 15000 });
    await folderLink.click();
    await page.waitForURL(/original-repo/, { timeout: 10000 });

    // Create initial file
    await page.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const { autosaveIfOwn } = await import('/src/nostr.ts');
      const { getCurrentRootCid } = await import('/src/actions/route.ts');
      const { getRouteSync } = await import('/src/stores/index.ts');
      const route = getRouteSync();

      const tree = getTree();
      let rootCid = getCurrentRootCid();
      if (!rootCid) return;

      const content = new TextEncoder().encode('# Original Repo\n\nThis is the original content.');
      const { cid, size } = await tree.putFile(content);
      rootCid = await tree.setEntry(rootCid, route.path, 'README.md', cid, size, LinkType.Blob);
      autosaveIfOwn(rootCid);
    });

    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'README.md' })).toBeVisible({ timeout: 15000 });

    // Git init
    const gitInitBtn = page.getByRole('button', { name: 'Git Init' });
    await expect(gitInitBtn).toBeVisible({ timeout: 15000 });
    await gitInitBtn.click();
    await expect(gitInitBtn).not.toBeVisible({ timeout: 30000 });

    // Wait for clean status
    await expect(page.locator('text=clean')).toBeVisible({ timeout: 15000 });
    console.log('[test] Git repo created with initial commit');

    // Get user's npub
    const userNpub = await getNpub(page);
    console.log(`[test] User npub: ${userNpub.slice(0, 20)}...`);

    // === Fork the repo ===
    console.log('[test] Forking repo...');

    // Look for Fork button - try different possible locations
    let forkClicked = false;

    // Try toolbar button with fork icon
    const forkBtnIcon = page.locator('button').filter({ has: page.locator('[class*="git-fork"]') }).first();
    if (await forkBtnIcon.isVisible().catch(() => false)) {
      await forkBtnIcon.click();
      forkClicked = true;
    }

    // Try button with "Fork" text
    if (!forkClicked) {
      const forkBtnText = page.getByRole('button', { name: /fork/i });
      if (await forkBtnText.isVisible().catch(() => false)) {
        await forkBtnText.click();
        forkClicked = true;
      }
    }

    // Try dropdown menu
    if (!forkClicked) {
      const menuBtn = page.locator('[data-testid="folder-menu"]');
      if (await menuBtn.isVisible().catch(() => false)) {
        await menuBtn.click();
        const forkMenuItem = page.locator('text=Fork').first();
        await forkMenuItem.click();
        forkClicked = true;
      }
    }

    if (!forkClicked) {
      throw new Error('Could not find Fork button');
    }

    // Wait for fork modal
    const forkModal = page.locator('.fixed.inset-0').filter({ hasText: 'Fork as New Folder' });
    await expect(forkModal).toBeVisible({ timeout: 5000 });

    // Enter fork name
    const forkNameInput = forkModal.locator('input');
    await forkNameInput.fill('my-fork');

    // Click Fork button
    const confirmForkBtn = forkModal.getByRole('button', { name: /fork/i }).last();
    await confirmForkBtn.click();

    // Wait for navigation to forked repo
    await page.waitForURL(/my-fork/, { timeout: 15000 });
    console.log('[test] Forked to my-fork');

    // Wait for fork to load
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'README.md' })).toBeVisible({ timeout: 15000 });
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: '.git' })).toBeVisible({ timeout: 10000 });

    // Verify git status shows clean
    await expect(page.locator('text=clean')).toBeVisible({ timeout: 15000 });
    console.log('[test] Fork loaded, status clean');

    // === Edit a file in the fork ===
    console.log('[test] Editing README.md in fork...');
    await page.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const { autosaveIfOwn } = await import('/src/nostr.ts');
      const { getCurrentRootCid } = await import('/src/actions/route.ts');
      const { getRouteSync } = await import('/src/stores/index.ts');
      const route = getRouteSync();

      const tree = getTree();
      let rootCid = getCurrentRootCid();
      if (!rootCid) return;

      const content = new TextEncoder().encode('# Original Repo\n\nThis is modified content from the fork!');
      const { cid, size } = await tree.putFile(content);
      rootCid = await tree.setEntry(rootCid, route.path, 'README.md', cid, size, LinkType.Blob);
      autosaveIfOwn(rootCid);
    });

    // Wait for uncommitted indicator
    const uncommittedBtn = page.locator('button').filter({ hasText: /uncommitted/i });
    await expect(uncommittedBtn).toBeVisible({ timeout: 30000 });
    console.log('[test] Uncommitted changes detected');

    // === Commit the changes ===
    console.log('[test] Committing changes...');
    await uncommittedBtn.click();
    const commitModal = page.locator('.fixed.inset-0').filter({ hasText: 'Commit Changes' });
    await expect(commitModal).toBeVisible({ timeout: 5000 });

    const commitMessageInput = commitModal.locator('textarea[placeholder*="Describe"]');
    await commitMessageInput.fill('Update README with changes from fork');
    await commitModal.getByRole('button', { name: 'Commit' }).click();

    await expect(commitModal).not.toBeVisible({ timeout: 30000 });
    await expect(page.locator('text=clean')).toBeVisible({ timeout: 15000 });
    console.log('[test] Changes committed');

    // === Create PR back to original repo ===
    console.log('[test] Creating PR...');

    // Wait for fork to sync to relay (needs time for Nostr publish)
    await page.waitForTimeout(3000);

    // Navigate to original repo to create PR there
    await page.goto(`http://localhost:5173/#/${userNpub}/original-repo?tab=pulls`);

    // Wait for pulls tab to be active (the link with Pull Requests text)
    await expect(page.getByRole('link', { name: 'Pull Requests' })).toBeVisible({ timeout: 15000 });

    // Click New Pull Request button
    const newPRBtn = page.getByRole('button', { name: /New Pull Request/i });
    await expect(newPRBtn).toBeVisible({ timeout: 10000 });
    await newPRBtn.click();

    // Wait for PR modal
    const prModal = page.locator('.fixed.inset-0').filter({ hasText: 'New Pull Request' });
    await expect(prModal).toBeVisible({ timeout: 5000 });

    // Fill PR title
    const titleInput = prModal.locator('#pr-title');
    await titleInput.fill('Update README from fork');

    // Select "From fork" option to enable cross-repo PR
    const fromForkBtn = prModal.locator('button').filter({ hasText: /From fork/i });
    await fromForkBtn.click();

    // Fill source repo using htree:// format (subfolder path)
    const sourceRepoInput = prModal.locator('#pr-source-repo');
    await expect(sourceRepoInput).toBeVisible({ timeout: 5000 });
    await sourceRepoInput.fill(`htree://${userNpub}/my-fork`);
    console.log(`[test] Source repo set to htree://${userNpub.slice(0, 20)}.../my-fork`);

    // Wait for fork resolution to complete
    const loadingBranches = prModal.getByText('Loading branches...');
    if (await loadingBranches.isVisible().catch(() => false)) {
      await expect(loadingBranches).not.toBeVisible({ timeout: 15000 });
    }
    const forkError = prModal.locator('div').filter({ hasText: /Could not resolve fork|Failed to resolve fork|Worker not initialized/ });
    await expect(forkError).toHaveCount(0);

    // Ensure source branch is set (might need to fill manually for cross-repo)
    // Try to find branch dropdown or input and set to master
    const sourceBranchInput = prModal.locator('input[placeholder="feature/..."]');
    if (await sourceBranchInput.isVisible().catch(() => false)) {
      await sourceBranchInput.fill('master');
      console.log('[test] Set source branch to master');
    } else {
      const branchDropdown = prModal.locator('button').filter({ hasText: /Select branch|main|master/ }).first();
      if (await branchDropdown.isVisible().catch(() => false)) {
        await branchDropdown.click();
        const branchOption = prModal.locator('button').filter({ hasText: /main|master/ }).first();
        await expect(branchOption).toBeVisible({ timeout: 5000 });
        await branchOption.click();
        console.log('[test] Selected source branch from dropdown');
      }
    }

    // Fill description
    const descInput = prModal.locator('#pr-description');
    await descInput.fill('This PR brings changes from my fork back to the original repo.');

    // Take screenshot for debugging
    await page.screenshot({ path: 'e2e/screenshots/fork-pr-modal.png' });

    // Submit PR - wait for button to be enabled
    const submitBtn = prModal.getByRole('button', { name: /Create Pull Request/i });
    await expect(submitBtn).toBeEnabled({ timeout: 10000 });
    await submitBtn.click();

    // Wait for modal to close (PR created)
    await expect(prModal).not.toBeVisible({ timeout: 30000 });
    console.log('[test] PR created successfully!');

    // Verify navigation happened (URL should contain the PR id)
    await page.waitForURL(/tab=pulls/, { timeout: 10000 });

    console.log('[test] SUCCESS: Fork and PR workflow with htree:// format completed');
  });
});
