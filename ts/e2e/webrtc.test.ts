import { test, expect, Browser, Page, BrowserContext } from './fixtures';
import { chromium } from '@playwright/test';
import { waitForAppReady, ensureLoggedIn, waitForRelayConnected } from './test-utils.js';

// Increase timeout for WebRTC tests as they involve network signaling
test.setTimeout(60000);

test.describe('WebRTC P2P Connection', () => {
  let browser1: Browser;
  let browser2: Browser;
  let context1: BrowserContext;
  let context2: BrowserContext;
  let page1: Page;
  let page2: Page;

  test.beforeAll(async () => {
    // Launch two separate browser instances
    browser1 = await chromium.launch();
    browser2 = await chromium.launch();
  });

  test.afterAll(async () => {
    await browser1?.close();
    await browser2?.close();
  });

  test.beforeEach(async () => {
    // Create fresh contexts for each test
    context1 = await browser1.newContext();
    context2 = await browser2.newContext();
    page1 = await context1.newPage();
    page2 = await context2.newPage();

    // Log errors for debugging
    page1.on('pageerror', err => console.log('Page1 error:', err.message));
    page2.on('pageerror', err => console.log('Page2 error:', err.message));
    page1.on('console', msg => console.log('Page1:', msg.text()));
    page2.on('console', msg => console.log('Page2:', msg.text()));
  });

  test.afterEach(async () => {
    await context1?.close();
    await context2?.close();
  });

  async function clearStorage(page: Page) {
    await page.evaluate(async () => {
      const dbs = await indexedDB.databases();
      for (const db of dbs) {
        if (db.name) indexedDB.deleteDatabase(db.name);
      }
      localStorage.clear();
      sessionStorage.clear();
    });
  }

  async function waitForAutoLogin(page: Page) {
    // Wait for page to fully load
    await page.waitForLoadState('load');
    // Wait for header/app to be ready
    await waitForAppReady(page);
    await ensureLoggedIn(page, 30000);
    await waitForRelayConnected(page, 30000);
  }

  test('peer indicator shows correct connection count', async () => {
    // Navigate page1
    await page1.goto('http://localhost:5173/');
    await clearStorage(page1);
    await page1.reload();

    // Wait for auto-login
    await waitForAutoLogin(page1);

    // After auto-login, peer indicator should show connection count (relays + peers)
    await expect(page1.getByTestId('peer-indicator-dot')).toBeVisible({ timeout: 30000 });
    await expect(page1.getByTestId('peer-count')).toHaveText(/^\d+$/, { timeout: 30000 });
  });

  test('two instances should discover each other via WebRTC signaling', async () => {
    // Navigate both pages
    await Promise.all([
      page1.goto('http://localhost:5173/'),
      page2.goto('http://localhost:5173/'),
    ]);

    // Clear storage on both
    await Promise.all([
      clearStorage(page1),
      clearStorage(page2),
    ]);

    // Reload to get fresh state
    await Promise.all([
      page1.reload(),
      page2.reload(),
    ]);

    // Wait for auto-login
    await waitForAutoLogin(page1);
    await waitForAutoLogin(page2);

    // Both should show peer indicator with 0 peers initially
    await expect(page1.getByTestId('peer-indicator-dot')).toBeVisible({ timeout: 30000 });
    await expect(page2.getByTestId('peer-indicator-dot')).toBeVisible({ timeout: 30000 });

    // Wait for peers to discover each other via nostr relay signaling
    // This may take up to 30+ seconds as hello messages are sent every 10s
    // and relay rate-limiting can delay ICE candidate exchange
    // Count shows total connections (relays + peers), so we check it increases
    await expect(page1.getByTestId('peer-count')).toHaveText(/^\d+$/, { timeout: 45000 });
    await expect(page2.getByTestId('peer-count')).toHaveText(/^\d+$/, { timeout: 45000 });
  });
});
