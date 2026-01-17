/**
 * RefResolver tests using local test relay
 * @vitest-environment node
 */
import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import WebSocket from 'ws';
import { spawn, type ChildProcess } from 'child_process';
import { join } from 'path';
import NDK, { NDKEvent, NDKPrivateKeySigner, type NDKFilter, type NDKSubscriptionOptions } from '@nostr-dev-kit/ndk';
import { nip19, generateSecretKey, getPublicKey } from 'nostr-tools';
import { createNostrRefResolver, type NostrFilter, type NostrEvent } from '../src/resolver/nostr.js';
import { toHex, fromHex, cid } from '../src/types.js';

// NDK requires WebSocket to be available globally in Node.js
// @ts-ignore
globalThis.WebSocket = WebSocket;

// Use local test relay
const TEST_RELAY_PORT = 14736; // Use different port to avoid conflicts
const TEST_RELAY = `ws://localhost:${TEST_RELAY_PORT}`;

let relayProcess: ChildProcess | null = null;

async function startLocalRelay(): Promise<void> {
  return new Promise((resolve, reject) => {
    const relayPath = join(__dirname, '../../../e2e/relay/index.js');
    relayProcess = spawn('node', [relayPath], {
      env: { ...process.env, RELAY_PORT: String(TEST_RELAY_PORT) },
      stdio: ['pipe', 'pipe', 'pipe'],
    });

    relayProcess.stdout?.on('data', (data) => {
      const msg = data.toString();
      if (msg.includes('Running on')) {
        resolve();
      }
    });

    relayProcess.stderr?.on('data', (data) => {
      console.error('Relay stderr:', data.toString());
    });

    relayProcess.on('error', reject);

    // Timeout after 5 seconds
    setTimeout(() => reject(new Error('Relay startup timeout')), 5000);
  });
}

function stopLocalRelay(): void {
  if (relayProcess) {
    relayProcess.kill();
    relayProcess = null;
  }
}

/**
 * Create subscribe and publish functions from NDK instance
 */
function createNostrFunctions(ndk: NDK) {
  return {
    subscribe: (filter: NostrFilter, onEvent: (event: NostrEvent) => void) => {
      const ndkFilter: NDKFilter = {
        kinds: filter.kinds,
        authors: filter.authors,
        '#d': filter['#d'],
        '#l': filter['#l'],
      };
      const opts: NDKSubscriptionOptions = {
        closeOnEose: false,
        cacheUsage: 4, // ONLY_RELAY - skip cache completely
      };
      const sub = ndk.subscribe(ndkFilter, opts);
      sub.on('event', (e: NDKEvent) => {
        onEvent({
          id: e.id,
          pubkey: e.pubkey,
          kind: e.kind ?? 30078,
          content: e.content,
          tags: e.tags,
          created_at: e.created_at ?? 0,
        });
      });
      return () => sub.stop();
    },
    publish: async (event: Omit<NostrEvent, 'id' | 'pubkey' | 'created_at'>) => {
      try {
        const ndkEvent = new NDKEvent(ndk);
        ndkEvent.kind = event.kind;
        ndkEvent.content = event.content;
        ndkEvent.tags = event.tags;
        await ndkEvent.publish();
        return true;
      } catch (e) {
        console.error('Failed to publish event:', e);
        return false;
      }
    },
  };
}

describe('NostrRefResolver', () => {
  let ndk: NDK;
  let secretKey: Uint8Array;
  let pubkey: string;
  let npub: string;

  beforeAll(async () => {
    // Start local relay
    await startLocalRelay();

    // Generate test keypair
    secretKey = generateSecretKey();
    pubkey = getPublicKey(secretKey);
    npub = nip19.npubEncode(pubkey);

    // Create NDK instance
    const nsec = nip19.nsecEncode(secretKey);
    ndk = new NDK({
      explicitRelayUrls: [TEST_RELAY],
      signer: new NDKPrivateKeySigner(nsec),
      // Disable signature verification for tests (events are from local relay)
      signatureVerificationFunction: async () => true,
    });

    // Set up connection promise before calling connect
    const connectionPromise = new Promise<void>((resolve) => {
      ndk.pool.once('relay:connect', () => resolve());
    });

    // Start connecting (doesn't wait for actual connection)
    ndk.connect();

    // Wait for relay to actually connect
    await connectionPromise;
  }, 15000);

  afterAll(() => {
    stopLocalRelay();
  });

  it('should timeout for unpublished key', async () => {
    const { subscribe, publish } = createNostrFunctions(ndk);
    const resolver = createNostrRefResolver({
      subscribe,
      publish,
      getPubkey: () => pubkey,
      nip19,
    });

    // resolve() waits indefinitely, so we use timeout on caller side
    const timeoutPromise = new Promise<null>((resolve) => setTimeout(() => resolve(null), 2000));
    const result = await Promise.race([
      resolver.resolve(`${npub}/unpublished-key-${Date.now()}`),
      timeoutPromise,
    ]);
    expect(result).toBeNull(); // Timeout returned null

    resolver.stop?.();
  });

  it('should publish and resolve a CID', async () => {
    const { subscribe, publish } = createNostrFunctions(ndk);
    const resolver = createNostrRefResolver({
      subscribe,
      publish,
      getPubkey: () => pubkey,
      nip19,
    });

    const treeName = `test-tree-${Date.now()}`;
    const key = `${npub}/${treeName}`;
    const testCid = cid(fromHex('abcd'.repeat(16))); // 32 bytes

    // Subscribe BEFORE publishing (NDK works with live events, not stored)
    let resolvedCid: typeof testCid | null = null;
    const unsubscribe = resolver.subscribe(key, (c) => {
      if (c) resolvedCid = c;
    });

    // Wait for subscription to be established
    await new Promise(r => setTimeout(r, 100));

    // Publish
    const published = await resolver.publish!(key, testCid);
    expect(published?.success).toBe(true);

    // Wait for event to be received
    await new Promise(r => setTimeout(r, 500));

    expect(resolvedCid).not.toBeNull();
    expect(toHex(resolvedCid!.hash)).toBe(toHex(testCid.hash));

    unsubscribe();
    resolver.stop?.();
  });

  it('should list trees for a user', async () => {
    const { subscribe, publish } = createNostrFunctions(ndk);
    const resolver = createNostrRefResolver({
      subscribe,
      publish,
      getPubkey: () => pubkey,
      nip19,
    });

    // Publish a couple of trees
    const tree1 = `list-test-1-${Date.now()}`;
    const tree2 = `list-test-2-${Date.now()}`;
    const cid1 = cid(fromHex('1111'.repeat(16)));
    const cid2 = cid(fromHex('2222'.repeat(16)));

    await resolver.publish!(`${npub}/${tree1}`, cid1);
    await resolver.publish!(`${npub}/${tree2}`, cid2);

    // Wait for relay
    await new Promise(r => setTimeout(r, 500));

    // List with callback (wait for results then unsubscribe)
    const trees = await new Promise<Array<{ key: string; cid: { hash: Uint8Array } }>>((resolve) => {
      let lastEntries: Array<{ key: string; cid: { hash: Uint8Array } }> = [];
      const unsubscribe = resolver.list!(npub, (entries) => {
        lastEntries = entries;
      });
      // Wait a bit for entries to come in, then resolve
      setTimeout(() => {
        unsubscribe();
        resolve(lastEntries);
      }, 1000);
    });

    expect(trees.length).toBeGreaterThanOrEqual(2);

    const names = trees.map(t => t.key.split('/')[1]);
    expect(names).toContain(tree1);
    expect(names).toContain(tree2);

    resolver.stop?.();
  });

  it('should subscribe and receive updates', async () => {
    const { subscribe, publish } = createNostrFunctions(ndk);
    const resolver = createNostrRefResolver({
      subscribe,
      publish,
      getPubkey: () => pubkey,
      nip19,
    });

    const treeName = `subscribe-test-${Date.now()}`;
    const key = `${npub}/${treeName}`;
    const initialCid = cid(fromHex('aaaa'.repeat(16)));
    const updatedCid = cid(fromHex('bbbb'.repeat(16)));

    // Publish initial value
    await resolver.publish!(key, initialCid);
    await new Promise(r => setTimeout(r, 500));

    // Subscribe
    const receivedHashes: string[] = [];
    const unsubscribe = resolver.subscribe(key, (received) => {
      if (received) {
        receivedHashes.push(toHex(received.hash));
      }
    });

    // Wait for initial callback
    await new Promise(r => setTimeout(r, 1000));

    // Should have received initial hash
    expect(receivedHashes.length).toBeGreaterThanOrEqual(1);
    expect(receivedHashes[receivedHashes.length - 1]).toBe(toHex(initialCid.hash));

    // Publish update
    await resolver.publish!(key, updatedCid);

    // Wait for update
    await new Promise(r => setTimeout(r, 1500));

    // Should have received updated hash
    expect(receivedHashes.length).toBeGreaterThanOrEqual(2);
    expect(receivedHashes[receivedHashes.length - 1]).toBe(toHex(updatedCid.hash));

    unsubscribe();
    resolver.stop?.();
  }, 10000);

  it('should list and receive tree list updates', async () => {
    const { subscribe, publish } = createNostrFunctions(ndk);
    const resolver = createNostrRefResolver({
      subscribe,
      publish,
      getPubkey: () => pubkey,
      nip19,
    });

    const treeName = `list-update-test-${Date.now()}`;
    const testCid = cid(fromHex('cccc'.repeat(16)));

    // Subscribe to list
    let lastEntries: Array<{ key: string; cid: { hash: Uint8Array } }> = [];
    const unsubscribe = resolver.list!(npub, (entries) => {
      lastEntries = entries;
    });

    // Wait for initial list
    await new Promise(r => setTimeout(r, 1000));

    const initialCount = lastEntries.length;

    // Publish new tree
    await resolver.publish!(`${npub}/${treeName}`, testCid);

    // Wait for update
    await new Promise(r => setTimeout(r, 1500));

    // Should have one more tree
    expect(lastEntries.length).toBe(initialCount + 1);
    expect(lastEntries.some(e => e.key.includes(treeName))).toBe(true);

    unsubscribe();
    resolver.stop?.();
  }, 10000);

  it('should handle invalid keys gracefully', async () => {
    const { subscribe, publish } = createNostrFunctions(ndk);
    const resolver = createNostrRefResolver({
      subscribe,
      publish,
      getPubkey: () => pubkey,
      nip19,
    });

    // Invalid npub
    const result1 = await resolver.resolve('invalid-key');
    expect(result1).toBeNull();

    // Missing tree name
    const result2 = await resolver.resolve(npub);
    expect(result2).toBeNull();

    // Invalid npub format
    const result3 = await resolver.resolve('npub1invalid/tree');
    expect(result3).toBeNull();

    resolver.stop?.();
  });

  it('should only keep latest event per tree', async () => {
    const { subscribe, publish } = createNostrFunctions(ndk);
    const resolver = createNostrRefResolver({
      subscribe,
      publish,
      getPubkey: () => pubkey,
      nip19,
    });

    const treeName = `latest-test-${Date.now()}`;
    const key = `${npub}/${treeName}`;

    // Publish multiple updates with longer delays to ensure distinct timestamps
    // (Nostr uses second-precision timestamps)
    const cid1 = cid(fromHex('1111'.repeat(16)));
    const cid2 = cid(fromHex('2222'.repeat(16)));
    const cid3 = cid(fromHex('3333'.repeat(16)));

    // Subscribe BEFORE publishing to receive all updates
    const receivedHashes: string[] = [];
    const unsubscribe = resolver.subscribe(key, (c) => {
      if (c) receivedHashes.push(toHex(c.hash));
    });

    // Wait for subscription to be established
    await new Promise(r => setTimeout(r, 100));

    await resolver.publish!(key, cid1);
    await new Promise(r => setTimeout(r, 1100)); // Wait >1s for next timestamp
    await resolver.publish!(key, cid2);
    await new Promise(r => setTimeout(r, 1100)); // Wait >1s for next timestamp
    await resolver.publish!(key, cid3);

    // Wait for all events to be received
    await new Promise(r => setTimeout(r, 500));

    // Should have received multiple updates, and the last one should be cid3
    expect(receivedHashes.length).toBeGreaterThanOrEqual(1);
    expect(receivedHashes[receivedHashes.length - 1]).toBe(toHex(cid3.hash));

    unsubscribe();
    resolver.stop?.();
  }, 10000);

  it('should preserve visibility info when publishing with visibility', async () => {
    const { subscribe, publish } = createNostrFunctions(ndk);
    const resolver = createNostrRefResolver({
      subscribe,
      publish,
      getPubkey: () => pubkey,
      nip19,
    });

    const treeName = `visibility-test-${Date.now()}`;
    const key = `${npub}/${treeName}`;
    const testCid = cid(fromHex('dddd'.repeat(16)));

    // Publish with link-visible visibility
    const published = await resolver.publish!(key, testCid, {
      visibility: 'link-visible',
    });
    expect(published?.success).toBe(true);

    // Subscribe to list and check visibility
    const entries = await new Promise<Array<{ key: string; visibility?: string }>>((resolve) => {
      let lastEntries: Array<{ key: string; visibility?: string }> = [];
      let unsubscribe: (() => void) | null = null;
      let resolved = false;
      unsubscribe = resolver.list!(npub, (entries) => {
        lastEntries = entries;
        const found = entries.find(e => e.key.includes(treeName));
        if (found && !resolved) {
          resolved = true;
          // Defer unsubscribe to avoid calling before assignment
          setTimeout(() => unsubscribe?.(), 0);
          resolve(lastEntries);
        }
      });
      // Timeout after 3s
      setTimeout(() => {
        if (!resolved) {
          resolved = true;
          unsubscribe?.();
          resolve(lastEntries);
        }
      }, 3000);
    });

    const entry = entries.find(e => e.key.includes(treeName));
    expect(entry).toBeDefined();
    expect(entry!.visibility).toBe('link-visible');

    resolver.stop?.();
  }, 10000);
});
