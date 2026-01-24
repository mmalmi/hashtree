import { test, expect, type Page } from './fixtures';
import { setupPageErrorHandler, disableOthersPool, waitForAppReady, navigateToPublicFolder } from './test-utils.js';
import * as fs from 'fs/promises';
import * as path from 'path';
import * as os from 'os';
import { execSync } from 'child_process';

const SUBDIR_NAME = 'subdir';
const SUBFILE_NAME = 'file.txt';
const README_NAME = 'README.md';

async function createTempGitRepo(): Promise<string> {
  const tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), 'git-file-bar-'));
  execSync('git init', { cwd: tmpDir });
  execSync('git config user.email "test@example.com"', { cwd: tmpDir });
  execSync('git config user.name "Test User"', { cwd: tmpDir });

  await fs.mkdir(path.join(tmpDir, SUBDIR_NAME), { recursive: true });
  await fs.writeFile(path.join(tmpDir, README_NAME), '# Git File Bar\n');
  await fs.writeFile(path.join(tmpDir, SUBDIR_NAME, SUBFILE_NAME), 'hello from subdir\n');

  execSync('git add .', { cwd: tmpDir });
  execSync('git commit -m "Initial commit"', { cwd: tmpDir });
  return tmpDir;
}

async function collectRepoFiles(rootDir: string, basePath = ''): Promise<Array<{ relativePath: string; content: number[] }>> {
  const entries = await fs.readdir(rootDir, { withFileTypes: true });
  const files: Array<{ relativePath: string; content: number[] }> = [];

  for (const entry of entries) {
    const fullPath = path.join(rootDir, entry.name);
    const relativePath = basePath ? `${basePath}/${entry.name}` : entry.name;

    if (entry.isDirectory()) {
      files.push(...await collectRepoFiles(fullPath, relativePath));
      continue;
    }

    if (entry.isFile()) {
      const content = await fs.readFile(fullPath);
      files.push({ relativePath, content: Array.from(content) });
    }
  }

  return files;
}

async function uploadGitRepo(page: Page): Promise<{ repoName: string; npub: string }> {
  await navigateToPublicFolder(page, { requireRelay: false });

  const npub = await page.evaluate(() => (window as any).__nostrStore?.getState?.()?.npub || '');
  const repoName = `git-bar-${Date.now()}`;
  const repoPath = await createTempGitRepo();
  const files = await collectRepoFiles(repoPath);

  await page.evaluate(async ({ repoName, files }) => {
    const { uploadFilesWithPaths } = await import('/src/stores/upload.ts');
    const filesWithPaths = files.map((entry: { relativePath: string; content: number[] }) => {
      const name = entry.relativePath.split('/').pop() || 'file';
      const data = new Uint8Array(entry.content);
      const file = new File([data], name);
      return { file, relativePath: `${repoName}/${entry.relativePath}` };
    });
    await uploadFilesWithPaths(filesWithPaths);
  }, { repoName, files });

  const repoLink = page.locator('[data-testid="file-list"] a').filter({ hasText: repoName }).first();
  await expect(repoLink).toBeVisible({ timeout: 30000 });

  return { repoName, npub };
}

test.describe('Git file bar', () => {
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await waitForAppReady(page);
    await disableOthersPool(page);
  });

  test('shows commit info when viewing a file in git repo', async ({ page }) => {
    test.slow();

    const { repoName, npub } = await uploadGitRepo(page);

    await page.goto(`/#/${npub}/public/${repoName}/${README_NAME}?g=${encodeURIComponent(repoName)}`);
    await waitForAppReady(page);

    const gitBar = page.locator('[data-testid="git-file-bar"]');
    await expect(gitBar).toBeVisible({ timeout: 60000 });
    await expect(gitBar.locator('button[title*="history" i]')).toBeVisible({ timeout: 60000 });
    await expect(gitBar.getByText(/\d+\s+(second|minute|hour|day|week|month|year)s?\s+ago/i)).toBeVisible();
  });

  test('clicking history opens git history modal', async ({ page }) => {
    test.slow();

    const { repoName, npub } = await uploadGitRepo(page);

    await page.goto(`/#/${npub}/public/${repoName}/${README_NAME}?g=${encodeURIComponent(repoName)}`);
    await waitForAppReady(page);

    const gitBar = page.locator('[data-testid="git-file-bar"]');
    await expect(gitBar).toBeVisible({ timeout: 60000 });
    await expect(gitBar.locator('button[title*="history" i]')).toBeVisible({ timeout: 60000 });
    await gitBar.locator('button[title*="history" i]').click();
    await expect(page.locator('[data-testid="git-history-modal"]')).toBeVisible({ timeout: 10000 });
  });

  test('shows git bar when viewing file in subdirectory via navigation', async ({ page }) => {
    test.slow();

    const { repoName, npub } = await uploadGitRepo(page);

    await page.goto(`/#/${npub}/public/${repoName}?g=${encodeURIComponent(repoName)}`);
    await waitForAppReady(page);

    const fileList = page.locator('[data-testid="file-list"]').last();
    const dirCell = fileList.locator('tbody tr td:nth-child(2)').filter({ hasText: SUBDIR_NAME }).first();
    await expect(dirCell).toBeVisible({ timeout: 30000 });
    await dirCell.click();
    await page.waitForFunction(
      (dir) => window.location.hash.includes(encodeURIComponent(dir)),
      SUBDIR_NAME,
      { timeout: 15000 }
    );

    const fileCell = fileList.locator('tbody tr td:nth-child(2)').filter({ hasText: SUBFILE_NAME }).first();
    await expect(fileCell).toBeVisible({ timeout: 30000 });
    await fileCell.click();
    await page.waitForFunction(
      (name) => window.location.hash.includes(encodeURIComponent(name)),
      SUBFILE_NAME,
      { timeout: 15000 }
    );

    const gitBar = page.locator('[data-testid="git-file-bar"]');
    await expect(gitBar).toBeVisible({ timeout: 60000 });
    await expect(gitBar.getByText(/\d+\s+(second|minute|hour|day|week|month|year)s?\s+ago/i)).toBeVisible();
  });
});
