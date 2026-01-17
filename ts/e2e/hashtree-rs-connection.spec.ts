/**
 * Cross-language WebRTC CONNECTION test: hashtree-ts (browser) <-> hashtreeRs (Rust)
 *
 * This test verifies actual WebRTC data channel connections, not just discovery.
 * Uses Playwright to run hashtree-ts WebRTCStore in a browser while hashtreeRs runs.
 */

import { test, expect } from './fixtures';
import { spawn, execSync, type ChildProcess } from 'child_process';
import fs from 'fs';
import path from 'path';
import { enableOthersPool, ensureLoggedIn, setupPageErrorHandler, useLocalRelay, waitForAppReady, getTestRelayUrl, getCrosslangPort } from './test-utils.js';
import { acquireHashtreeRsLock, releaseHashtreeRsLock } from './hashtree-rs-lock.js';
import { generateSecretKey, getPublicKey } from 'nostr-tools';

const HASHTREE_RS_DIR = '/workspace/hashtree-rs';
const hashtreeRsAvailable = (() => {
  try {
    execSync('cargo --version', { stdio: 'ignore' });
  } catch {
    return false;
  }
  return fs.existsSync(path.join(HASHTREE_RS_DIR, 'Cargo.toml'));
})();

function ensureHtreeBinary(): void {
  const debugBin = path.join(HASHTREE_RS_DIR, 'target/debug/htree');
  const releaseBin = path.join(HASHTREE_RS_DIR, 'target/release/htree');

  if (fs.existsSync(debugBin) || fs.existsSync(releaseBin)) {
    return;
  }

  console.log('Building htree binary for hashtree-rs tests...');
  execSync('cargo build --bin htree --features p2p', { cwd: HASHTREE_RS_DIR, stdio: 'inherit' });

  if (!fs.existsSync(debugBin) && !fs.existsSync(releaseBin)) {
    throw new Error('htree binary build completed, but binary not found');
  }
}

function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes).map(b => b.toString(16).padStart(2, '0')).join('');
}

test.describe('hashtreeRs WebRTC Connection', () => {
  test.skip(!hashtreeRsAvailable, 'hashtree-rs repo or Rust toolchain not available');
  test.setTimeout(420000);

  test('hashtree-ts and hashtreeRs establish WebRTC connection', async ({ page }, testInfo) => {
    ensureHtreeBinary();
    const localRelay = getTestRelayUrl();
    const crosslangPort = getCrosslangPort(testInfo.workerIndex);

    setupPageErrorHandler(page);
    await page.goto('/');
    await waitForAppReady(page);
    await ensureLoggedIn(page, 20000);
    await useLocalRelay(page);
    await enableOthersPool(page, 2);

    // Wait for WebRTC test helpers to be initialized
    await page.waitForFunction(() => typeof (window as any).runWebRTCTest === 'function', { timeout: 10000 });

    const tsPubkey = await page.evaluate(() => (window as any).__nostrStore?.getState?.()?.pubkey || null);
    expect(tsPubkey).toBeTruthy();

    let hashtreeRsProcess: ChildProcess | null = null;
    let lockFd: number | null = null;
    const rustSecretKey = generateSecretKey();
    const hashtreeRsPubkey = getPublicKey(rustSecretKey);
    const rustSecretHex = bytesToHex(rustSecretKey);
    let testContentHash: string | null = null;
    let rustReady = false;
    const hashtreeRsConnectedPeers = new Set<string>();
    const outputLines: string[] = [];

    try {
      lockFd = await acquireHashtreeRsLock(240000);
      const followResult = await page.evaluate(async (rustPubkey) => {
        const getWorkerAdapter = (window as any).__getWorkerAdapter;
        if (!getWorkerAdapter) return { ok: false, reason: 'no __getWorkerAdapter' };

        let adapter = getWorkerAdapter();
        let retries = 0;
        while (!adapter && retries < 50) {
          await new Promise(r => setTimeout(r, 200));
          adapter = getWorkerAdapter();
          retries++;
        }
        if (!adapter?.setFollows) return { ok: false, reason: 'no worker adapter' };

        await adapter.setFollows([rustPubkey]);
        await adapter.sendHello?.();
        return { ok: true, retries };
      }, hashtreeRsPubkey);

      expect(followResult.ok).toBe(true);

      console.log('Starting hashtreeRs peer...');
      hashtreeRsProcess = spawn(
        'cargo',
        [
          'test',
          '--package',
          'hashtree-cli',
          '--features',
          'p2p',
          '--test',
          'crosslang_peer',
          '--',
          '--nocapture',
          '--ignored',
          '--test-threads=1',
        ],
        {
          cwd: HASHTREE_RS_DIR,
          env: {
            ...process.env,
            RUST_LOG: 'warn,hashtree_cli::webrtc=info',
            CROSSLANG_SECRET_KEY: rustSecretHex,
            CROSSLANG_FOLLOW_PUBKEY: tsPubkey,
            LOCAL_RELAY: localRelay,
            CROSSLANG_PORT: String(crosslangPort),
          },
          stdio: ['ignore', 'pipe', 'pipe'],
        }
      );

      const outputHandler = (data: Buffer) => {
        const text = data.toString();
        for (const line of text.split('\n')) {
          if (line.trim()) {
            outputLines.push(line.trim());
            if (outputLines.length > 200) outputLines.shift();
          }

          const hashMatch = line.match(/CROSSLANG_HASH:([a-f0-9]{64})/);
          if (hashMatch) {
            testContentHash = hashMatch[1];
            console.log(`[hashtreeRs] Test content hash: ${testContentHash.slice(0, 16)}...`);
          }

          if (line.includes('CROSSLANG_READY')) {
            rustReady = true;
          }

          const connectedMatch = line.match(/CROSSLANG_CONNECTED:([a-f0-9]{64})/);
          if (connectedMatch) {
            hashtreeRsConnectedPeers.add(connectedMatch[1]);
            console.log(`[hashtreeRs] CONNECTED to peer: ${connectedMatch[1].slice(0, 16)}...`);
          }

          if (line.includes('CROSSLANG_')) {
            console.log(`[hashtreeRs] ${line.trim()}`);
          }
        }
      };

      hashtreeRsProcess.stdout?.on('data', outputHandler);
      hashtreeRsProcess.stderr?.on('data', outputHandler);

      const exitPromise = new Promise<never>((_, reject) => {
        hashtreeRsProcess?.on('exit', (code, signal) => {
          reject(new Error(`hashtree-rs exited before ready (code=${code}, signal=${signal}). Recent output:\n${outputLines.join('\n')}`));
        });
      });

      await Promise.race([
        new Promise<void>((resolve, reject) => {
          const timeout = setTimeout(() => reject(new Error(`Timed out waiting for hashtree-rs markers. Recent output:\n${outputLines.join('\n')}`)), 240000);
          const check = setInterval(() => {
            if (testContentHash && rustReady) {
              clearInterval(check);
              clearTimeout(timeout);
              resolve();
            }
          }, 500);
        }),
        exitPromise,
      ]);

      let connectedToRust = false;
      for (let i = 0; i < 60; i++) {
        await page.waitForTimeout(2000);
        if (i % 3 === 0) {
          await page.evaluate(async () => {
            const adapter = (window as any).__getWorkerAdapter?.();
            await adapter?.sendHello?.();
          });
        }

        const peerInfo = await page.evaluate(async (rustPubkey) => {
          const adapter = (window as any).__getWorkerAdapter?.();
          if (!adapter?.getPeerStats) return { connected: false, peers: [] as any[] };
          const peers = await adapter.getPeerStats();
          const rustPeer = peers.find((p: { pubkey?: string }) => p.pubkey === rustPubkey);
          return {
            connected: !!rustPeer?.connected,
            peers: peers.map((p: { pubkey?: string; connected?: boolean; pool?: string }) => ({
              pk: p.pubkey?.slice(0, 16),
              connected: p.connected,
              pool: p.pool,
            })),
          };
        }, hashtreeRsPubkey);

        console.log(`[TS] Check ${i + 1}: rust connected=${peerInfo.connected}, peers=${JSON.stringify(peerInfo.peers)}`);
        if (peerInfo.connected) {
          connectedToRust = true;
          break;
        }
      }

      expect(connectedToRust).toBe(true);

      const content = await page.evaluate(async (hashHex) => {
        const getWebRTCStore = (window as any).__getWebRTCStore;
        const webrtcStore = getWebRTCStore?.();
        if (!webrtcStore?.get) return null;

        const hexToBytes = (hex: string): Uint8Array => {
          const bytes = new Uint8Array(hex.length / 2);
          for (let i = 0; i < bytes.length; i++) {
            bytes[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
          }
          return bytes;
        };

        const hashBytes = hexToBytes(hashHex);
        for (let attempt = 0; attempt < 3; attempt++) {
          const result = await Promise.race([
            webrtcStore.get(hashBytes),
            new Promise<null>(resolve => setTimeout(() => resolve(null), 15000)),
          ]);
          if (result) {
            return new TextDecoder().decode(result as Uint8Array);
          }
          await new Promise(r => setTimeout(r, 1000));
        }
        return null;
      }, testContentHash);

      console.log('hashtreeRs connected peers:', Array.from(hashtreeRsConnectedPeers));
      expect(content).toBeTruthy();
      expect(content).toContain('Hello from hashtree-rs');
    } finally {
      if (hashtreeRsProcess) {
        hashtreeRsProcess.kill('SIGTERM');
        await new Promise(r => setTimeout(r, 500));
        hashtreeRsProcess.kill('SIGKILL');
      }
      if (lockFd !== null) {
        releaseHashtreeRsLock(lockFd);
      }
    }
  });
});
