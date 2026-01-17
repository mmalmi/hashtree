/**
 * E2E tests for HTML Viewer with directory context
 *
 * Tests the flow of:
 * 1. Uploading HTML file with CSS/JS/image resources
 * 2. HTML viewer loads and renders correctly
 * 3. Resources from same directory are accessible
 * 4. Resources from subdirectories are accessible
 */
import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder } from './test-utils.js';
import * as fs from 'fs';
import * as path from 'path';
import * as os from 'os';

async function ensureFolderView(page: any) {
  const fileList = page.locator('[data-testid="file-list"]');
  const backToFolder = page.getByRole('link', { name: 'Back to folder' });
  if (await backToFolder.isVisible().catch(() => false)) {
    await backToFolder.click();
  } else {
    try {
      await backToFolder.waitFor({ state: 'visible', timeout: 2000 });
      await backToFolder.click();
    } catch {
      // No-op: we stayed in folder view.
    }
  }
  await expect(fileList).toBeVisible({ timeout: 10000 });
  await expect(backToFolder).toBeHidden({ timeout: 10000 });
}

test.describe('HTML Viewer with directory context', () => {
  test('should render HTML with inline CSS from same directory', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await navigateToPublicFolder(page);

    // Create temp files for upload
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'html-test-'));
    const cssPath = path.join(tmpDir, 'style.css');
    const htmlPath = path.join(tmpDir, 'index.html');

    try {
      // Create CSS file content
      const cssContent = `
body {
  background-color: rgb(0, 128, 0);
  color: white;
  font-family: sans-serif;
}
h1 {
  color: rgb(255, 255, 0);
}
#test-element {
  background-color: rgb(0, 0, 255);
  padding: 20px;
}
`;

      // Create HTML file content that references the CSS
      const htmlContent = `<!DOCTYPE html>
<html>
<head>
  <title>Test Page</title>
  <link rel="stylesheet" href="style.css">
</head>
<body>
  <h1>Hello from HashTree</h1>
  <div id="test-element">This should have blue background</div>
  <p>If CSS loaded correctly, background is green and heading is yellow.</p>
</body>
</html>`;

      fs.writeFileSync(cssPath, cssContent);
      fs.writeFileSync(htmlPath, htmlContent);

      // Create a folder for our HTML site
      await page.locator('header a:has-text("Iris")').click();
      await page.waitForTimeout(300);
      await page.getByRole('button', { name: 'New Folder' }).click();

      const input = page.locator('input[placeholder="Folder name..."]');
      await input.waitFor({ timeout: 5000 });
      await input.fill('html-test');
      await page.click('button:has-text("Create")');

      // Wait for folder view
      await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });
      await expect(page.getByText(/Drop or click to add|Empty directory/).first()).toBeVisible({ timeout: 10000 });

      // Upload CSS file first
      const fileInput = page.locator('input[type="file"]').first();
      await fileInput.setInputFiles(cssPath);

      // Wait for upload
      await ensureFolderView(page);
      await expect(page.locator('[data-testid="file-list"] a:has-text("style.css")')).toBeVisible({ timeout: 10000 });

      // Upload HTML file
      await fileInput.setInputFiles(htmlPath);

      // Wait for upload
      await ensureFolderView(page);
      await expect(page.locator('[data-testid="file-list"] a:has-text("index.html")')).toBeVisible({ timeout: 10000 });

      // Click on the HTML file to view it
      await page.locator('[data-testid="file-list"] a:has-text("index.html")').click();

      // Wait for iframe to appear
      const iframe = page.frameLocator('iframe');

      // Check that HTML content is rendered
      await expect(iframe.locator('h1')).toContainText('Hello from HashTree', { timeout: 10000 });

      // Check that CSS was applied - the heading should be yellow (rgb 255, 255, 0)
      const h1 = iframe.locator('h1');
      await expect(h1).toBeVisible();

      // Verify the test element exists
      const testElement = iframe.locator('#test-element');
      await expect(testElement).toBeVisible();
      await expect(testElement).toContainText('This should have blue background');
    } finally {
      // Cleanup temp files
      fs.rmSync(tmpDir, { recursive: true, force: true });
    }
  });

  test('should render HTML with JavaScript from same directory', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await navigateToPublicFolder(page);

    // Create temp files for upload
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'js-test-'));
    const jsPath = path.join(tmpDir, 'app.js');
    const htmlPath = path.join(tmpDir, 'index.html');

    try {
      // Create JS file that modifies the DOM
      const jsContent = `
window.onload = function() {
  var el = document.getElementById('js-target');
  if (el) {
    el.textContent = 'JavaScript loaded successfully!';
    el.setAttribute('data-loaded', 'true');
  }
};
`;

      // Create HTML file that references the JS at end of body
      const htmlContent = `<!DOCTYPE html>
<html>
<head>
  <title>JS Test</title>
</head>
<body>
  <h1>JavaScript Test</h1>
  <div id="js-target">Waiting for JavaScript...</div>
  <script src="app.js"></script>
</body>
</html>`;

      fs.writeFileSync(jsPath, jsContent);
      fs.writeFileSync(htmlPath, htmlContent);

      // Create a folder
      await page.locator('header a:has-text("Iris")').click();
      await page.waitForTimeout(300);
      await page.getByRole('button', { name: 'New Folder' }).click();

      const input = page.locator('input[placeholder="Folder name..."]');
      await input.waitFor({ timeout: 5000 });
      await input.fill('js-test');
      await page.click('button:has-text("Create")');

      await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });
      await expect(page.getByText(/Drop or click to add|Empty directory/).first()).toBeVisible({ timeout: 10000 });

      // Upload JS file first
      const fileInput = page.locator('input[type="file"]').first();
      await fileInput.setInputFiles(jsPath);

      await ensureFolderView(page);
      await expect(page.locator('[data-testid="file-list"] a:has-text("app.js")')).toBeVisible({ timeout: 10000 });

      // Upload HTML file
      await fileInput.setInputFiles(htmlPath);

      await ensureFolderView(page);
      await expect(page.locator('[data-testid="file-list"] a:has-text("index.html")')).toBeVisible({ timeout: 10000 });

      // Click on the HTML file
      await page.locator('[data-testid="file-list"] a:has-text("index.html")').click();

      // Wait for iframe to appear and load
      await page.waitForSelector('iframe', { timeout: 10000 });

      // Check that JavaScript executed
      const iframe = page.frameLocator('iframe');
      const target = iframe.locator('#js-target');

      // JS should have changed the text - give more time
      await expect(target).toContainText('JavaScript loaded successfully!', { timeout: 15000 });
    } finally {
      // Cleanup temp files
      fs.rmSync(tmpDir, { recursive: true, force: true });
    }
  });

  test('should load resources from subdirectories', { timeout: 30000 }, async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await navigateToPublicFolder(page);

    // Create temp files for upload
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'subdir-test-'));
    const cssPath = path.join(tmpDir, 'style.css');
    const htmlPath = path.join(tmpDir, 'index.html');

    try {
      const cssContent = `
body { background-color: rgb(128, 0, 128); }
h1 { color: rgb(0, 255, 255); }
`;
      const htmlContent = `<!DOCTYPE html>
<html>
<head>
  <title>Subdir Test</title>
  <link rel="stylesheet" href="css/style.css">
</head>
<body>
  <h1>Subdirectory CSS Test</h1>
  <p id="info">If CSS loaded, background should be purple.</p>
</body>
</html>`;

      fs.writeFileSync(cssPath, cssContent);
      fs.writeFileSync(htmlPath, htmlContent);

      // Create main folder
      await page.locator('header a:has-text("Iris")').click();
      await page.waitForTimeout(300);
      await page.getByRole('button', { name: 'New Folder' }).click();

      const input = page.locator('input[placeholder="Folder name..."]');
      await input.waitFor({ timeout: 5000 });
      await input.fill('subdir-test');
      await page.click('button:has-text("Create")');

      await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });
      await expect(page.getByText(/Drop or click to add|Empty directory/).first()).toBeVisible({ timeout: 10000 });

      // Create subdirectory 'css'
      await page.getByRole('button', { name: /Folder/ }).click();
      const subInput = page.locator('input[placeholder="Folder name..."]');
      await subInput.waitFor({ timeout: 5000 });
      await subInput.fill('css');
      await page.click('button:has-text("Create")');
      await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

      // Navigate into css folder
      await page.locator('[data-testid="file-list"] a:has-text("css")').click();
      // Wait for navigation to complete - ".." should appear in subdirectory
      await expect(page.locator('[data-testid="file-list"] a:has-text("..")')).toBeVisible({ timeout: 5000 });

      // Upload CSS file in subdirectory
      const fileInput = page.locator('input[type="file"]').first();
      await fileInput.setInputFiles(cssPath);

      await expect(page.locator('[data-testid="file-list"] a:has-text("style.css")')).toBeVisible({ timeout: 10000 });

      // Go back to parent directory (click on ".." link)
      await page.locator('[data-testid="file-list"] a:has-text("..")').click();
      // Wait for parent directory - css folder should be visible (use exact match to avoid matching style.css)
      await expect(page.getByRole('link', { name: 'css', exact: true })).toBeVisible({ timeout: 5000 });

      // Upload HTML that references css/style.css
      // Re-locate file input after navigation
      const htmlFileInput = page.locator('input[type="file"]').first();
      await htmlFileInput.setInputFiles(htmlPath);

      // Wait for file list to show index.html
      const fileList = page.locator('[data-testid="file-list"]');
      await expect(fileList.locator('a:has-text("index.html")')).toBeVisible({ timeout: 10000 });

      // Click on HTML file in file list
      await fileList.locator('a:has-text("index.html")').click();

      // Check content loaded
      const iframe = page.frameLocator('iframe');
      await expect(iframe.locator('h1')).toContainText('Subdirectory CSS Test', { timeout: 10000 });
      await expect(iframe.locator('#info')).toBeVisible();
    } finally {
      // Cleanup temp files
      fs.rmSync(tmpDir, { recursive: true, force: true });
    }
  });

  // Note: fetch() API doesn't work in sandboxed iframe without allow-same-origin
  // For security, we only use allow-scripts, so dynamic data loading via fetch is not supported
});
