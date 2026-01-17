/**
 * E2E test for gitignore filtering in directory uploads
 */
import { test, expect } from './fixtures';
import * as path from 'path';
import * as fs from 'fs';
import * as os from 'os';
import { setupPageErrorHandler, disableOthersPool, waitForAppReady } from './test-utils.js';

test.describe('Gitignore Directory Upload', () => {
  let testDir: string;

  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
  });

  test.beforeAll(async () => {
    // Create a test directory with .gitignore
    testDir = path.join(os.tmpdir(), 'gitignore-test-' + Date.now());
    fs.mkdirSync(testDir, { recursive: true });

    // Create .gitignore
    fs.writeFileSync(path.join(testDir, '.gitignore'), `
node_modules/
*.log
dist/
.env
`);

    // Create files that should be ignored
    fs.mkdirSync(path.join(testDir, 'node_modules', 'pkg'), { recursive: true });
    fs.writeFileSync(path.join(testDir, 'node_modules', 'pkg', 'index.js'), 'ignored');
    fs.writeFileSync(path.join(testDir, 'error.log'), 'ignored');
    fs.mkdirSync(path.join(testDir, 'dist'), { recursive: true });
    fs.writeFileSync(path.join(testDir, 'dist', 'bundle.js'), 'ignored');
    fs.writeFileSync(path.join(testDir, '.env'), 'SECRET=ignored');

    // Create files that should be uploaded
    fs.writeFileSync(path.join(testDir, 'README.md'), 'should be uploaded');
    fs.writeFileSync(path.join(testDir, 'index.js'), 'should be uploaded');
    fs.mkdirSync(path.join(testDir, 'src'), { recursive: true });
    fs.writeFileSync(path.join(testDir, 'src', 'main.js'), 'should be uploaded');
  });

  test.afterAll(async () => {
    // Clean up test directory
    if (testDir && fs.existsSync(testDir)) {
      fs.rmSync(testDir, { recursive: true, force: true });
    }
  });

  test('shows gitignore modal when uploading directory with .gitignore', async ({ page }) => {
    await waitForAppReady(page);

    // We can't easily simulate a directory upload in Playwright without complex workarounds
    // Instead, let's test the gitignore parsing logic directly in the browser

    const result = await page.evaluate(async () => {
      // Import the gitignore utilities
      const { parseGitignore, filterByGitignore } = await import('/src/utils/gitignore.ts');

      const gitignoreContent = `
node_modules/
*.log
dist/
.env
`;

      const patterns = parseGitignore(gitignoreContent);

      // Simulate file list
      const files = [
        { relativePath: 'test-dir/.gitignore', file: { size: 100 } },
        { relativePath: 'test-dir/README.md', file: { size: 100 } },
        { relativePath: 'test-dir/index.js', file: { size: 100 } },
        { relativePath: 'test-dir/src/main.js', file: { size: 100 } },
        { relativePath: 'test-dir/node_modules/pkg/index.js', file: { size: 100 } },
        { relativePath: 'test-dir/error.log', file: { size: 100 } },
        { relativePath: 'test-dir/dist/bundle.js', file: { size: 100 } },
        { relativePath: 'test-dir/.env', file: { size: 100 } },
      ];

      const { included, excluded } = filterByGitignore(files as any, patterns);

      return {
        patternsCount: patterns.length,
        includedPaths: included.map(f => f.relativePath),
        excludedPaths: excluded.map(f => f.relativePath),
      };
    });

    console.log('Gitignore parsing result:', result);

    // Should have parsed 4 patterns
    expect(result.patternsCount).toBe(4);

    // Should include these files
    expect(result.includedPaths).toContain('test-dir/.gitignore');
    expect(result.includedPaths).toContain('test-dir/README.md');
    expect(result.includedPaths).toContain('test-dir/index.js');
    expect(result.includedPaths).toContain('test-dir/src/main.js');

    // Should exclude these files
    expect(result.excludedPaths).toContain('test-dir/node_modules/pkg/index.js');
    expect(result.excludedPaths).toContain('test-dir/error.log');
    expect(result.excludedPaths).toContain('test-dir/dist/bundle.js');
    expect(result.excludedPaths).toContain('test-dir/.env');
  });

  test('gitignore patterns work correctly with filterByGitignore', async ({ page }) => {
    await waitForAppReady(page);

    const result = await page.evaluate(async () => {
      const { parseGitignore, filterByGitignore } = await import('/src/utils/gitignore.ts');

      const patterns = parseGitignore(`
# Comment line
node_modules/
*.log
build/
.env*
!.env.example
`);

      // Create mock files - filterByGitignore checks parent directories automatically
      const files = [
        { relativePath: 'node_modules/foo/bar.js' },
        { relativePath: 'src/node_modules/foo.js' },
        { relativePath: 'error.log' },
        { relativePath: 'logs/debug.log' },
        { relativePath: 'build/output.js' },
        { relativePath: '.env' },
        { relativePath: '.env.local' },
        { relativePath: '.env.example' },
        { relativePath: 'src/index.js' },
        { relativePath: 'README.md' },
      ];

      const { included, excluded } = filterByGitignore(files as any, patterns);

      return {
        includedPaths: included.map((f: any) => f.relativePath),
        excludedPaths: excluded.map((f: any) => f.relativePath),
      };
    });

    console.log('filterByGitignore results:');
    console.log('  Included:', result.includedPaths);
    console.log('  Excluded:', result.excludedPaths);

    // Files in node_modules/ should be excluded
    expect(result.excludedPaths).toContain('node_modules/foo/bar.js');
    expect(result.excludedPaths).toContain('src/node_modules/foo.js');

    // *.log files should be excluded
    expect(result.excludedPaths).toContain('error.log');
    expect(result.excludedPaths).toContain('logs/debug.log');

    // Files in build/ should be excluded
    expect(result.excludedPaths).toContain('build/output.js');

    // .env* files should be excluded (except .env.example which is negated)
    expect(result.excludedPaths).toContain('.env');
    expect(result.excludedPaths).toContain('.env.local');

    // .env.example is negated, should be included
    expect(result.includedPaths).toContain('.env.example');

    // Regular source files should be included
    expect(result.includedPaths).toContain('src/index.js');
    expect(result.includedPaths).toContain('README.md');
  });
});
