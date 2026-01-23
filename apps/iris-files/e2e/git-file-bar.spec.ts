import { test, expect } from '@playwright/test';

test.describe('Git file bar', () => {
  test('shows commit info when viewing a file in git repo', async ({ page }) => {
    test.slow();

    // Navigate to a file in the hashtree repo
    await page.goto('/#/npub1xndmdgymsf4a34rzr7346vp8qcptxf75pjqweh8naa8rklgxpfqqmfjtce/hashtree/README.md');

    // Wait for git file bar to appear
    const gitBar = page.locator('[data-testid="git-file-bar"]');
    await expect(gitBar).toBeVisible({ timeout: 60000 });

    // Should have a history button (icon only)
    await expect(gitBar.locator('button[title*="history" i]')).toBeVisible();

    // Should show relative time (e.g., "2 days ago", "3 months ago")
    await expect(gitBar.getByText(/\d+\s+(second|minute|hour|day|week|month|year)s?\s+ago/i)).toBeVisible();
  });

  test('clicking history opens git history modal', async ({ page }) => {
    test.slow();

    await page.goto('/#/npub1xndmdgymsf4a34rzr7346vp8qcptxf75pjqweh8naa8rklgxpfqqmfjtce/hashtree/README.md');

    // Wait for git bar
    const gitBar = page.locator('[data-testid="git-file-bar"]');
    await expect(gitBar).toBeVisible({ timeout: 60000 });

    // Click history button (icon only)
    await gitBar.locator('button[title*="history" i]').click();

    // Should open history modal
    await expect(page.locator('[data-testid="git-history-modal"]')).toBeVisible({ timeout: 10000 });
  });

  test('shows git bar when viewing file in subdirectory via navigation', async ({ page }) => {
    test.slow();

    // Navigate to the repo root first
    await page.goto('/#/npub1xndmdgymsf4a34rzr7346vp8qcptxf75pjqweh8naa8rklgxpfqqmfjtce/hashtree');

    // Wait for directory listing
    await expect(page.locator('[data-testid="directory-listing"]')).toBeVisible({ timeout: 30000 });

    // Click on 'apps' directory
    await page.click('text=apps');

    // Wait for subdirectory listing
    await expect(page.locator('[data-testid="directory-listing"]')).toBeVisible({ timeout: 30000 });

    // Click on 'iris-files' directory
    await page.click('text=iris-files');

    // Wait for subdirectory listing
    await expect(page.locator('[data-testid="directory-listing"]')).toBeVisible({ timeout: 30000 });

    // Click on package.json file
    await page.click('text=package.json');

    // Git bar should appear (with ?g= parameter propagated)
    const gitBar = page.locator('[data-testid="git-file-bar"]');
    await expect(gitBar).toBeVisible({ timeout: 60000 });

    // Should show relative time
    await expect(gitBar.getByText(/\d+\s+(second|minute|hour|day|week|month|year)s?\s+ago/i)).toBeVisible();
  });
});
