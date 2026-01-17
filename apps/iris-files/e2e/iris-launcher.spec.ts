import { test, expect } from '@playwright/test';

test.describe('Iris App Launcher', () => {
  test.beforeEach(async ({ page }) => {
    // Navigate to Iris app and clear localStorage
    await page.goto('/iris.html');
    await page.evaluate(() => {
      localStorage.removeItem('iris:apps');
    });
    await page.reload();
  });

  test('should display toolbar and app launcher on homepage', async ({ page }) => {
    await page.goto('/iris.html');

    // Check address bar is visible
    const addressBar = page.getByPlaceholder('Search or enter address');
    await expect(addressBar).toBeVisible({ timeout: 10000 });

    // Check "Favourites" section heading is visible
    const favoritesHeading = page.locator('h2:has-text("Favourites")');
    await expect(favoritesHeading).toBeVisible({ timeout: 5000 });

    // Check "Suggestions" section heading is visible
    const suggestionsHeading = page.locator('h2:has-text("Suggestions")');
    await expect(suggestionsHeading).toBeVisible({ timeout: 5000 });
  });

  test('should show suggested apps', async ({ page }) => {
    await page.goto('/iris.html');

    // Check that default suggestions are visible
    const irisFiles = page.locator('button:has-text("Iris Files")');
    await expect(irisFiles).toBeVisible({ timeout: 5000 });

    const irisVideo = page.locator('button:has-text("Iris Video")');
    await expect(irisVideo).toBeVisible({ timeout: 5000 });

    const irisSocial = page.locator('button:has-text("Iris Social")');
    await expect(irisSocial).toBeVisible({ timeout: 5000 });
  });

  test('should add app to favorites from suggestions', async ({ page }) => {
    await page.goto('/iris.html');

    // Find the add button (has title="Add to favourites") for first suggestion
    const addButton = page.locator('button[title="Add to favourites"]').first();
    await addButton.click();

    // Verify localStorage was updated
    const apps = await page.evaluate(() => {
      const stored = localStorage.getItem('iris:apps');
      return stored ? JSON.parse(stored) : [];
    });
    expect(apps.length).toBeGreaterThan(0);
    expect(apps[0].name).toBe('Iris Files');
  });

  test('should remove app from favorites', async ({ page }) => {
    // Pre-populate localStorage with a test app
    await page.goto('/iris.html');
    await page.evaluate(() => {
      const apps = [{
        url: 'https://example.com/remove-test',
        name: 'Test App',
        addedAt: Date.now()
      }];
      localStorage.setItem('iris:apps', JSON.stringify(apps));
    });
    await page.reload();

    // Verify app is visible in favorites
    const appCard = page.locator('button:has-text("Test App")');
    await expect(appCard).toBeVisible({ timeout: 5000 });

    // Hover over the app container to show remove button
    const appContainer = appCard.locator('..'); // parent div.group
    await appContainer.hover();

    // Click remove button (has title="Remove")
    const removeButton = page.locator('button[title="Remove"]').first();
    await removeButton.click();

    // Verify app is removed
    await expect(appCard).not.toBeVisible({ timeout: 5000 });

    // Verify localStorage was updated
    const apps = await page.evaluate(() => {
      const stored = localStorage.getItem('iris:apps');
      return stored ? JSON.parse(stored) : [];
    });
    expect(apps).toHaveLength(0);
  });

  test('should navigate to app when clicking favorite', async ({ page }) => {
    // Pre-populate localStorage with a test app
    await page.goto('/iris.html');
    await page.evaluate(() => {
      const apps = [{
        url: 'https://example.com/click-test',
        name: 'Click Test App',
        addedAt: Date.now()
      }];
      localStorage.setItem('iris:apps', JSON.stringify(apps));
    });
    await page.reload();

    // Click the app card
    const appCard = page.locator('button:has-text("Click Test App")');
    await appCard.click();

    // Verify URL changed to app frame route
    await expect(page).toHaveURL(/\/app\//);
  });

  test('should navigate to app via address bar', async ({ page }) => {
    await page.goto('/iris.html');

    // Type URL in address bar
    const addressBar = page.getByPlaceholder('Search or enter address');
    await addressBar.fill('https://example.com/address-test');
    await addressBar.press('Enter');

    // Verify URL changed to app frame route
    await expect(page).toHaveURL(/\/app\//);
  });

  test('should show back button when viewing app', async ({ page }) => {
    const testAppUrl = encodeURIComponent('https://example.com/back-test');
    await page.goto(`/iris.html#/app/${testAppUrl}`);

    // Back button should be enabled (not disabled)
    const backButton = page.locator('button[title="Back"]');
    await expect(backButton).toBeVisible({ timeout: 5000 });

    // Wait a moment for history to update
    await page.waitForTimeout(500);
  });

  test('should persist favorites across page reloads', async ({ page }) => {
    // Pre-add an app to localStorage
    await page.evaluate(() => {
      const apps = [{
        url: 'https://example.com/persist-test',
        name: 'Persist Test',
        addedAt: Date.now()
      }];
      localStorage.setItem('iris:apps', JSON.stringify(apps));
    });

    // Navigate fresh
    await page.goto('/iris.html');

    // Wait for the Favourites heading to appear
    const favoritesHeading = page.locator('h2:has-text("Favourites")');
    await expect(favoritesHeading).toBeVisible({ timeout: 10000 });

    // App should be visible from localStorage
    const appCard = page.locator('button:has-text("Persist Test")');
    await expect(appCard).toBeVisible({ timeout: 5000 });
  });
});
