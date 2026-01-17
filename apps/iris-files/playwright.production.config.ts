import { defineConfig, devices } from '@playwright/test';

process.env.PLAYWRIGHT_PRODUCTION = 'true';

/**
 * Playwright config for testing with production relays.
 * Use this when you need to test loading real content from Nostr.
 *
 * Run with: pnpm exec playwright test --config=playwright.production.config.ts
 */
export default defineConfig({
  testDir: './e2e',
  fullyParallel: false, // Serial for production relay tests
  forbidOnly: !!process.env.CI,
  retries: 0,
  workers: 1,
  reporter: 'list',
  timeout: 60000, // 60s for production relay tests
  expect: { timeout: 30000 },
  use: {
    baseURL: 'http://localhost:5173',
    trace: 'off',
    actionTimeout: 30000,
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
  ],
  webServer: {
    command: 'pnpm run dev',
    url: 'http://localhost:5173',
    reuseExistingServer: true,
    timeout: 15000,
    // No VITE_TEST_MODE - uses production relays
  },
});
