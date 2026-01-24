import { test, expect, type Page } from './fixtures';
import { setupPageErrorHandler, disableOthersPool, waitForAppReady, presetProductionRelaysInDB, getTestRelayUrl, useLocalRelay, navigateToPublicFolder } from './test-utils.js';
import * as fs from 'fs/promises';
import * as path from 'path';
import * as os from 'os';
import { execSync } from 'child_process';

const README_NAME = 'README.md';
const SRC_DIR = 'src';
const MAIN_FILE = 'main.ts';

async function createTempGitRepo(): Promise<string> {
  const tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), 'git-perf-'));
  execSync('git init', { cwd: tmpDir });
  execSync('git config user.email "test@example.com"', { cwd: tmpDir });
  execSync('git config user.name "Test User"', { cwd: tmpDir });

  await fs.mkdir(path.join(tmpDir, SRC_DIR), { recursive: true });
  await fs.writeFile(path.join(tmpDir, README_NAME), '# Git Perf\n');
  await fs.writeFile(path.join(tmpDir, SRC_DIR, MAIN_FILE), 'export const answer = 42;\n');

  execSync('git add .', { cwd: tmpDir });
  execSync('git commit -m "Initial commit"', { cwd: tmpDir });

  await fs.writeFile(path.join(tmpDir, SRC_DIR, 'utils.ts'), 'export function add(a: number, b: number) { return a + b; }\n');
  execSync('git add .', { cwd: tmpDir });
  execSync('git commit -m "Add utils"', { cwd: tmpDir });

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
  const repoName = `git-perf-${Date.now()}`;
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

test.describe('Git performance', () => {
  test('measure git operations on hashtree repo', async ({ page }) => {
    test.slow(); // This test needs more time

    setupPageErrorHandler(page);

    // Collect console logs
    const perfLogs: string[] = [];
    const allLogs: string[] = [];
    page.on('console', msg => {
      const text = msg.text();
      allLogs.push(text);
      if (text.includes('[git perf]') || text.includes('[git]') || text.includes('getCommitCountFast')) {
        perfLogs.push(text);
        console.log(text);
      }
    });

    await page.goto('/');
    await presetProductionRelaysInDB(page);
    await page.reload();
    await waitForAppReady(page);
    await disableOthersPool(page);

    const testRelayUrl = getTestRelayUrl();
    const isTestMode = await page.evaluate((relayUrl) => {
      const store = (window as any).__settingsStore;
      if (!store?.subscribe) return false;
      let settings: any = null;
      store.subscribe((value: any) => { settings = value; })();
      return settings?.network?.relays?.includes(relayUrl);
    }, testRelayUrl);

    if (isTestMode) {
      console.log('Using local relay for git perf test...');
      await useLocalRelay(page);
      const { repoName, npub } = await uploadGitRepo(page);
      console.log('Navigating to local git repo...');
      await page.goto(`/#/${npub}/public/${repoName}?g=${encodeURIComponent(repoName)}`);
    } else {
      // Navigate to hashtree repo with production config
      console.log('Navigating to hashtree repo...');
      await page.goto('/#/npub1xndmdgymsf4a34rzr7346vp8qcptxf75pjqweh8naa8rklgxpfqqmfjtce/hashtree');
    }

    await waitForAppReady(page);

    // Wait for relay connection indicator
    console.log('Waiting for relay connection...');
    await expect(page.locator('[class*="i-lucide-wifi"]')).toBeVisible({ timeout: 30000 });

    // Wait for git repo view to appear (directory listing table)
    console.log('Waiting for directory listing...');
    await expect(page.locator('[data-testid="file-list"] table').first()).toBeVisible({ timeout: 60000 });

    // Wait for file last commits to complete by polling our collected logs
    console.log('Waiting for file commit info (file last commits)...');
    const startTime = Date.now();

    // Poll until we see both completion logs
    let fileCommitsTime = '';
    let commitCountTime = '';

    await expect.poll(() => {
      if (!fileCommitsTime) {
        const fileLog = perfLogs.find(log => log.includes('getFileLastCommits completed'));
        if (fileLog) {
          const match = fileLog.match(/completed in (\d+) ms/);
          if (match) fileCommitsTime = match[1];
        }
      }

      if (!commitCountTime) {
        const countLog = perfLogs.find(log => log.includes('getCommitCountFast completed'));
        if (countLog) {
          const match = countLog.match(/completed in (\d+) ms/);
          if (match) commitCountTime = match[1];
        }
      }

      return Boolean(fileCommitsTime && commitCountTime);
    }, { timeout: 90000, intervals: [500, 1000, 2000, 3000] }).toBe(true);

    const loadTime = Date.now() - startTime;
    console.log(`\nFile commit info completed in: ${fileCommitsTime || 'N/A'}ms`);
    console.log(`Commit count completed in: ${commitCountTime || 'N/A'}ms`);
    console.log(`Total wait time: ${loadTime}ms`);

    // Print all perf logs
    console.log('\n=== Performance Logs ===');
    for (const log of perfLogs) {
      console.log(log);
    }
    console.log('========================\n');

    // Print summary
    console.log('Total logs collected:', allLogs.length);
    console.log('Perf logs collected:', perfLogs.length);
  });
});
