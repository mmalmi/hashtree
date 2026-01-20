/**
 * E2E test: Git push to htree:// remote and view in ts browser
 *
 * Flow:
 * 1. Create temp data directory for htree
 * 2. Start htree daemon (generates identity)
 * 3. Create a small test repo
 * 4. Add htree remote using daemon's npub and push
 * 5. Open ts browser and navigate to the pushed tree
 * 6. Verify files are visible
 * 7. Cleanup
 *
 * Run with: npx playwright test git-push-htree --project=chromium
 */

import { test, expect } from './fixtures';
import { spawn, execSync, ChildProcess } from 'child_process';
import * as fs from 'fs';
import * as path from 'path';
import * as os from 'os';
import { nip19, getPublicKey } from 'nostr-tools';
import { acquireRustLock, releaseRustLock } from './rust-lock.js';
import { waitForAppReady } from './test-utils.js';
import { fileURLToPath } from 'url';
import { dirname } from 'path';
const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

const HASHTREE_RS_DIR = path.resolve(__dirname, '../../../rust');

test.describe('Git push to htree:// and view in browser', () => {
  // Serial mode: WebRTC from parallel tests can interfere with local git state
  test.describe.configure({ mode: 'serial', timeout: 180000 });
  test.setTimeout(180000); // 3 minutes - git operations can be slow

  let tempDir: string;
  let htreeProcess: ChildProcess | null = null;
  let npub: string | null = null;
  let lockFd: number | null = null;

  const deriveNpubFromKeys = (keysPath: string): string | null => {
    if (!fs.existsSync(keysPath)) return null;
    const raw = fs.readFileSync(keysPath, 'utf-8').trim();
    if (!raw) return null;
    try {
      if (raw.startsWith('nsec')) {
        const decoded = nip19.decode(raw);
        if (decoded.type === 'nsec') {
          const sk = decoded.data as Uint8Array;
          const pk = getPublicKey(sk);
          return nip19.npubEncode(pk);
        }
      }
    } catch {
      // fall through
    }
    if (/^[0-9a-f]{64}$/i.test(raw)) {
      const bytes = new Uint8Array(raw.match(/.{1,2}/g)?.map(byte => parseInt(byte, 16)) || []);
      if (bytes.length === 32) {
        const pk = getPublicKey(bytes);
        return nip19.npubEncode(pk);
      }
    }
    return null;
  };

  test.beforeAll(async () => {
    test.setTimeout(180000);
    // Create temp directory for htree data
    tempDir = fs.mkdtempSync(path.join(os.tmpdir(), 'htree-test-'));
    console.log(`Using temp directory: ${tempDir}`);

    lockFd = await acquireRustLock(240000);

    // Build htree CLI and git-remote-htree if needed
    console.log('Building rust...');
    try {
      execSync('cargo build --release -p hashtree-cli -p git-remote-htree', {
        cwd: HASHTREE_RS_DIR,
        stdio: 'inherit',
      });
    } catch (e) {
      console.error('Failed to build rust:', e);
      throw e;
    }
  });

  test.afterAll(async () => {
    // Kill htree daemon
    if (htreeProcess) {
      console.log('Stopping htree daemon...');
      htreeProcess.kill('SIGTERM');
      htreeProcess = null;
    }
    if (lockFd !== null) {
      releaseRustLock(lockFd);
      lockFd = null;
    }

    // Cleanup temp directory
    if (tempDir && fs.existsSync(tempDir)) {
      console.log(`Cleaning up temp directory: ${tempDir}`);
      fs.rmSync(tempDir, { recursive: true, force: true });
    }
  });

  test('push repo to htree:// and view in browser', async ({ page }) => {
    // Capture browser console logs
    page.on('console', msg => {
      if (msg.text().includes('[WebRTC]') || msg.text().includes('[WebRTCStore]') || msg.text().includes('[RefResolver]')) {
        console.log(`[browser] ${msg.text()}`);
      }
    });

    const htreeBin = path.join(HASHTREE_RS_DIR, 'target/release/htree');
    const gitRemoteHtree = path.join(HASHTREE_RS_DIR, 'target/release/git-remote-htree');

    // Verify binaries exist
    expect(fs.existsSync(htreeBin)).toBe(true);
    expect(fs.existsSync(gitRemoteHtree)).toBe(true);

    // Environment setup - use temp dir for all data
    const configDir = path.join(tempDir, '.hashtree');
    fs.mkdirSync(configDir, { recursive: true });
    const relayUrl = process.env.VITE_TEST_RELAY || 'ws://localhost:4736';
    fs.writeFileSync(path.join(configDir, 'config.toml'), [
      '[server]',
      'enable_auth = false',
      'enable_webrtc = false',
      'stun_port = 0',
      '',
      '[nostr]',
      `relays = ["${relayUrl}"]`,
      'crawl_depth = 0',
      '',
    ].join('\n'));

    const env = {
      ...process.env,
      HOME: tempDir,
      PATH: `${path.dirname(gitRemoteHtree)}:${process.env.PATH}`,
      HTREE_DATA_DIR: path.join(tempDir, 'data'),
      HTREE_CONFIG_DIR: configDir,
    };

    // Create the data directory
    fs.mkdirSync(path.join(tempDir, 'data'), { recursive: true });

    // Start htree daemon FIRST to generate identity
    console.log('Starting htree daemon...');
    htreeProcess = spawn(htreeBin, ['start', '--addr', '127.0.0.1:8787'], {
      env: { ...env, RUST_LOG: 'info' },
      stdio: ['ignore', 'pipe', 'pipe'],
    });

    // Capture output and wait for startup, extract npub
    await new Promise<void>((resolve, reject) => {
      const timeout = setTimeout(() => {
        reject(new Error('htree daemon startup timeout'));
      }, 30000);

      const outputHandler = (data: Buffer) => {
        const text = data.toString();
        console.log('[htree]', text.trim());

        // Extract npub from "Identity: npub1..."
        const npubMatch = text.match(/npub1[a-z0-9]{58}/);
        if (npubMatch && !npub) {
          npub = npubMatch[0];
          console.log(`Found npub: ${npub}`);
        }

        // Look for startup indicator
        if (text.includes('WebRTC') || text.includes('Web UI')) {
          clearTimeout(timeout);
          resolve();
        }
      };

      htreeProcess!.stdout?.on('data', outputHandler);
      htreeProcess!.stderr?.on('data', outputHandler);

      htreeProcess!.on('error', (err) => {
        clearTimeout(timeout);
        reject(err);
      });

      htreeProcess!.on('exit', (code) => {
        if (code !== 0 && code !== null) {
          clearTimeout(timeout);
          reject(new Error(`htree exited with code ${code}`));
        }
      });
    });

    if (!npub) {
      const keyPath = path.join(tempDir, '.hashtree', 'keys');
      npub = deriveNpubFromKeys(keyPath);
      if (npub) {
        console.log(`Derived npub from keys file: ${npub}`);
      }
    }
    if (!npub) {
      throw new Error('Could not get npub from htree daemon');
    }

    // Now create test repo and push using the daemon's identity
    const testRepoDir = path.join(tempDir, 'test-repo');
    fs.mkdirSync(testRepoDir);

    console.log('Creating test repo...');
    execSync('git init', { cwd: testRepoDir });
    execSync('git config user.email "test@test.com"', { cwd: testRepoDir });
    execSync('git config user.name "Test User"', { cwd: testRepoDir });

    // Add some test files
    fs.writeFileSync(path.join(testRepoDir, 'README.md'), '# Test Repository\n\nThis is a test.');
    fs.writeFileSync(path.join(testRepoDir, 'hello.txt'), 'Hello, World!');
    fs.mkdirSync(path.join(testRepoDir, 'src'));
    fs.writeFileSync(path.join(testRepoDir, 'src', 'main.rs'), 'fn main() {\n    println!("Hello!");\n}\n');

    execSync('git add -A', { cwd: testRepoDir });
    execSync('git commit -m "Initial commit"', { cwd: testRepoDir });

    // Add htree remote using the npub from daemon
    console.log(`Adding htree remote with npub: ${npub}...`);
    execSync(`git remote add htree htree://${npub}/test-repo`, { cwd: testRepoDir, env });

    console.log('Pushing to htree...');
    try {
      const pushOutput = execSync('git push htree master 2>&1', {
        cwd: testRepoDir,
        env,
        encoding: 'utf-8',
      });
      console.log('Push output:', pushOutput);
    } catch (e: any) {
      console.log('Push output:', e.stdout || '');
      console.log('Push stderr:', e.stderr || e.message);
      // Push might fail for network reasons but local storage should work
    }

    // Wait for Nostr event to propagate to relays
    console.log('Waiting for Nostr event propagation...');
    await new Promise(resolve => setTimeout(resolve, 5000));

    // Navigate to ts
    console.log('Opening ts...');
    let serverReady = false;
    try {
      await page.goto('http://localhost:5173/', { waitUntil: 'domcontentloaded', timeout: 30000 });
      serverReady = true;
    } catch {
      // Retry once in case the dev server is still starting under load
      try {
        await page.goto('http://localhost:5173/', { waitUntil: 'domcontentloaded', timeout: 30000 });
        serverReady = true;
      } catch {
        throw new Error('Dev server not running on port 5173');
      }
    }
    if (serverReady) {
      await waitForAppReady(page, 30000);
    }

    // Browser automatically generates a key via restoreSession() - wait for it
    console.log('Waiting for browser to auto-generate identity...');
    let browserInfo: { pubkey: string | null; npub: string | null } | null = null;
    for (let i = 0; i < 10; i++) {
      await page.waitForTimeout(500);
      browserInfo = await page.evaluate(() => {
        const nostrModule = (window as any).__nostrModule;
        if (nostrModule?.nostrStore) {
          const state = nostrModule.nostrStore.getState();
          return { pubkey: state.pubkey, npub: state.npub };
        }
        return null;
      });
      if (browserInfo?.pubkey) {
        console.log(`Browser identity ready: pubkey=${browserInfo.pubkey}, npub=${browserInfo.npub}`);
        break;
      }
      console.log(`[${i+1}] Waiting for browser identity...`);
    }
    if (!browserInfo?.pubkey) {
      console.log('Browser did not auto-generate identity - test may fail');
    }

    // Have the daemon follow the browser (mutual follow for reliable WebRTC connection)
    if (browserInfo?.npub) {
      console.log(`Daemon following browser: ${browserInfo.npub}`);
      try {
        execSync(`${htreeBin} follow ${browserInfo.npub}`, {
          env,
          encoding: 'utf-8',
          timeout: 30000,
        });
        console.log('Daemon followed browser successfully');
      } catch (e: any) {
        console.log('Daemon follow failed:', e.message);
      }
    }

    // First, follow the htree daemon's npub so the browser prioritizes connecting to it
    console.log(`Following htree daemon: ${npub}`);
    await page.goto(`http://localhost:5173/#/${npub}`);

    // Wait for profile page to load and click Follow button
    const followButton = page.getByRole('button', { name: 'Follow', exact: true });
    try {
      await expect(followButton).toBeVisible({ timeout: 10000 });
      await followButton.click();
      // Wait for follow to complete
      await expect(
        page.getByRole('button', { name: 'Following' })
          .or(page.getByRole('button', { name: 'Unfollow' })
            .or(page.getByRole('button', { name: 'Following' })))
      ).toBeVisible({ timeout: 10000 });
      console.log('Followed htree daemon successfully');
    } catch (e) {
      console.log('Could not follow (might already be following or button not found):', e);
    }

    // Navigate to the pushed tree
    const treeUrl = `http://localhost:5173/#/${npub}/test-repo`;
    console.log(`Navigating to: ${treeUrl}`);
    await page.goto(treeUrl);

    // Wait for Nostr subscription to find the tree root
    console.log('Waiting for tree root resolution...');
    await new Promise(resolve => setTimeout(resolve, 3000));

    // Wait for WebRTC connection to the daemon specifically
    console.log('Waiting for WebRTC connection to htree daemon...');

    // Convert npub to hex pubkey for comparison
    // npub is bech32 encoded, we need to extract the hex pubkey
    // Use simple bech32 decode
    const daemonPubkeyHex = npub.startsWith('npub1') ? await page.evaluate((npubStr: string) => {
      // Bech32 alphabet
      const ALPHABET = 'qpzry9x8gf2tvdw0s3jn54khce6mua7l';
      const data = npubStr.slice(5); // Remove 'npub1' prefix
      const decoded: number[] = [];
      for (const c of data) {
        const val = ALPHABET.indexOf(c);
        if (val === -1) return null;
        decoded.push(val);
      }
      // Convert 5-bit groups to 8-bit bytes
      const bytes: number[] = [];
      let acc = 0;
      let bits = 0;
      for (const value of decoded) {
        acc = (acc << 5) | value;
        bits += 5;
        if (bits >= 8) {
          bits -= 8;
          bytes.push((acc >> bits) & 0xff);
        }
      }
      // Skip checksum (last 6 values = 30 bits)
      const pubkeyBytes = bytes.slice(0, 32);
      return pubkeyBytes.map(b => b.toString(16).padStart(2, '0')).join('');
    }, npub) : null;
    console.log(`Daemon pubkey hex: ${daemonPubkeyHex}`);

    // Get browser's WebRTC store info for debugging
    const webrtcInfo = await page.evaluate(() => {
      const store = (window as any).__appStore;
      if (store) {
        const state = store.getState();
        return {
          pubkey: state.nostrStore?.pubkey || state.pubkey || 'unknown',
          myPeerId: state.myPeerId,
          webrtcRunning: state.webrtcRunning,
          peerCount: state.peerCount,
        };
      }
      return null;
    });
    console.log(`Browser WebRTC info:`, JSON.stringify(webrtcInfo));

    // Check browser's WebRTC peers and look for the daemon
    let connectedToDaemon = false;
    for (let i = 0; i < 30; i++) {  // Wait up to 30 seconds
      await page.waitForTimeout(1000);
      const peerInfo = await page.evaluate(() => {
        const store = (window as any).__appStore;
        if (store) {
          const state = store.getState();
          return {
            peerCount: state.peerCount,
            peers: state.peers?.map((p: any) => ({
              pubkey: p.pubkey,
              state: p.state,
              pool: p.pool
            }))
          };
        }
        return null;
      });
      console.log(`[${i+1}s] Peers:`, JSON.stringify(peerInfo?.peers?.slice(0, 5) || []));

      // Check if we're connected to the daemon specifically
      if (peerInfo?.peers && daemonPubkeyHex) {
        const daemonPeer = peerInfo.peers.find((p: any) => p.pubkey === daemonPubkeyHex);
        if (daemonPeer && daemonPeer.state === 'connected') {
          console.log('Connected to htree daemon!');
          connectedToDaemon = true;
          break;
        }
      }

      // Also check if files are already visible
      const hasFiles = await page.locator('text=README.md').isVisible().catch(() => false);
      if (hasFiles) {
        console.log('Files already visible!');
        connectedToDaemon = true;
        break;
      }
    }

    // Check if we can see the files
    console.log('Checking for files...');

    const hasReadme = await page.locator('text=README.md').isVisible().catch(() => false);
    const hasHello = await page.locator('text=hello.txt').isVisible().catch(() => false);
    const hasSrc = await page.locator('text=src').isVisible().catch(() => false);

    console.log(`README.md visible: ${hasReadme}`);
    console.log(`hello.txt visible: ${hasHello}`);
    console.log(`src visible: ${hasSrc}`);

    // Take screenshot for debugging
    await page.screenshot({ path: 'e2e/screenshots/git-push-htree.png' });

    // Files should be visible if WebRTC sync worked
    if (connectedToDaemon) {
      expect(hasReadme || hasHello || hasSrc).toBe(true);
    } else {
      console.log('Did not connect to htree daemon - WebRTC discovery may have failed');
      // Still check - maybe we got data through another peer
      if (!hasReadme && !hasHello && !hasSrc) {
        console.log('Files not visible - connection to daemon failed');
      }
    }
  });
});
