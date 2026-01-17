/**
 * Cross-language WebSocket integration test: hashtree-ts <-> hashtree-rs
 *
 * Tests that the TypeScript WebSocketPeer can communicate with the
 * Rust hashtree-rs /ws/data endpoint using the shared protocol:
 * - JSON messages: req, res
 * - Binary messages: [4-byte LE request_id][data]
 */

import { test, expect } from './fixtures';
import { spawn, ChildProcess, execSync } from 'child_process';
import fs from 'fs';
import path from 'path';
import WebSocket from 'ws';
import { createHash } from 'crypto';
import { acquireHashtreeRsLock, releaseHashtreeRsLock } from './hashtree-rs-lock.js';

// Polyfill WebSocket for Node.js environment
(globalThis as any).WebSocket = WebSocket;

const RUST_SERVER_PORT = 18787;
const RUST_SERVER_URL = `ws://127.0.0.1:${RUST_SERVER_PORT}/ws/data`;
const HTTP_URL = `http://127.0.0.1:${RUST_SERVER_PORT}`;

/**
 * Simple WebSocket client that speaks the hashtree protocol
 * (mirrors wsPeer.ts but simplified for testing)
 */
class TestWsClient {
  private ws: WebSocket | null = null;
  private pendingRequests = new Map<number, {
    hash: string;
    resolve: (data: Uint8Array | null) => void;
    timeout: ReturnType<typeof setTimeout>;
  }>();
  private nextRequestId = 1;
  private name: string;

  constructor(name = 'TestWsClient') {
    this.name = name;
  }

  async connect(url: string): Promise<boolean> {
    return new Promise((resolve) => {
      try {
        this.ws = new WebSocket(url);
        this.ws.binaryType = 'arraybuffer';

        const timeout = setTimeout(() => {
          this.ws?.close();
          resolve(false);
        }, 5000);

        this.ws.onopen = () => {
          clearTimeout(timeout);
          console.log(`[${this.name}] Connected`);
          resolve(true);
        };

        this.ws.onerror = (err) => {
          clearTimeout(timeout);
          console.log(`[${this.name}] Error:`, err);
          resolve(false);
        };

        this.ws.onmessage = (event) => {
          if (typeof event.data === 'string') {
            this.handleJsonMessage(event.data);
          } else if (event.data instanceof ArrayBuffer) {
            this.handleBinaryMessage(event.data);
          }
        };
      } catch {
        resolve(false);
      }
    });
  }

  close(): void {
    if (this.ws) {
      this.ws.close();
      this.ws = null;
    }
    for (const pending of this.pendingRequests.values()) {
      clearTimeout(pending.timeout);
      pending.resolve(null);
    }
    this.pendingRequests.clear();
  }

  async request(hashHex: string, timeoutMs = 5000): Promise<Uint8Array | null> {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      return null;
    }

    const requestId = this.nextRequestId++;

    return new Promise((resolve) => {
      const timeout = setTimeout(() => {
        this.pendingRequests.delete(requestId);
        console.log(`[${this.name}] Request timeout for`, hashHex.slice(0, 16));
        resolve(null);
      }, timeoutMs);

      this.pendingRequests.set(requestId, { hash: hashHex, resolve, timeout });

      const msg = { type: 'req', id: requestId, hash: hashHex };
      this.ws!.send(JSON.stringify(msg));
      console.log(`[${this.name}] Sent request:`, msg);
    });
  }

  protected handleJsonMessage(data: string): void {
    try {
      const msg = JSON.parse(data);
      console.log(`[${this.name}] Received JSON:`, msg);

      if (msg.type === 'res') {
        const pending = this.pendingRequests.get(msg.id);
        if (pending && !msg.found) {
          clearTimeout(pending.timeout);
          this.pendingRequests.delete(msg.id);
          pending.resolve(null);
        }
        // If found=true, wait for binary data
      }
    } catch (err) {
      console.log(`[${this.name}] Error parsing JSON:`, err);
    }
  }

  protected handleBinaryMessage(data: ArrayBuffer): void {
    // Parse: [4-byte LE request_id][payload]
    const view = new DataView(data);
    const requestId = view.getUint32(0, true); // little-endian
    const payload = new Uint8Array(data, 4);

    console.log(`[${this.name}] Received binary:`, requestId, 'bytes:', payload.length);

    const pending = this.pendingRequests.get(requestId);
    if (!pending) return;

    clearTimeout(pending.timeout);
    this.pendingRequests.delete(requestId);

    // Verify hash
    const computedHash = createHash('sha256').update(payload).digest('hex');
    if (computedHash === pending.hash) {
      pending.resolve(payload);
    } else {
      console.log(`[${this.name}] Hash mismatch:`, computedHash, 'expected:', pending.hash);
      pending.resolve(null);
    }
  }

  protected getWs(): WebSocket | null {
    return this.ws;
  }

  protected getName(): string {
    return this.name;
  }
}

/**
 * WebSocket client that can also serve data (respond to requests)
 * Used to simulate a browser that has content and can serve it to others
 */
class ServingWsClient extends TestWsClient {
  // Local storage: hash -> data
  private localData = new Map<string, Uint8Array>();

  constructor(name = 'ServingClient') {
    super(name);
  }

  /**
   * Add data to local storage
   */
  addData(data: Uint8Array): string {
    const hash = createHash('sha256').update(data).digest('hex');
    this.localData.set(hash, data);
    console.log(`[${this.getName()}] Added data with hash:`, hash.slice(0, 16));
    return hash;
  }

  protected handleJsonMessage(data: string): void {
    try {
      const msg = JSON.parse(data);
      console.log(`[${this.getName()}] Received JSON:`, msg);

      if (msg.type === 'req') {
        // Server is forwarding a request to us - check if we have the data
        this.handleRequest(msg.id, msg.hash);
      } else if (msg.type === 'res') {
        // Response to our own request
        const pending = (this as any).pendingRequests.get(msg.id);
        if (pending && !msg.found) {
          clearTimeout(pending.timeout);
          (this as any).pendingRequests.delete(msg.id);
          pending.resolve(null);
        }
      }
    } catch (err) {
      console.log(`[${this.getName()}] Error parsing JSON:`, err);
    }
  }

  private handleRequest(id: number, hash: string): void {
    const ws = this.getWs();
    if (!ws || ws.readyState !== WebSocket.OPEN) return;

    const data = this.localData.get(hash);
    if (data) {
      console.log(`[${this.getName()}] Serving data for hash:`, hash.slice(0, 16));

      // Send found response
      ws.send(JSON.stringify({ type: 'res', id, hash, found: true }));

      // Send binary data: [4-byte LE id][data]
      const packet = new Uint8Array(4 + data.length);
      const view = new DataView(packet.buffer);
      view.setUint32(0, id, true); // little-endian
      packet.set(data, 4);
      ws.send(packet.buffer);
    } else {
      // Don't have it - stay silent, let server timeout and try next peer
      console.log(`[${this.getName()}] Don't have hash:`, hash.slice(0, 16), '(silent)');
    }
  }
}

test.describe('hashtree-rs WebSocket Integration', () => {
  // Serial mode: shares rust server process via beforeAll/afterAll
  test.describe.configure({ mode: 'serial', timeout: 180000 });
  test.setTimeout(180000);

  let rustProcess: ChildProcess | null = null;
  let tempDir: string | null = null;
  let configDir: string | null = null;
  let lockFd: number | null = null;

  test.beforeAll(async () => {
    test.setTimeout(180000);
    // Check if hashtree-rs binary exists (skip tests if not built)
    try {
      execSync('cargo metadata --manifest-path /workspace/hashtree-rs/Cargo.toml', { stdio: 'ignore' });
    } catch {
      console.log('hashtree-rs not available, skipping ws-interop tests');
      test.skip();
      return;
    }

    lockFd = await acquireHashtreeRsLock(240000);

    // Create temp directory for Rust server storage
    tempDir = execSync('mktemp -d').toString().trim();
    console.log('Temp directory:', tempDir);

    // Create isolated config dir with auth disabled
    configDir = execSync('mktemp -d').toString().trim();
    const configPath = path.join(configDir, 'config.toml');
    fs.writeFileSync(configPath, '[server]\nenable_auth = false\n', 'utf8');

    // Start hashtree-rs server
    console.log('Starting hashtree-rs server...');
    rustProcess = spawn(
      'cargo',
      ['run', '-p', 'hashtree-cli', '--release', '--', 'start', '--addr', `127.0.0.1:${RUST_SERVER_PORT}`, '--data-dir', tempDir],
      {
        cwd: '/workspace/hashtree-rs',
        env: { ...process.env, RUST_LOG: 'hashtree_cli=debug', HTREE_CONFIG_DIR: configDir },
        stdio: ['ignore', 'pipe', 'pipe'],
      }
    );

    // Log output
    rustProcess.stdout?.on('data', (data) => {
      console.log('[hashtree-rs stdout]', data.toString().trim());
    });
    rustProcess.stderr?.on('data', (data) => {
      console.log('[hashtree-rs stderr]', data.toString().trim());
    });

    // Wait for server to start (longer timeout for initial compilation)
    let serverReady = false;
    for (let i = 0; i < 60; i++) {
      await new Promise(r => setTimeout(r, 1000));
      try {
        const response = await fetch(`${HTTP_URL}/api/stats`);
        if (response.ok) {
          serverReady = true;
          console.log('hashtree-rs server is ready');
          break;
        }
      } catch {
        // Server not ready yet
      }
    }

    if (!serverReady) {
      throw new Error('hashtree-rs server failed to start within 60 seconds');
    }
  });

  test.afterAll(async () => {
    if (rustProcess) {
      rustProcess.kill('SIGTERM');
      await new Promise(r => setTimeout(r, 500));
      rustProcess.kill('SIGKILL');
    }
    if (lockFd !== null) {
      releaseHashtreeRsLock(lockFd);
      lockFd = null;
    }
    if (tempDir) {
      try {
        execSync(`rm -rf "${tempDir}"`);
      } catch {
        // Ignore cleanup errors
      }
    }
    if (configDir) {
      try {
        execSync(`rm -rf "${configDir}"`);
      } catch {
        // Ignore cleanup errors
      }
    }
  });

  test('TypeScript WebSocketPeer can connect to hashtree-rs /ws/data', async () => {
    const client = new TestWsClient();
    const connected = await client.connect(RUST_SERVER_URL);
    expect(connected).toBe(true);
    client.close();
  });

  test('receives not-found response for missing content via WebSocket protocol', async () => {
    // This test verifies that the protocol works correctly:
    // - Client can send JSON request with { type: 'req', id, hash }
    // - Server responds with { type: 'res', id, hash, found: false } for missing content

    const client = new TestWsClient();
    const connected = await client.connect(RUST_SERVER_URL);
    expect(connected).toBe(true);

    // Request a hash that doesn't exist
    const nonExistentHash = 'deadbeef'.repeat(8); // 64 char hex
    console.log('Requesting non-existent hash:', nonExistentHash.slice(0, 16));

    const data = await client.request(nonExistentHash, 2000);
    // Should return null (not found) - this verifies the protocol is working
    expect(data).toBeNull();

    client.close();
  });

  test('can send multiple requests in sequence', async () => {
    // Verify the protocol handles multiple sequential requests correctly
    const client = new TestWsClient();
    const connected = await client.connect(RUST_SERVER_URL);
    expect(connected).toBe(true);

    // Send multiple requests - all should get "not found" responses
    const hashes = [
      'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
      'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
      'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc',
    ];

    for (const hash of hashes) {
      const data = await client.request(hash, 2000);
      expect(data).toBeNull(); // All should be not found
    }

    client.close();
  });

  test('JSON protocol format is correct', async () => {
    // Verify the JSON message format works correctly
    const ws = new WebSocket(RUST_SERVER_URL);
    ws.binaryType = 'arraybuffer';

    await new Promise<void>((resolve, reject) => {
      ws.onopen = () => resolve();
      ws.onerror = () => reject(new Error('Failed to connect'));
      setTimeout(() => reject(new Error('Connection timeout')), 5000);
    });

    // Send a request and verify we get a properly formatted response
    const responsePromise = new Promise<{ type: string; id: number; hash: string; found: boolean }>((resolve, reject) => {
      const timeout = setTimeout(() => reject(new Error('Response timeout')), 5000);

      ws.onmessage = (event) => {
        if (typeof event.data === 'string') {
          clearTimeout(timeout);
          resolve(JSON.parse(event.data));
        }
      };
    });

    // Send request with specific ID
    const requestId = 123;
    const testHash = 'eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee';
    ws.send(JSON.stringify({ type: 'req', id: requestId, hash: testHash }));

    const response = await responsePromise;
    ws.close();

    // Verify response format matches protocol spec
    expect(response.type).toBe('res');
    expect(response.id).toBe(requestId);
    expect(response.hash).toBe(testHash);
    expect(response.found).toBe(false);
  });

  test('WebSocket relay: Browser B gets file from Browser A via Rust server', async () => {
    // This test verifies end-to-end peer-to-peer relay through the server:
    // 1. Browser A (ServingWsClient) connects and has some data
    // 2. Browser B (TestWsClient) connects and requests that data
    // 3. Server forwards request to A, gets response, relays to B
    // 4. B receives the data successfully

    // Browser A: has content to serve
    const browserA = new ServingWsClient('BrowserA');
    const connectedA = await browserA.connect(RUST_SERVER_URL);
    expect(connectedA).toBe(true);

    // Add some test data to Browser A
    const testContent = new TextEncoder().encode('Hello from Browser A! This is test content for relay.');
    const contentHash = browserA.addData(testContent);
    console.log('Test content hash:', contentHash);

    // Give server time to register Browser A
    await new Promise(r => setTimeout(r, 100));

    // Browser B: wants to get the content
    const browserB = new TestWsClient('BrowserB');
    const connectedB = await browserB.connect(RUST_SERVER_URL);
    expect(connectedB).toBe(true);

    // Browser B requests the data (which only Browser A has)
    // Server should forward to A, get response, and relay to B
    console.log('Browser B requesting data from Browser A via server relay...');
    const receivedData = await browserB.request(contentHash, 5000);

    // Verify B received the correct data
    expect(receivedData).not.toBeNull();
    if (receivedData) {
      const receivedText = new TextDecoder().decode(receivedData);
      console.log('Browser B received:', receivedText);
      expect(receivedText).toBe('Hello from Browser A! This is test content for relay.');
    }

    // Clean up
    browserA.close();
    browserB.close();
  });

  test('WebSocket relay: Multiple browsers can serve different content', async () => {
    // Test with 3 browsers: A has file1, B has file2, C requests both

    // Browser A
    const browserA = new ServingWsClient('BrowserA');
    await browserA.connect(RUST_SERVER_URL);
    const file1 = new TextEncoder().encode('File 1 content from A');
    const hash1 = browserA.addData(file1);

    // Browser B
    const browserB = new ServingWsClient('BrowserB');
    await browserB.connect(RUST_SERVER_URL);
    const file2 = new TextEncoder().encode('File 2 content from B');
    const hash2 = browserB.addData(file2);

    await new Promise(r => setTimeout(r, 100));

    // Browser C requests both files
    const browserC = new TestWsClient('BrowserC');
    await browserC.connect(RUST_SERVER_URL);

    // Request file 1 (from A)
    const received1 = await browserC.request(hash1, 5000);
    expect(received1).not.toBeNull();
    expect(new TextDecoder().decode(received1!)).toBe('File 1 content from A');

    // Request file 2 (from B)
    const received2 = await browserC.request(hash2, 5000);
    expect(received2).not.toBeNull();
    expect(new TextDecoder().decode(received2!)).toBe('File 2 content from B');

    // Clean up
    browserA.close();
    browserB.close();
    browserC.close();
  });
});
