/**
 * WebdriverIO config for Tauri E2E testing
 *
 * Prerequisites:
 * 1. Build the Tauri app: pnpm tauri:build
 * 2. Start Xvfb: Xvfb :99 -screen 0 1920x1080x24 &
 * 3. Run tests: DISPLAY=:99 pnpm test:tauri
 */
import type { Options } from '@wdio/types';
import { spawn, ChildProcess, execSync } from 'child_process';
import * as path from 'path';
import { fileURLToPath } from 'url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

let tauriDriver: ChildProcess | null = null;
let weStartedDriver = false;

// Check if tauri-driver is already running
function isDriverRunning(): boolean {
  try {
    execSync('curl -s http://127.0.0.1:4444/status', { timeout: 2000 });
    return true;
  } catch {
    return false;
  }
}

export const config: Options.Testrunner = {
  autoCompileOpts: {
    autoCompile: true,
    tsNodeOpts: {
      project: './tsconfig.json',
      transpileOnly: true,
    },
  },
  specs: ['./e2e-tauri/**/*.spec.ts'],
  exclude: [],
  maxInstances: 1,
  // Connect to tauri-driver WebDriver server
  hostname: '127.0.0.1',
  port: 4444,
  capabilities: [
    {
      maxInstances: 1,
      'tauri:options': {
        application: path.resolve(
          __dirname,
          '../apps/iris-files/src-tauri/target/release/iris'
        ),
      },
    } as WebdriverIO.Capabilities,
  ],
  logLevel: 'info',
  bail: 0,
  waitforTimeout: 30000,
  connectionRetryTimeout: 120000,
  connectionRetryCount: 3,
  framework: 'mocha',
  reporters: ['spec'],
  mochaOpts: {
    ui: 'bdd',
    timeout: 180000,
  },

  // Start tauri-driver before tests (if not already running)
  onPrepare: async function () {
    if (isDriverRunning()) {
      console.log('[tauri-driver] Already running, skipping spawn');
      return;
    }

    console.log('[tauri-driver] Starting...');
    tauriDriver = spawn('tauri-driver', ['--port', '4444', '--native-driver', '/usr/bin/WebKitWebDriver'], {
      stdio: ['ignore', 'pipe', 'pipe'],
      env: { ...process.env, DISPLAY: process.env.DISPLAY || ':99' },
      detached: false,
    });
    weStartedDriver = true;

    tauriDriver.stdout?.on('data', (data) => {
      console.log(`[tauri-driver] ${data.toString().trim()}`);
    });
    tauriDriver.stderr?.on('data', (data) => {
      console.error(`[tauri-driver] ${data.toString().trim()}`);
    });

    tauriDriver.on('error', (err) => {
      console.error('[tauri-driver] Failed to start:', err);
    });

    tauriDriver.on('exit', (code) => {
      if (code !== 0 && code !== null) {
        console.error(`[tauri-driver] Exited with code ${code}`);
      }
    });

    // Wait for tauri-driver to be ready
    for (let i = 0; i < 10; i++) {
      await new Promise((resolve) => setTimeout(resolve, 500));
      if (isDriverRunning()) {
        console.log('[tauri-driver] Ready');
        return;
      }
    }
    console.warn('[tauri-driver] May not be ready yet');
  },

  // Stop tauri-driver after tests (only if we started it)
  onComplete: async function () {
    if (weStartedDriver && tauriDriver) {
      console.log('[tauri-driver] Stopping...');
      tauriDriver.kill();
      tauriDriver = null;
    }
  },
};
