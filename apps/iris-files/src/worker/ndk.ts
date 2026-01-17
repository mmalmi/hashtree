/**
 * NDK instance for Worker
 *
 * Runs NDK with:
 * - Real relay connections
 * - ndk-cache (Dexie) for IndexedDB caching
 * - nostr-wasm for fast signature verification
 *
 * Main thread communicates via WorkerAdapter postMessage.
 */

import NDK, { NDKEvent, NDKPrivateKeySigner, type NDKFilter } from 'ndk';
import NDKCacheAdapterDexie from 'ndk-cache';
import { verifyEvent, matchFilter } from 'nostr-tools';
import { NostrWasm } from './nostr-wasm';
import type { SignedEvent, NostrFilter } from './protocol';

// NDK instance - initialized lazily
let ndk: NDK | null = null;

// nostr-wasm verifier interface
interface WasmVerifier {
  verifyEvent(event: unknown): void; // throws on invalid sig
}
let wasmVerifier: WasmVerifier | null = null;
let wasmLoading = false;

// Event callbacks
let onEventCallback: ((subId: string, event: SignedEvent) => void) | null = null;
let onEoseCallback: ((subId: string) => void) | null = null;

// Active subscriptions
const subscriptions = new Map<string, ReturnType<NDK['subscribe']>>();

/**
 * Load nostr-wasm from public wasm file (not base64 inlined)
 * Runs in background - verification falls back to JS until loaded
 */
async function loadNostrWasm(): Promise<void> {
  if (wasmVerifier || wasmLoading) return;
  wasmLoading = true;

  try {
    // Fetch wasm from public directory (not base64 inlined)
    const response = fetch('/secp256k1.wasm');
    wasmVerifier = await NostrWasm(response);
    console.log('[Worker NDK] nostr-wasm loaded from wasm file');
  } catch (err) {
    console.warn('[Worker NDK] nostr-wasm load failed, using JS fallback:', err);
  } finally {
    wasmLoading = false;
  }
}

/**
 * Custom signature verification function for NDK
 * Uses nostr-wasm if loaded, falls back to nostr-tools
 */
async function verifySignature(event: NDKEvent): Promise<boolean> {
  if (wasmVerifier) {
    try {
      // nostr-wasm verifyEvent checks both id hash and signature
      wasmVerifier.verifyEvent({
        id: event.id,
        pubkey: event.pubkey,
        created_at: event.created_at,
        kind: event.kind,
        tags: event.tags,
        content: event.content,
        sig: event.sig,
      });
      return true;
    } catch {
      return false;
    }
  }

  // Fallback to nostr-tools until wasm loads
  // Don't call event.verifySignature() - that would cause infinite recursion
  return verifyEvent({
    id: event.id!,
    pubkey: event.pubkey,
    created_at: event.created_at!,
    kind: event.kind!,
    tags: event.tags,
    content: event.content,
    sig: event.sig!,
  });
}

/**
 * Initialize NDK with cache and nostr-wasm
 */
export async function initNdk(
  relays: string[],
  options: {
    pubkey?: string;
    nsec?: string;
  } = {}
): Promise<void> {
  // Create cache adapter
  const cacheAdapter = new NDKCacheAdapterDexie({ dbName: 'hashtree-ndk-worker', eventCacheSize: 5000 });

  // Create NDK instance
  ndk = new NDK({
    explicitRelayUrls: relays,
    cacheAdapter,
    // Custom verification - will use wasm when loaded, JS fallback until then
    signatureVerificationFunction: verifySignature,
  });

  // Set up signer if nsec provided
  if (options.nsec) {
    ndk.signer = new NDKPrivateKeySigner(options.nsec);
  }

  // Connect to relays immediately, but don't block init on network latency.
  // Subscriptions will attach as relays come online.
  const connectPromise = ndk.connect();

  // Wait for at least one relay to connect (with timeout)
  const CONNECTION_TIMEOUT = 5000;
  const connectionWait = new Promise<void>((resolve) => {
    const startTime = Date.now();
    const checkConnection = () => {
      const connected = ndk?.pool?.connectedRelays();
      if (connected && connected.size > 0) {
        console.log('[Worker NDK] At least one relay connected');
        resolve();
        return;
      }
      if (Date.now() - startTime > CONNECTION_TIMEOUT) {
        console.warn('[Worker NDK] Connection timeout, proceeding anyway');
        resolve();
        return;
      }
      setTimeout(checkConnection, 50);
    };
    checkConnection();
  });
  void connectionWait;

  // Log when all relays are connected (async, don't block)
  connectPromise.then(() => {
    console.log('[Worker NDK] All relays connected');
  });

  // Lazy load nostr-wasm in background
  loadNostrWasm();

  console.log('[Worker NDK] Initialized with', relays.length, 'relays');
}

/**
 * Get the NDK instance
 */
export function getNdk(): NDK | null {
  return ndk;
}

/**
 * Set event callback
 */
export function setOnEvent(callback: (subId: string, event: SignedEvent) => void): void {
  onEventCallback = callback;
}

/**
 * Set EOSE callback
 */
export function setOnEose(callback: (subId: string) => void): void {
  onEoseCallback = callback;
}

/**
 * Subscribe to events
 */
export function subscribe(subId: string, filters: NostrFilter[]): void {
  if (!ndk) {
    console.error('[Worker NDK] Not initialized');
    return;
  }

  // Close existing subscription with same ID
  unsubscribe(subId);

  // Convert NostrFilter to NDKFilter
  const ndkFilters: NDKFilter[] = filters.map(f => {
    const filter: NDKFilter = {
      ids: f.ids,
      authors: f.authors,
      kinds: f.kinds,
      since: f.since,
      until: f.until,
      limit: f.limit,
    };

    // Copy tag filters
    for (const key of Object.keys(f)) {
      if (key.startsWith('#')) {
        (filter as Record<string, unknown>)[key] = f[key];
      }
    }

    return filter;
  });

  // skipValidation: nostr-wasm verifyEvent handles structure validation
  const sub = ndk.subscribe(ndkFilters, { closeOnEose: false, skipValidation: true });

  sub.on('event', (event: NDKEvent) => {
    const signedEvent: SignedEvent = {
      id: event.id!,
      pubkey: event.pubkey,
      kind: event.kind!,
      content: event.content,
      tags: event.tags,
      created_at: event.created_at!,
      sig: event.sig!,
    };
    onEventCallback?.(subId, signedEvent);
  });

  sub.on('eose', () => {
    onEoseCallback?.(subId);
  });

  subscriptions.set(subId, sub);
  console.log('[Worker NDK] Subscribed:', subId);
}

/**
 * Unsubscribe
 */
export function unsubscribe(subId: string): void {
  const sub = subscriptions.get(subId);
  if (sub) {
    sub.stop();
    subscriptions.delete(subId);
    console.log('[Worker NDK] Unsubscribed:', subId);
  }
}

/**
 * Publish an event
 */
export async function publish(event: SignedEvent): Promise<void> {
  if (!ndk) {
    throw new Error('NDK not initialized');
  }

  const ndkEvent = new NDKEvent(ndk, {
    id: event.id,
    pubkey: event.pubkey,
    kind: event.kind,
    content: event.content,
    tags: event.tags,
    created_at: event.created_at,
    sig: event.sig,
  });

  await ndkEvent.publish();

  // Dispatch to local subscriptions that match this event
  // NDK doesn't echo published events back to sender, so we do it locally
  if (onEventCallback) {
    for (const [subId, sub] of subscriptions) {
      // Get the filters from the subscription (NDK stores them on the subscription object)
      const filters = (sub as unknown as { filters?: NDKFilter[] }).filters || [];
      for (const filter of filters) {
        if (matchFilter(filter, event)) {
          onEventCallback(subId, event);
          break; // Only dispatch once per subscription
        }
      }
    }
  }
}

/**
 * Close NDK and all subscriptions
 */
export function closeNdk(): void {
  for (const [subId, sub] of subscriptions) {
    sub.stop();
    console.log('[Worker NDK] Closed subscription:', subId);
  }
  subscriptions.clear();

  // NDK doesn't have a close method, but we can disconnect relays
  if (ndk?.pool) {
    for (const relay of ndk.pool.relays.values()) {
      relay.disconnect();
    }
  }

  ndk = null;
  console.log('[Worker NDK] Closed');
}

/**
 * Update relays dynamically
 * Disconnects old relays and connects to new ones
 */
export async function setRelays(relays: string[]): Promise<void> {
  if (!ndk?.pool) {
    console.warn('[Worker NDK] Cannot setRelays - NDK not initialized');
    return;
  }

  console.log('[Worker NDK] Updating relays:', relays);

  try {
    // Disconnect all current relays
    for (const relay of ndk.pool.relays.values()) {
      try {
        relay.disconnect();
      } catch (e) {
        console.warn('[Worker NDK] Error disconnecting relay:', e);
      }
    }

    // Clear the relay pool
    ndk.pool.relays.clear();

    // Add new relays
    for (const url of relays) {
      try {
        ndk.addExplicitRelay(url);
      } catch (e) {
        console.warn('[Worker NDK] Error adding relay:', url, e);
      }
    }

    // Reconnect with timeout
    const connectPromise = ndk.connect();
    const timeoutPromise = new Promise<void>((_, reject) =>
      setTimeout(() => reject(new Error('Connect timeout')), 10000)
    );

    await Promise.race([connectPromise, timeoutPromise]).catch(e => {
      console.warn('[Worker NDK] Connect timeout/error, proceeding anyway:', e);
    });

    console.log('[Worker NDK] Relays updated, connected to', relays.length, 'relays');
  } catch (e) {
    console.error('[Worker NDK] Error in setRelays:', e);
  }
}

/**
 * Get relay stats
 */
export function getRelayStats(): { url: string; connected: boolean; eventsReceived: number; eventsSent: number }[] {
  if (!ndk?.pool) return [];

  const stats: { url: string; connected: boolean; eventsReceived: number; eventsSent: number }[] = [];

  for (const relay of ndk.pool.relays.values()) {
    stats.push({
      url: relay.url,
      connected: relay.status >= 5, // NDKRelayStatus.CONNECTED = 5
      eventsReceived: 0, // TODO: track this
      eventsSent: 0,
    });
  }

  return stats;
}

/**
 * Republish all user's hashtree events from cache to relays
 * This helps recover when events exist locally but weren't properly published
 *
 * For unsigned events (never signed), we sign them using the worker's signing flow.
 * For signed events, we republish directly.
 * Also pushes blob data to Blossom servers.
 *
 * @param prefix - Optional URL-encoded prefix to filter trees by d-tag
 */
export async function republishTrees(
  pubkey: string,
  signFn: (template: { kind: number; created_at: number; content: string; tags: string[][] }) => Promise<{ id: string; pubkey: string; kind: number; content: string; tags: string[][]; created_at: number; sig: string }>,
  pushToBlossomFn?: (hash: Uint8Array, key?: Uint8Array, treeName?: string) => Promise<{ pushed: number; skipped: number; failed: number }>,
  prefix?: string
): Promise<number> {
  if (!ndk) {
    throw new Error('NDK not initialized');
  }

  // Decode prefix if provided (it's URL-encoded)
  const decodedPrefix = prefix ? decodeURIComponent(prefix) : undefined;
  console.log('[Worker NDK] Republishing trees for', pubkey, decodedPrefix ? `with prefix: ${decodedPrefix}` : '');

  // Fetch user's hashtree events from cache and relays
  const filter: NDKFilter = {
    kinds: [30078],
    authors: [pubkey],
    '#l': ['hashtree'],
  };

  // Fetch events (will check cache first)
  const events = await ndk.fetchEvents(filter);
  console.log('[Worker NDK] Found', events.size, 'events');

  // Filter by prefix if provided
  const filteredEvents = decodedPrefix
    ? Array.from(events).filter(event => {
        const dTag = event.tags.find(t => t[0] === 'd')?.[1];
        return dTag?.startsWith(decodedPrefix);
      })
    : Array.from(events);
  console.log('[Worker NDK] After prefix filter:', filteredEvents.length, 'events');

  let count = 0;
  let skippedNoHash = 0;

  for (const event of filteredEvents) {
    // Skip events without a hash tag (deleted trees)
    const hasHash = event.tags.some(t => t[0] === 'hash' && t[1]);
    if (!hasHash) {
      skippedNoHash++;
      continue;
    }

    const dTag = event.tags.find(t => t[0] === 'd')?.[1];

    try {
      const relaySet = ndk.pool?.connectedRelays();
      if (!relaySet || relaySet.size === 0) {
        console.warn('[Worker NDK] No connected relays');
        continue;
      }

      let signedEvent: NDKEvent;

      if (event.sig) {
        // Already signed - use existing event
        console.log('[Worker NDK] Republishing signed event:', dTag);
        signedEvent = event;
      } else {
        // Unsigned event - sign it using worker's signing flow
        console.log('[Worker NDK] Signing:', dTag);

        const template = {
          kind: 30078,
          created_at: Math.floor(Date.now() / 1000),
          content: event.content || '',
          tags: event.tags,
        };

        const signed = await signFn(template);

        // Create NDKEvent from signed event
        signedEvent = new NDKEvent(ndk, {
          id: signed.id,
          pubkey: signed.pubkey,
          kind: signed.kind,
          content: signed.content,
          tags: signed.tags,
          created_at: signed.created_at,
          sig: signed.sig,
        });
      }

      // Publish to all connected relays
      const promises = Array.from(relaySet).map(async (relay) => {
        try {
          await relay.publish(signedEvent);
        } catch (e) {
          console.warn('[Worker NDK] Failed to publish to relay:', relay.url, e);
        }
      });
      await Promise.all(promises);
      count++;
      console.log('[Worker NDK] Published:', dTag);

      // Push blob data to Blossom if function provided
      if (pushToBlossomFn) {
        const hashTag = event.tags.find(t => t[0] === 'hash')?.[1];
        const keyTag = event.tags.find(t => t[0] === 'key')?.[1];

        if (hashTag) {
          try {
            const hashBytes = new Uint8Array(hashTag.match(/.{2}/g)!.map(b => parseInt(b, 16)));
            const keyBytes = keyTag ? new Uint8Array(keyTag.match(/.{2}/g)!.map(b => parseInt(b, 16))) : undefined;

            console.log('[Worker NDK] Pushing to Blossom:', dTag);
            const result = await pushToBlossomFn(hashBytes, keyBytes, dTag);
            console.log('[Worker NDK] Blossom push result:', dTag, result);
          } catch (e) {
            console.warn('[Worker NDK] Failed to push to Blossom:', dTag, e);
          }
        }
      }
    } catch (e) {
      console.warn('[Worker NDK] Failed to republish:', dTag, e);
    }
  }

  console.log('[Worker NDK] Republished', count, 'trees, skipped', skippedNoHash, 'deleted');
  return count;
}

/**
 * Republish a single tree's event from cache to relays
 * This republishes the original event as-is (preserves signature/timestamp)
 * Works for any user's tree, not just own.
 */
export async function republishTree(pubkey: string, treeName: string): Promise<boolean> {
  if (!ndk) {
    throw new Error('NDK not initialized');
  }

  console.log('[Worker NDK] Republishing single tree:', pubkey, treeName);

  // Fetch the specific event
  const filter: NDKFilter = {
    kinds: [30078],
    authors: [pubkey],
    '#d': [treeName],
    '#l': ['hashtree'],
  };

  const events = await ndk.fetchEvents(filter);
  if (events.size === 0) {
    console.warn('[Worker NDK] Event not found for', treeName);
    return false;
  }

  // Get the most recent event (in case there are multiple)
  let event: NDKEvent | null = null;
  for (const e of events) {
    if (!event || (e.created_at && event.created_at && e.created_at > event.created_at)) {
      event = e;
    }
  }

  if (!event || !event.sig) {
    console.warn('[Worker NDK] No signed event found for', treeName);
    return false;
  }

  // Skip events without a hash tag (deleted trees)
  const hasHash = event.tags.some(t => t[0] === 'hash' && t[1]);
  if (!hasHash) {
    console.warn('[Worker NDK] Event has no hash (deleted tree):', treeName);
    return false;
  }

  const relaySet = ndk.pool?.connectedRelays();
  if (!relaySet || relaySet.size === 0) {
    console.warn('[Worker NDK] No connected relays');
    return false;
  }

  // Publish to all connected relays
  let success = false;
  const promises = Array.from(relaySet).map(async (relay) => {
    try {
      await relay.publish(event);
      success = true;
    } catch (e) {
      console.warn('[Worker NDK] Failed to publish to relay:', relay.url, e);
    }
  });
  await Promise.all(promises);

  console.log('[Worker NDK] Republished tree:', treeName, success ? 'success' : 'failed');
  return success;
}
