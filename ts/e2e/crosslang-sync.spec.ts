/**
 * Cross-language E2E sync test: hashtree-ts (browser) <-> hashtree-rs (Rust)
 *
 * This test verifies actual content sync between TypeScript and Rust implementations:
 * 1. Pre-generates keypairs for both sides so they can mutually follow from start
 * 2. Spawns a hashtree-rs server with test content
 * 3. Uses Playwright to run hashtree-ts in a browser
 * 4. Establishes WebRTC connection between them
 * 5. Verifies content can be synced from Rust to TypeScript
 *
 * Run with: npm run test:e2e -- crosslang-sync
 * Requires: cargo/Rust toolchain installed
 */

import { test, expect } from './fixtures';
import { spawn, execSync, type ChildProcess } from 'child_process';
import { enableOthersPool, ensureLoggedIn, setupPageErrorHandler, useLocalRelay, waitForAppReady, getTestRelayUrl, getCrosslangPort } from './test-utils.js';
import * as path from 'path';
import { fileURLToPath } from 'url';
import { nip19, generateSecretKey, getPublicKey } from 'nostr-tools';
import fs from 'fs';
import { acquireHashtreeRsLock, releaseHashtreeRsLock } from './hashtree-rs-lock.js';

// Run tests in this file serially to avoid WebRTC/timing conflicts
test.describe.configure({ mode: 'serial' });

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const HASHTREE_RS_DIR = path.resolve(__dirname, '../../hashtree-rs');

// Simple bytesToHex implementation
function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes).map(b => b.toString(16).padStart(2, '0')).join('');
}

// Generate a keypair and return all formats
function generateKeypair() {
  const secretKey = generateSecretKey();
  const pubkeyHex = getPublicKey(secretKey);
  const nsec = nip19.nsecEncode(secretKey);
  const npub = nip19.npubEncode(pubkeyHex);
  return { secretKey, pubkeyHex, nsec, npub };
}

// Check if cargo is available
function hasRustToolchain(): boolean {
  try {
    execSync('cargo --version', { stdio: 'ignore' });
    return true;
  } catch {
    return false;
  }
}

// Build htree binary if needed
function ensureHtreeBinary(): string | null {
  try {
    const workspaceRoot = HASHTREE_RS_DIR;

    // Try to find existing binary
    const debugBin = path.join(workspaceRoot, 'target/debug/htree');
    const releaseBin = path.join(workspaceRoot, 'target/release/htree');

    try {
      execSync(`test -f ${debugBin}`, { stdio: 'ignore' });
      return debugBin;
    } catch {}

    try {
      execSync(`test -f ${releaseBin}`, { stdio: 'ignore' });
      return releaseBin;
    } catch {}

    // Build the binary
    console.log('Building htree binary...');
    execSync('cargo build --bin htree --features p2p', { cwd: workspaceRoot, stdio: 'inherit' });
    return debugBin;
  } catch (e) {
    console.log('Failed to build htree:', e);
    return null;
  }
}

test.describe('Cross-Language Sync', () => {
  test.setTimeout(180000);

  test('hashtree-ts syncs content from hashtree-rs via WebRTC', async ({ page }, testInfo) => {
    // Skip if no Rust toolchain
    if (!hasRustToolchain()) {
      test.skip(true, 'Rust toolchain not available');
      return;
    }

    if (!fs.existsSync(path.join(HASHTREE_RS_DIR, 'Cargo.toml'))) {
      test.skip(true, 'hashtree-rs repo not available');
      return;
    }

    const htreeBin = ensureHtreeBinary();
    if (!htreeBin) {
      throw new Error('Could not build htree binary');
    }

    setupPageErrorHandler(page);
    const localRelay = getTestRelayUrl();
    const crosslangPort = getCrosslangPort(testInfo.workerIndex);

    // Pre-generate Rust keypair so TS can follow it immediately on startup
    const rustKeys = generateKeypair();
    console.log(`[Pre-gen] Rust npub: ${rustKeys.npub.slice(0, 20)}...`);

    let rustProcess: ChildProcess | null = null;
    let lockFd: number | null = null;
    let contentHash: string | null = null;
    let tsPubkeyHex: string | null = null;

    try {
      lockFd = await acquireHashtreeRsLock(90000);
      // ===== STEP 1: Start TS app and wait for full initialization =====
      console.log('[TS] Starting app...');
      await page.goto('/');
      await waitForAppReady(page);
      await ensureLoggedIn(page, 20000);
      await useLocalRelay(page);
      await enableOthersPool(page, 2);

      // Page ready - navigateToPublicFolder handles waiting

      // Wait for app to fully initialize (pubkey exists)
      await expect(page.getByRole('link', { name: 'public' }).first()).toBeVisible({ timeout: 20000 });

      // Get TS pubkey for Rust
      tsPubkeyHex = await page.evaluate(() => {
        const nostrStore = (window as any).__nostrStore;
        return nostrStore?.getState()?.pubkey || null;
      });

      if (!tsPubkeyHex) {
        throw new Error('Could not get TS pubkey');
      }
      console.log(`[TS] Pubkey: ${tsPubkeyHex.slice(0, 16)}...`);

      // ===== STEP 2: Configure TS to accept the Rust peer =====
      // Wait for worker adapter to be initialized, then set follows for peer classification
      // Use window-exposed getters to avoid Vite module duplication issues
      console.log('[TS] Waiting for worker adapter and configuring...');
      const configResult = await page.evaluate(async ({ rustPubkey, localRelay }) => {
        // Use window-exposed getter (from testHelpers.ts) to get the worker adapter
        const getWorkerAdapter = (window as any).__getWorkerAdapter;
        const settingsStore = (window as any).__settingsStore;

        if (!getWorkerAdapter) {
          return { success: false, reason: '__getWorkerAdapter not exposed on window' };
        }

        // Wait up to 10s for worker adapter to be initialized
        let adapter = getWorkerAdapter();
        let retries = 0;
        while (!adapter && retries < 50) {
          await new Promise(r => setTimeout(r, 200));
          adapter = getWorkerAdapter();
          retries++;
        }

        if (!adapter) {
          return { success: false, reason: 'no workerAdapter after 10s' };
        }

        console.log('[TS] Worker adapter initialized after', retries * 200, 'ms');

        try {
          // 1. Set follows to include the Rust pubkey for WebRTC peer classification
          await adapter.setFollows([rustPubkey]);
          console.log('[TS] Set follows to include Rust:', rustPubkey.slice(0, 16));

          // Small delay to ensure worker processed setFollows
          await new Promise(r => setTimeout(r, 100));

          // 2. Broadcast hello to trigger peer discovery with updated follows
          await adapter.sendHello();
          console.log('[TS] Hello broadcasted');

          // 3. Update settings for future store creations
          if (settingsStore) {
            settingsStore.setNetworkSettings({ relays: [localRelay] });
          }

          return { success: true };
        } catch (e) {
          return { success: false, reason: String(e) };
        }
      }, { rustPubkey: rustKeys.pubkeyHex, localRelay });
      console.log('[TS] Config result:', configResult);

      // ===== STEP 3: Start Rust server with TS in follows =====
      console.log('[Rust] Starting hashtree-rs server...');

      // Pass both keys via environment - Rust uses its key and follows TS
      // Also pass local relay URL for deterministic signaling
      rustProcess = spawn('cargo', [
        'test', '--package', 'hashtree-cli', '--features', 'p2p', '--test', 'crosslang_peer',
        '--', '--nocapture', '--test-threads=1', '--ignored'
      ], {
        cwd: HASHTREE_RS_DIR,
        env: {
          ...process.env,
          RUST_LOG: 'debug',
          CROSSLANG_SECRET_KEY: bytesToHex(rustKeys.secretKey),
          CROSSLANG_FOLLOW_PUBKEY: tsPubkeyHex,
          LOCAL_RELAY: localRelay,
          CROSSLANG_PORT: String(crosslangPort),
        },
        stdio: ['ignore', 'pipe', 'pipe'],
      });

      // Log browser console for Rust peer events
      page.on('console', msg => {
        const text = msg.text();
        if (text.includes('WebRTC') || text.includes('Peer') ||
            text.includes('connected') || text.includes('Connection')) {
          console.log(`[TS] ${text}`);
        }
      });

      // Capture Rust output
      const rustOutputHandler = (data: Buffer) => {
        const text = data.toString();
        for (const line of text.split('\n')) {
          const hashMatch = line.match(/CROSSLANG_HASH:([a-f0-9]{64})/);
          if (hashMatch) contentHash = hashMatch[1];

          // Log relay connections, hello sends, and crosslang markers
          if (line.includes('CROSSLANG_') || line.includes('Peers:') || line.includes('connected') || line.includes('[Peer') || line.includes('Received') || line.includes('store') ||
              line.includes('relay') || line.includes('hello') || line.includes('Subscribed') || line.includes('Connecting') || line.includes('[handle_')) {
            console.log(`[Rust] ${line.trim()}`);
          }
        }
      };

      rustProcess.stdout?.on('data', rustOutputHandler);
      rustProcess.stderr?.on('data', rustOutputHandler);

      // Wait for Rust server to output the content hash
      await new Promise<void>((resolve, reject) => {
        const timeout = setTimeout(() => reject(new Error('Rust server timeout')), 60000);
        const check = setInterval(() => {
          if (contentHash) {
            clearInterval(check);
            clearTimeout(timeout);
            resolve();
          }
        }, 500);
      });

      console.log(`[Rust] Ready! Content hash: ${contentHash!.slice(0, 16)}...`);

      // ===== STEP 4: Wait for WebRTC connection =====
      console.log('[TS] Waiting for WebRTC connection to Rust peer...');

      let connectedToRust = false;
      for (let i = 0; i < 30; i++) {
        await page.waitForTimeout(2000);

        // Periodically broadcast hello to ensure peer discovery
        if (i % 5 === 0) {
          await page.evaluate(async () => {
            const adapter = (window as any).__getWorkerAdapter?.();
            if (adapter?.sendHello) await adapter.sendHello();
          });
        }

        const peerInfo = await page.evaluate(async (rustPk) => {
          // Get peers directly from worker adapter (not UI store which may be stale)
          const adapter = (window as any).__getWorkerAdapter?.();
          if (!adapter) return { total: 0, connected: 0, rustPeer: null, allPeers: [] };
          const peers = await adapter.getPeerStats?.() || [];
          const rustPeer = peers.find((p: any) => p.pubkey === rustPk);
          return {
            total: peers.length,
            connected: peers.filter((p: any) => p.connected).length,
            rustPeer: rustPeer ? { state: rustPeer.connected ? 'connected' : 'disconnected', pool: rustPeer.pool } : null,
            allPeers: peers.map((p: any) => ({ pk: p.pubkey?.slice(0, 16), state: p.connected ? 'connected' : 'disconnected', pool: p.pool })),
          };
        }, rustKeys.pubkeyHex);

        console.log(`[TS] Check ${i + 1}: ${peerInfo.connected}/${peerInfo.total} peers, Rust: ${JSON.stringify(peerInfo.rustPeer)}, all: ${JSON.stringify(peerInfo.allPeers)}`);

        if (peerInfo.rustPeer?.state === 'connected') {
          connectedToRust = true;
          console.log('[TS] Connected to Rust peer!');
          break;
        }
      }

      // ===== STEP 5: Request content via WebRTC =====
      console.log(`[TS] Requesting content: ${contentHash!.slice(0, 16)}...`);

      const content = await page.evaluate(async (hashHex) => {
        // Use window-exposed getter to get the actual store instance
        const getWebRTCStore = (window as any).__getWebRTCStore;
        const webrtcStore = getWebRTCStore?.();
        if (!webrtcStore?.get) return null;

        // Convert hex string to Uint8Array
        const hexToBytes = (hex: string): Uint8Array => {
          const bytes = new Uint8Array(hex.length / 2);
          for (let i = 0; i < bytes.length; i++) {
            bytes[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
          }
          return bytes;
        };
        const hash = hexToBytes(hashHex);

        try {
          const result = await Promise.race([
            webrtcStore.get(hash),
            new Promise<null>(r => setTimeout(() => r(null), 15000)),
          ]);
          if (result) {
            return { source: 'webrtc', data: new TextDecoder().decode(result as Uint8Array) };
          }
        } catch (e) {
          console.log('WebRTC get error:', e);
        }
        return null;
      }, contentHash);

      console.log('[TS] Content result:', content);

      // ===== VERIFY =====
      if (content) {
        console.log(`\n=== SUCCESS: Content synced via WebRTC! ===`);
        console.log(`Content: ${content.data}`);
        expect(content.data).toContain('Hello from hashtree-rs');
      } else {
        console.log('\n=== WebRTC sync failed ===');
        console.log(`Connected to Rust: ${connectedToRust}`);
      }

      expect(content).not.toBeNull();
      expect(content?.source).toBe('webrtc');

    } finally {
      if (rustProcess) {
        rustProcess.kill();
      }
      if (lockFd !== null) {
        releaseHashtreeRsLock(lockFd);
      }
    }
  });
});
