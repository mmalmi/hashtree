import { test, expect } from '@playwright/test';

test.describe('Git performance', () => {
  test('measure git operations on hashtree repo', async ({ page }) => {
    test.slow(); // This test needs more time

    // Collect console logs
    const perfLogs: string[] = [];
    const allLogs: string[] = [];
    page.on('console', msg => {
      const text = msg.text();
      allLogs.push(text);
      if (text.includes('[git perf]') || text.includes('[git]')) {
        perfLogs.push(text);
        console.log(text);
      }
    });

    // Navigate to hashtree repo with production config
    console.log('Navigating to hashtree repo...');
    await page.goto('/#/npub1xndmdgymsf4a34rzr7346vp8qcptxf75pjqweh8naa8rklgxpfqqmfjtce/hashtree');

    // Wait for relay connection indicator
    console.log('Waiting for relay connection...');
    await expect(page.locator('[class*="i-lucide-wifi"]')).toBeVisible({ timeout: 30000 });

    // Wait for git repo view to appear (directory listing)
    // Use nth(1) because there are two file-list elements - first is regular browser, second is git repo
    console.log('Waiting for directory listing...');
    await expect(page.getByTestId('file-list').nth(1)).toBeVisible({ timeout: 60000 });

    // Wait for file last commits to complete by polling our collected logs
    console.log('Waiting for file commit info (file last commits)...');
    const startTime = Date.now();

    // Poll until we see the completion log
    let attempts = 0;
    while (attempts < 180) { // 180 * 500ms = 90 seconds
      const completionLog = perfLogs.find(log => log.includes('getFileLastCommits completed'));
      if (completionLog) {
        // Extract time from log like "getFileLastCommits completed in 3707 ms"
        const match = completionLog.match(/completed in (\d+) ms/);
        if (match) {
          console.log(`\nFile commit info completed in: ${match[1]}ms`);
        }
        break;
      }
      await page.waitForTimeout(500);
      attempts++;
    }

    const loadTime = Date.now() - startTime;
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
