import { test, expect } from '@playwright/test';

test.describe('Git file bar', () => {
  test('shows commit info when viewing a file in git repo', async ({ page }) => {
    test.slow();

    // Navigate to a file in the hashtree repo
    await page.goto('/#/npub1xndmdgymsf4a34rzr7346vp8qcptxf75pjqweh8naa8rklgxpfqqmfjtce/hashtree/README.md');

    // Wait for git file bar to appear
    const gitBar = page.locator('[data-testid="git-file-bar"]');
    await expect(gitBar).toBeVisible({ timeout: 60000 });

    // Should have a link/button to view history
    await expect(gitBar.locator('button, a').filter({ hasText: /history/i })).toBeVisible();

    // Should show relative time (e.g., "2 days ago", "3 months ago")
    await expect(gitBar.getByText(/\d+\s+(second|minute|hour|day|week|month|year)s?\s+ago/i)).toBeVisible();
  });

  test('clicking history opens git history modal', async ({ page }) => {
    test.slow();

    await page.goto('/#/npub1xndmdgymsf4a34rzr7346vp8qcptxf75pjqweh8naa8rklgxpfqqmfjtce/hashtree/README.md');

    // Wait for git bar
    const gitBar = page.locator('[data-testid="git-file-bar"]');
    await expect(gitBar).toBeVisible({ timeout: 60000 });

    // Click history button
    await gitBar.locator('button, a').filter({ hasText: /history/i }).click();

    // Should open history modal
    await expect(page.locator('[data-testid="git-history-modal"]')).toBeVisible({ timeout: 10000 });
  });
});
