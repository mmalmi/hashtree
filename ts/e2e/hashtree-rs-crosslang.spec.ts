/**
 * Cross-language E2E test: hashtree-ts (browser) <-> hashtree-rs (Rust)
 *
 * Runs a hashtree-rs WebRTC manager in background and verifies that
 * hashtree-ts running in a browser can discover and connect to it.
 */

import { test, expect } from './fixtures';
import { getCrosslangPort } from './test-utils';
import WebSocket from 'ws';
import { SimplePool, finalizeEvent, generateSecretKey, getPublicKey, type Event, nip44 } from 'nostr-tools';
import { spawn, execSync, type ChildProcess } from 'child_process';
import fs from 'fs';
import path from 'path';
import { acquireHashtreeRsLock, releaseHashtreeRsLock } from './hashtree-rs-lock.js';

// Polyfill WebSocket for Node.js
(globalThis as any).WebSocket = WebSocket;

const WEBRTC_KIND = 25050;
const HELLO_TAG = 'hello';
const HASHTREE_RS_DIR = '/workspace/hashtree-rs';
const tsSecretKey = generateSecretKey();
const tsPubkey = getPublicKey(tsSecretKey);
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

function generateUuid(): string {
  return Math.random().toString(36).substring(2, 15) +
    Math.random().toString(36).substring(2, 15);
}

async function publishWithRetry(
  pool: SimplePool,
  relayUrl: string,
  event: Event,
  attempts = 5
): Promise<boolean> {
  let lastError: unknown;
  for (let attempt = 0; attempt < attempts; attempt++) {
    try {
      const relay = await pool.ensureRelay(relayUrl);
      relay.connectionTimeout = 15000;
      relay.publishTimeout = 15000;
      await relay.connect();
      await Promise.any(pool.publish([relayUrl], event));
      return true;
    } catch (err) {
      lastError = err;
      await new Promise(r => setTimeout(r, 1000));
    }
  }
  console.log('Publish failed:', lastError);
  return false;
}

function unwrapGiftContent(
  secretKey: Uint8Array,
  ephemeralPubkey: string,
  ciphertext: string
): { content: string; senderPubkey: string } | null {
  try {
    const conversationKey = nip44.v2.utils.getConversationKey(secretKey, ephemeralPubkey);
    const plaintext = nip44.v2.decrypt(ciphertext, conversationKey);
    const seal = JSON.parse(plaintext) as { content?: string; pubkey?: string };
    if (typeof seal.content !== 'string') return null;
    const senderPubkey = typeof seal.pubkey === 'string' ? seal.pubkey : ephemeralPubkey;
    return { content: seal.content, senderPubkey };
  } catch {
    return null;
  }
}

test.describe('hashtree-rs Cross-Language', () => {
  test.setTimeout(360000);
  test.describe.configure({ mode: 'serial', timeout: 360000 });
  test.skip(!hashtreeRsAvailable, 'hashtree-rs repo or Rust toolchain not available');

  let rsPeerProcess: ChildProcess | null = null;
  let rsPeerPubkey: string | null = null;
  let lockFd: number | null = null;
  const outputLines: string[] = [];
  let localRelay = '';
  let crosslangPort = 0;
  let rsReady = false;

  test.beforeAll(async ({ relayUrl }, testInfo) => {
    testInfo.setTimeout(360000);
    // Start hashtree-rs crosslang test in background
    console.log('Starting hashtree-rs peer...');
    ensureHtreeBinary();
    localRelay = relayUrl;
    crosslangPort = getCrosslangPort(testInfo.workerIndex);
    lockFd = await acquireHashtreeRsLock(240000);
    try {
      rsPeerProcess = spawn(
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
            RUST_LOG: 'warn',
            LOCAL_RELAY: localRelay,
            CROSSLANG_FOLLOW_PUBKEY: tsPubkey,
            CROSSLANG_PORT: String(crosslangPort),
          },
          stdio: ['ignore', 'pipe', 'pipe'],
        }
      );
    } catch (err) {
      if (lockFd !== null) {
        releaseHashtreeRsLock(lockFd);
        lockFd = null;
      }
      throw err;
    }

    // Capture hashtree-rs pubkey and ready marker
    const pubkeyPromise = new Promise<string>((resolve, reject) => {
      const handler = (data: Buffer) => {
        const lines = data.toString().split('\n');
        for (const line of lines) {
          if (line.trim()) {
            outputLines.push(line.trim());
            if (outputLines.length > 200) outputLines.shift();
          }
          const match = line.match(/CROSSLANG_PUBKEY:([a-f0-9]{64})/);
          if (match) {
            resolve(match[1]);
          }
          if (line.includes('CROSSLANG_READY')) {
            rsReady = true;
          }
        }
      };
      rsPeerProcess!.stdout?.on('data', handler);
      rsPeerProcess!.stderr?.on('data', handler);

      rsPeerProcess!.on('exit', (code, signal) => {
        reject(new Error(`hashtree-rs exited before ready (code=${code}, signal=${signal}). Recent output:\n${outputLines.join('\n')}`));
      });
    });

    // Wait for pubkey with timeout
    rsPeerPubkey = await Promise.race([
      pubkeyPromise,
      new Promise<string>((_, reject) => setTimeout(() => reject(new Error(`Timeout waiting for hashtree-rs pubkey. Recent output:\n${outputLines.join('\n')}`)), 30000))
    ]).catch(() => null);

    if (rsPeerPubkey) {
      console.log(`hashtree-rs pubkey: ${rsPeerPubkey.slice(0, 16)}...`);
    } else {
      console.log('Warning: Could not capture hashtree-rs pubkey');
    }

    await Promise.race([
      new Promise<void>((resolve, reject) => {
        const start = Date.now();
        const interval = setInterval(() => {
          if (rsReady) {
            clearInterval(interval);
            resolve();
          } else if (Date.now() - start > 90000) {
            clearInterval(interval);
            reject(new Error(`Timeout waiting for hashtree-rs ready. Recent output:\n${outputLines.join('\n')}`));
          }
        }, 500);
      }),
      new Promise<void>((_, reject) => {
        rsPeerProcess!.once('exit', (code, signal) => {
          reject(new Error(`hashtree-rs exited before ready (code=${code}, signal=${signal}). Recent output:\n${outputLines.join('\n')}`));
        });
      }),
    ]);
  });

  test.afterAll(async () => {
    if (rsPeerProcess) {
      rsPeerProcess.kill();
      rsPeerProcess = null;
    }
    if (lockFd !== null) {
      releaseHashtreeRsLock(lockFd);
    }
  });

  test('hashtree-ts discovers hashtree-rs peer via relay', async () => {
    if (!rsPeerPubkey) {
      throw new Error('hashtree-rs pubkey not captured');
    }

    const pool = new SimplePool();

    // Generate keys for TypeScript peer
    const tsSk = tsSecretKey;
    const tsPk = tsPubkey;
    const tsUuid = generateUuid();

    console.log('TypeScript peer pubkey:', tsPk.slice(0, 16) + '...');

    const discoveredPeers = new Map<string, any>();
    let foundRsPeer = false;
    let receivedOfferFromRsPeer = false;

    const since = Math.floor(Date.now() / 1000) - 60;
    const sub = pool.subscribe(
      [localRelay],
      [
        {
          kinds: [WEBRTC_KIND],
          '#l': [HELLO_TAG],
          since,
        },
        {
          kinds: [WEBRTC_KIND],
          '#p': [tsPk],
          since,
        },
      ],
      {
        onevent(event: Event) {
          if (event.pubkey === tsPk) return;

          const isHello = event.tags.some((t) => t[0] === 'l' && t[1] === HELLO_TAG);
          if (isHello) {
            const peerIdTag = event.tags.find((t) => t[0] === 'peerId');
            const peerId = peerIdTag?.[1];
            if (!peerId) return;

            if (!discoveredPeers.has(event.pubkey)) {
              discoveredPeers.set(event.pubkey, { peerId });
              console.log(`Discovered: ${event.pubkey.slice(0, 16)}... peerId=${peerId.slice(0, 12)}`);

              if (rsPeerPubkey && event.pubkey === rsPeerPubkey) {
                foundRsPeer = true;
                console.log(`*** FOUND HASHTREE-RS PEER! peerId=${peerId} ***`);
              }
            }
            return;
          }

          try {
            let content = event.content;
            if (!content) return;

            let senderPubkey = event.pubkey;
            if (!content.startsWith('{')) {
              const unwrapped = unwrapGiftContent(tsSk, event.pubkey, content);
              if (!unwrapped) {
                return;
              }
              content = unwrapped.content;
              senderPubkey = unwrapped.senderPubkey;
            }

            const msg = JSON.parse(content);

            if (msg.type === 'offer') {
              const target = typeof msg.targetPeerId === 'string' ? msg.targetPeerId : msg.recipient;
              const recipientPk = typeof target === 'string' ? target.split(':')[0] : null;
              if (recipientPk === tsPk) {
                console.log(`Received OFFER from ${senderPubkey.slice(0, 16)}...`);
                if (rsPeerPubkey && senderPubkey === rsPeerPubkey) {
                  receivedOfferFromRsPeer = true;
                  console.log('*** RECEIVED OFFER FROM HASHTREE-RS! ***');
                }
              }
            }
          } catch {
            // Ignore parse errors
          }
        },
      }
    );

    await new Promise(r => setTimeout(r, 1000));

    // Send hellos and wait for discovery
    for (let i = 0; i < 15; i++) {
      const expiration = Math.floor((Date.now() + 5 * 60 * 1000) / 1000);
      const helloEvent = finalizeEvent({
        kind: WEBRTC_KIND,
        created_at: Math.floor(Date.now() / 1000),
        tags: [
          ['l', HELLO_TAG],
          ['peerId', tsUuid],
          ['expiration', expiration.toString()],
        ],
        content: '',
      }, tsSk);

      await publishWithRetry(pool, localRelay, helloEvent);
      await new Promise(r => setTimeout(r, 2000));

      console.log(`Check ${i + 1}: Discovered ${discoveredPeers.size} peers, foundRsPeer=${foundRsPeer}`);

      // Success if we found hashtree-rs or received an offer from it
      if (foundRsPeer || receivedOfferFromRsPeer) {
        break;
      }
    }

    // Cleanup
    sub.close();
    pool.close([localRelay]);

    console.log('\n=== Results ===');
    console.log(`Peers discovered: ${discoveredPeers.size}`);
    console.log(`Found hashtree-rs: ${foundRsPeer}`);
    console.log(`Received offer from hashtree-rs: ${receivedOfferFromRsPeer}`);

    // Verify hashtree-rs's peerId was correctly received
    if (rsPeerPubkey && foundRsPeer) {
      const rsPeer = discoveredPeers.get(rsPeerPubkey);
      console.log(`hashtree-rs peerId: ${rsPeer?.peerId}`);
      expect(rsPeer?.peerId).toBeTruthy();
      expect(typeof rsPeer?.peerId).toBe('string');
      expect(rsPeer?.peerId.length).toBeGreaterThan(5);
    }

    expect(foundRsPeer || receivedOfferFromRsPeer).toBe(true);
  });

  test('hashtree-rs responds to hashtree-ts peer via relay', async () => {
    if (!rsPeerPubkey) {
      throw new Error('hashtree-rs pubkey not captured');
    }

    const pool = new SimplePool();
    const tsSk = tsSecretKey;
    const tsPk = tsPubkey;
    // Use a high UUID so hashtree-rs (lower UUID) initiates the offer.
    const tsUuid = 'z'.repeat(30);

    console.log('TypeScript peer pubkey:', tsPk.slice(0, 16) + '...');
    console.log('TypeScript peer UUID:', tsUuid);

    let receivedOfferFromRs = false;

    const since = Math.floor(Date.now() / 1000) - 30;
    const sub = pool.subscribe(
      [localRelay],
      [
        {
          kinds: [WEBRTC_KIND],
          '#l': [HELLO_TAG],
          since,
        },
        {
          kinds: [WEBRTC_KIND],
          '#p': [tsPk],
          since,
        },
      ],
      {
        onevent(event: Event) {
          if (event.pubkey === tsPk) return;
          try {
            let content = event.content;
            if (!content) return;

            let senderPubkey = event.pubkey;
            if (!content.startsWith('{')) {
              const unwrapped = unwrapGiftContent(tsSk, event.pubkey, content);
              if (!unwrapped) {
                return;
              }
              content = unwrapped.content;
              senderPubkey = unwrapped.senderPubkey;
            }
            const msg = JSON.parse(content);
            if (msg.type === 'offer') {
              const target = typeof msg.targetPeerId === 'string' ? msg.targetPeerId : msg.recipient;
              const recipientPk = typeof target === 'string' ? target.split(':')[0] : null;
              if (recipientPk === tsPk && senderPubkey === rsPeerPubkey) {
                receivedOfferFromRs = true;
                console.log('*** RECEIVED OFFER FROM HASHTREE-RS ***');
              }
            }
          } catch {
            // Ignore parse errors
          }
        },
      }
    );

    await new Promise(r => setTimeout(r, 1000));

    // Send hellos to prompt an offer from hashtree-rs
    for (let i = 0; i < 30; i++) {
      const expiration = Math.floor((Date.now() + 5 * 60 * 1000) / 1000);
      const helloEvent = finalizeEvent({
        kind: WEBRTC_KIND,
        created_at: Math.floor(Date.now() / 1000),
        tags: [
          ['l', HELLO_TAG],
          ['peerId', tsUuid],
          ['expiration', expiration.toString()],
        ],
        content: '',
      }, tsSk);

      await publishWithRetry(pool, localRelay, helloEvent);
      await new Promise(r => setTimeout(r, 2000));

      console.log(`Check ${i + 1}: received offer from hashtree-rs = ${receivedOfferFromRs}`);

      if (receivedOfferFromRs) {
        break;
      }
    }

    sub.close();
    pool.close([localRelay]);

    console.log('\n=== Results ===');
    console.log(`Received offer from hashtree-rs: ${receivedOfferFromRs}`);

    expect(receivedOfferFromRs).toBe(true);
  });
});
