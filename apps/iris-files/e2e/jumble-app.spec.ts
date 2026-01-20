import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool, useLocalRelay } from './test-utils.js';
import { execSync } from 'child_process';
import fs from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

const JUMBLE_DIR = path.resolve(__dirname, '../../../../jumble');
const JUMBLE_DIST_DIR = path.join(JUMBLE_DIR, 'dist');
const SHOULD_RUN = process.env.E2E_JUMBLE === '1';

test.skip(!SHOULD_RUN, 'Set E2E_JUMBLE=1 to run the Jumble integration test.');

const MIME_TYPES: Record<string, string> = {
  '.html': 'text/html',
  '.css': 'text/css',
  '.js': 'application/javascript',
  '.mjs': 'application/javascript',
  '.json': 'application/json',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.jpeg': 'image/jpeg',
  '.webp': 'image/webp',
  '.ico': 'image/x-icon',
  '.woff': 'font/woff',
  '.woff2': 'font/woff2',
  '.ttf': 'font/ttf',
  '.otf': 'font/otf',
};

function toPosixPath(value: string): string {
  return value.split(path.sep).join('/');
}

function collectDistFiles(distDir: string): Array<{ absolutePath: string; relativePath: string; mimeType: string }> {
  const results: Array<{ absolutePath: string; relativePath: string; mimeType: string }> = [];
  const stack = [distDir];

  while (stack.length) {
    const current = stack.pop();
    if (!current) continue;
    const entries = fs.readdirSync(current, { withFileTypes: true });
    for (const entry of entries) {
      if (entry.name === '.DS_Store') continue;
      const absolutePath = path.join(current, entry.name);
      if (entry.isDirectory()) {
        stack.push(absolutePath);
        continue;
      }
      if (entry.isFile()) {
        const ext = path.extname(entry.name).toLowerCase();
        if (ext === '.map') continue;
        const relativePath = toPosixPath(path.relative(distDir, absolutePath));
        const mimeType = MIME_TYPES[ext] ?? 'application/octet-stream';
        results.push({ absolutePath, relativePath, mimeType });
      }
    }
  }

  return results;
}

function ensureJumbleBuild(): void {
  const shouldBuild = process.env.E2E_JUMBLE_BUILD === '1' || !fs.existsSync(JUMBLE_DIST_DIR);
  if (!shouldBuild) return;

  if (!fs.existsSync(path.join(JUMBLE_DIR, 'node_modules'))) {
    execSync('npm install', { cwd: JUMBLE_DIR, stdio: 'inherit' });
  }

  execSync('npm run build', { cwd: JUMBLE_DIR, stdio: 'inherit' });
}

test.describe('Jumble build integration', () => {
  test('uploads Jumble dist and renders in HTML viewer', async ({ page }) => {
    test.slow();
    test.setTimeout(240000);

    ensureJumbleBuild();
    const distFiles = collectDistFiles(JUMBLE_DIST_DIR);
    expect(distFiles.length).toBeGreaterThan(0);

    setupPageErrorHandler(page);
    page.on('console', (msg) => {
      if (msg.type() === 'error') {
        console.log('[Jumble console error]', msg.text());
      }
    });
    await page.goto('/');
    await disableOthersPool(page);
    await useLocalRelay(page);
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    const rootFolder = `jumble-dist-${Date.now()}`;
    const uploadPayload = distFiles.map((entry) => ({
      relativePath: `${rootFolder}/${entry.relativePath}`,
      base64: fs.readFileSync(entry.absolutePath).toString('base64'),
      mimeType: entry.mimeType,
    }));

    await page.evaluate(async (payload) => {
      const { uploadFilesWithPaths } = await import('/src/stores/upload.ts');
      const filesWithPaths = payload.map((entry: { relativePath: string; base64: string; mimeType: string }) => {
        const name = entry.relativePath.split('/').pop() || 'file';
        const bytes = Uint8Array.from(atob(entry.base64), (c) => c.charCodeAt(0));
        const file = new File([bytes], name, { type: entry.mimeType });
        return { file, relativePath: entry.relativePath };
      });
      await uploadFilesWithPaths(filesWithPaths);
    }, uploadPayload);

    const folderLink = page.locator('[data-testid="file-list"] a').filter({ hasText: rootFolder }).first();
    await expect(folderLink).toBeVisible({ timeout: 60000 });

    const url = page.url();
    const npubMatch = url.match(/npub1[a-z0-9]+/);
    expect(npubMatch).toBeTruthy();
    const targetPath = `${npubMatch![0]}/public/${rootFolder}/index.html`;

    const searchInput = page.locator('input[type="search"], input[placeholder*="Search"]').first();
    await expect(searchInput).toBeVisible({ timeout: 10000 });
    await searchInput.fill(targetPath);
    await searchInput.press('Enter');

    await page.waitForURL(new RegExp(`${rootFolder}.*index\\.html`), { timeout: 60000 });

    const iframe = page.frameLocator('iframe');
    await expect(iframe.locator('#root')).toBeVisible({ timeout: 60000 });
    await expect(iframe.locator('#root > *').first()).toBeVisible({ timeout: 60000 });
  });
});
