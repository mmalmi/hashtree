import { defineConfig, devices } from '@playwright/test';
import os from 'os';
import { getPublicKey } from 'nostr-tools';
import { BOOTSTRAP_SECKEY_HEX } from './e2e/nostr-test-keys';

// Workers: use PW_WORKERS env var, or default to 100% of CPU cores.
// PW_WORKERS can be a number (4) or percentage (100%).
const workersEnv = process.env.PW_WORKERS;
const maxWorkersEnv = process.env.PW_MAX_WORKERS;
const maxWorkers = Number.isFinite(Number(maxWorkersEnv)) ? Number(maxWorkersEnv) : 32;
const cpuCount = os.cpus().length || 1;
const resolveWorkers = (): number | string => {
  if (!workersEnv) {
    return Math.min(cpuCount, maxWorkers);
  }
  if (/^\d+$/.test(workersEnv)) {
    return parseInt(workersEnv, 10);
  }
  const percentMatch = workersEnv.match(/^(\d+)%$/);
  if (percentMatch) {
    const percent = Math.max(1, Math.min(100, parseInt(percentMatch[1], 10)));
    const computed = Math.max(1, Math.floor((cpuCount * percent) / 100));
    return Math.min(computed, maxWorkers);
  }
  return workersEnv;
};
const workers = resolveWorkers();

const slowSpecs = [
  'e2e/yjs-collaboration.spec.ts',
  'e2e/livestream-viewer.spec.ts',
];
const fastMode = process.env.E2E_FAST === '1';

const testBootstrapPubkey = process.env.VITE_TEST_BOOTSTRAP_PUBKEY ?? getPublicKey(BOOTSTRAP_SECKEY_HEX);
process.env.VITE_TEST_BOOTSTRAP_PUBKEY = testBootstrapPubkey;

/**
 * Playwright E2E test configuration.
 *
 * The webServer config below automatically starts the dev server before tests.
 * No need to manually run `pnpm dev` first - just run `pnpm test:e2e`.
 */
export default defineConfig({
  testDir: './e2e',
  testIgnore: fastMode ? slowSpecs : undefined,
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0, // Retry once on CI to handle flaky tests
  workers,
  reporter: 'list',
  timeout: 60000, // 60s global timeout for parallel stability
  expect: { timeout: 20000 }, // 20s for expect assertions
  use: {
    baseURL: 'http://localhost:5173',
    trace: 'off',
    actionTimeout: 20000,
    navigationTimeout: 60000,
    launchOptions: {
      args: ['--disable-features=WebRtcHideLocalIpsWithMdns'],
    },
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
  ],
  webServer: [
    {
      command: 'node e2e/relay/index.js',
      url: 'http://localhost:4736',
      reuseExistingServer: !process.env.CI,
      timeout: 5000,
    },
    {
      command: 'pnpm run dev',
      url: 'http://localhost:5173',
      reuseExistingServer: !process.env.CI,
      timeout: 15000,
      env: {
        // Test mode: local relay, no Blossom, others pool disabled
        VITE_TEST_MODE: 'true',
        VITE_TEST_RELAY: 'ws://localhost:4736',
        VITE_TEST_BOOTSTRAP_PUBKEY: testBootstrapPubkey,
        CHOKIDAR_USEPOLLING: '1',
        CHOKIDAR_INTERVAL: '100',
      },
    },
  ],
});
