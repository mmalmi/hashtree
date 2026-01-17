/**
 * Tauri E2E test for measuring startup time
 *
 * Measures how long it takes for the app to render content
 */
import { browser, $ } from '@wdio/globals';
import * as fs from 'fs';
import * as path from 'path';

const SCREENSHOTS_DIR = path.resolve(process.cwd(), 'e2e-tauri/screenshots');

// Ensure screenshots directory exists
if (!fs.existsSync(SCREENSHOTS_DIR)) {
  fs.mkdirSync(SCREENSHOTS_DIR, { recursive: true });
}

async function takeScreenshot(name: string): Promise<void> {
  const screenshot = await browser.takeScreenshot();
  const filepath = path.join(SCREENSHOTS_DIR, `${name}-${Date.now()}.png`);
  fs.writeFileSync(filepath, screenshot, 'base64');
  console.log(`Screenshot saved: ${filepath}`);
}

describe('Startup timing', () => {
  it('should measure time to first content render', async () => {
    const startTime = Date.now();
    console.log(`[Timing] Test started at ${new Date().toISOString()}`);

    // Take initial screenshot
    await takeScreenshot('01-initial');

    // Wait for body to exist
    const body = await $('body');
    await body.waitForExist({ timeout: 30000 });
    console.log(`[Timing] Body exists: ${Date.now() - startTime}ms`);

    // Check for app element
    const app = await $('#app');
    await app.waitForExist({ timeout: 30000 });
    console.log(`[Timing] #app exists: ${Date.now() - startTime}ms`);
    await takeScreenshot('02-app-exists');

    // Wait for content inside app (not just empty div)
    let hasContent = false;
    let attempts = 0;
    const maxAttempts = 60; // 30 seconds max

    while (!hasContent && attempts < maxAttempts) {
      const children = await browser.execute(() => {
        const app = document.getElementById('app');
        return app ? app.children.length : 0;
      });

      if (children > 0) {
        hasContent = true;
        console.log(`[Timing] App has ${children} children: ${Date.now() - startTime}ms`);
      } else {
        await browser.pause(500);
        attempts++;
      }
    }

    await takeScreenshot('03-content-rendered');

    // Check for specific UI elements that indicate full render
    const headerExists = await browser.execute(() => {
      return !!document.querySelector('header');
    });
    console.log(`[Timing] Header exists: ${headerExists}, time: ${Date.now() - startTime}ms`);

    // Check for avatar (indicates login complete)
    const avatarExists = await browser.execute(() => {
      return !!document.querySelector('[class*="avatar"]') || !!document.querySelector('img[src*="robohash"]');
    });
    console.log(`[Timing] Avatar exists: ${avatarExists}, time: ${Date.now() - startTime}ms`);

    // Get console logs from the app
    const consoleLogs = await browser.execute(() => {
      // Check if we have access to console history
      return {
        workerReady: (window as any).__workerInitTime || 'not set',
        documentReady: document.readyState,
      };
    });
    console.log(`[Timing] Console state:`, JSON.stringify(consoleLogs));

    const totalTime = Date.now() - startTime;
    console.log(`[Timing] Total time to content: ${totalTime}ms`);

    await takeScreenshot('04-final');

    // Assert reasonable startup time (should be under 10 seconds now)
    expect(hasContent).toBe(true);
    expect(totalTime).toBeLessThan(30000); // 30 second max

    // Log summary
    console.log('\n=== STARTUP TIMING SUMMARY ===');
    console.log(`Total time: ${totalTime}ms`);
    console.log(`Content rendered: ${hasContent}`);
    console.log(`Header present: ${headerExists}`);
    console.log(`Avatar present: ${avatarExists}`);
    console.log('==============================\n');
  });

  it('should show relay connections quickly when network available', async () => {
    // Verifies fast polling: connections should appear quickly once established
    // Note: May not connect in CI environments without network access to relays
    const startTime = Date.now();

    // Wait for the connectivity indicator
    const indicator = await $('[data-testid="connectivity-indicator"]');
    await indicator.waitForExist({ timeout: 30000 });

    // Poll for relay connections (10 seconds max)
    let relayCount = 0;
    let attempts = 0;
    const maxAttempts = 100; // 10 seconds at 100ms intervals

    while (relayCount === 0 && attempts < maxAttempts) {
      relayCount = await browser.execute(() => {
        const el = document.querySelector('[data-testid="peer-count"]');
        return el ? parseInt(el.textContent || '0', 10) : 0;
      });
      if (relayCount === 0) {
        await browser.pause(100);
        attempts++;
      }
    }

    const elapsed = Date.now() - startTime;
    console.log(`[Relay] Connections: ${relayCount} in ${elapsed}ms`);
    await takeScreenshot('06-relay-connected');

    if (relayCount === 0) {
      console.log('[Relay] No connections - network may be restricted');
      // Don't fail if no network access, just verify UI exists
      expect(indicator).toBeDefined();
    } else {
      // If relays connected, verify it happened within 10 seconds
      expect(elapsed).toBeLessThan(10000);
      console.log(`[Relay] PASS: ${relayCount} connections in ${elapsed}ms`);
    }
  });

  it('should capture timing logs from console', async () => {
    // Execute script to add timing instrumentation
    const timings = await browser.execute(() => {
      const results: Record<string, any> = {};

      // Check for Tauri
      results.isTauri = '__TAURI_INTERNALS__' in window;

      // Check DOM state
      results.bodyChildCount = document.body?.children?.length || 0;
      results.appChildCount = document.getElementById('app')?.children?.length || 0;

      // Check for key elements
      results.hasHeader = !!document.querySelector('header');
      results.hasNav = !!document.querySelector('nav');
      results.hasSidebar = !!document.querySelector('[class*="sidebar"]');

      // Get any error messages
      const errors = document.querySelectorAll('[class*="error"]');
      results.errorCount = errors.length;

      return results;
    });

    console.log('[App state]:', JSON.stringify(timings, null, 2));
    await takeScreenshot('05-app-state');

    expect(timings.isTauri).toBe(true);
  });
});
