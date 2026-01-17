import { test, expect } from '@playwright/test';
import { createServer } from 'http';
import { readFileSync } from 'fs';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

// Serve the test PWA fixture
let server: ReturnType<typeof createServer>;
const PWA_PORT = 9876;
const PWA_URL = `http://localhost:${PWA_PORT}`;

test.beforeAll(async () => {
  const fixtureDir = join(__dirname, 'fixtures/test-pwa');

  server = createServer((req, res) => {
    // Add CORS headers for cross-origin requests
    res.setHeader('Access-Control-Allow-Origin', '*');
    res.setHeader('Access-Control-Allow-Methods', 'GET, OPTIONS');
    res.setHeader('Access-Control-Allow-Headers', 'Content-Type');

    // Handle preflight requests
    if (req.method === 'OPTIONS') {
      res.statusCode = 204;
      res.end();
      return;
    }

    const url = req.url === '/' ? '/index.html' : req.url;
    const filePath = join(fixtureDir, url!);

    try {
      const content = readFileSync(filePath);
      const ext = filePath.split('.').pop();
      const contentTypes: Record<string, string> = {
        'html': 'text/html',
        'css': 'text/css',
        'js': 'application/javascript',
        'json': 'application/json',
        'png': 'image/png',
      };
      res.setHeader('Content-Type', contentTypes[ext!] || 'application/octet-stream');
      res.end(content);
    } catch {
      res.statusCode = 404;
      res.end('Not found');
    }
  });

  await new Promise<void>((resolve) => server.listen(PWA_PORT, resolve));
});

test.afterAll(async () => {
  server?.close();
});

test.describe('PWA Save to Hashtree', () => {
  test('should show star button when loading external URL', async ({ page }) => {
    await page.goto('/iris.html');

    // Wait for app to load
    await expect(page.locator('h2:has-text("Favourites")')).toBeVisible({ timeout: 10000 });

    // Enter the test PWA URL in the address bar
    const addressBar = page.locator('input[placeholder="Search or enter address"]');
    await addressBar.fill(PWA_URL);
    await addressBar.press('Enter');

    // Wait for iframe to load
    const iframe = page.locator('iframe[title="App"]');
    await expect(iframe).toBeVisible({ timeout: 10000 });

    // Star button should be enabled (not disabled) for external URLs
    const starButton = page.locator('button[title="Add bookmark"]');
    await expect(starButton).toBeVisible({ timeout: 5000 });
    await expect(starButton).toBeEnabled();
  });

  test('should save PWA and navigate to nhash URL when clicking star', async ({ page }) => {
    test.slow(); // This test involves network requests

    await page.goto('/iris.html');

    const addressBar = page.locator('input[placeholder="Search or enter address"]');
    await addressBar.fill(PWA_URL);
    await addressBar.press('Enter');

    // Wait for iframe
    const iframe = page.locator('iframe[title="App"]');
    await expect(iframe).toBeVisible({ timeout: 10000 });

    // Click star button to bookmark
    const starButton = page.locator('button[title="Add bookmark"]');
    await starButton.click();

    // Wait for save to complete and navigate to nhash
    // The address bar should now show an nhash URL
    await expect(addressBar).toHaveValue(/nhash/, { timeout: 30000 });
  });

  test('should add saved PWA to favorites', async ({ page }) => {
    test.slow();

    await page.goto('/iris.html');

    const addressBar = page.locator('input[placeholder="Search or enter address"]');
    await addressBar.fill(PWA_URL);
    await addressBar.press('Enter');

    // Wait for iframe
    const iframe = page.locator('iframe[title="App"]');
    await expect(iframe).toBeVisible({ timeout: 10000 });

    // Click star to bookmark
    const starButton = page.locator('button[title="Add bookmark"]');
    await starButton.click();

    // Wait for save to complete
    await expect(addressBar).toHaveValue(/nhash/, { timeout: 30000 });

    // Go back to home
    await page.goto('/iris.html#/');
    await expect(page.locator('h2:has-text("Favourites")')).toBeVisible({ timeout: 10000 });

    // Debug: Check localStorage
    const apps = await page.evaluate(() => localStorage.getItem('iris:apps'));
    console.log('Saved apps:', apps);

    // The PWA should appear in favorites
    await expect(page.locator('button:has-text("Test PWA")')).toBeVisible({ timeout: 10000 });
  });

  test('should show filled star for bookmarked nhash URL', async ({ page }) => {
    test.slow();

    await page.goto('/iris.html');

    const addressBar = page.locator('input[placeholder="Search or enter address"]');
    await addressBar.fill(PWA_URL);
    await addressBar.press('Enter');

    // Wait for iframe and click star
    const iframe = page.locator('iframe[title="App"]');
    await expect(iframe).toBeVisible({ timeout: 10000 });

    const starButton = page.locator('button[title="Add bookmark"]');
    await starButton.click();

    // Wait for nhash navigation
    await expect(addressBar).toHaveValue(/nhash/, { timeout: 30000 });

    // Star should now show "Remove bookmark" title (bookmarked state)
    const removeButton = page.locator('button[title="Remove bookmark"]');
    await expect(removeButton).toBeVisible({ timeout: 5000 });
  });
});
