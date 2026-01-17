/**
 * follows hook - manages follow lists and follow/unfollow actions
 * Svelte version using writable stores
 */
import { writable, get } from 'svelte/store';
import { nip19 } from 'nostr-tools';
import { NDKEvent } from 'ndk';
import { LRUCache } from '../utils/lruCache';
import { KeyedEventEmitter } from '../utils/keyedEventEmitter';
import { ndk, nostrStore } from '../nostr';

export interface Follows {
  pubkey: string;
  follows: string[];
  followedAt: number;
}

// Cache follows lists
const followsCache = new LRUCache<string, Follows>(100);

// Event emitter for follows updates
const followsEmitter = new KeyedEventEmitter<string, Follows>();

// Track active subscriptions - kept open for live updates
const activeSubscriptions = new Map<string, { stop: () => void }>();

/**
 * Subscribe to follows (kept open for live updates).
 * NOTE: Main thread NDK has no relay connections - use subscribe() which
 * works via NDK's internal relay pool, not fetchEvents() which hangs forever.
 */
function fetchFollows(pubkey: string): void {
  if (!pubkey || pubkey.length !== 64) {
    console.warn('[follows] Invalid pubkey:', pubkey);
    return;
  }

  // Already subscribed
  if (activeSubscriptions.has(pubkey)) return;

  // Track latest event timestamp to only process newer events
  let latestTimestamp = 0;

  const sub = ndk.subscribe(
    { kinds: [3], authors: [pubkey] },
    { closeOnEose: false } // Keep open for live updates
  );

  sub.on('event', (event: NDKEvent) => {
    const eventTime = event.created_at || 0;

    // Only process if newer than what we have
    if (eventTime <= latestTimestamp) return;
    latestTimestamp = eventTime;

    const followPubkeys = event.tags
      .filter(t => t[0] === 'p' && t[1])
      .map(t => t[1]);

    const follows: Follows = {
      pubkey: event.pubkey,
      follows: followPubkeys,
      followedAt: eventTime,
    };
    followsCache.set(pubkey, follows);
    followsEmitter.notify(pubkey, follows);
  });

  activeSubscriptions.set(pubkey, { stop: () => sub.stop() });
}

/**
 * Create a Svelte store for a user's follows list
 */
export function createFollowsStore(pubkey?: string) {
  const pubkeyHex = pubkey?.startsWith('npub1')
    ? (() => {
        try {
          const decoded = nip19.decode(pubkey);
          return decoded.data as string;
        } catch {
          return '';
        }
      })()
    : pubkey || '';

  const { subscribe: storeSubscribe, set } = writable<Follows | undefined>(
    pubkeyHex ? followsCache.get(pubkeyHex) : undefined
  );

  if (pubkeyHex) {
    // Subscribe to updates
    const unsub = followsEmitter.subscribe(pubkeyHex, set);

    // Fetch if not cached
    const cached = followsCache.get(pubkeyHex);
    if (cached) {
      set(cached);
    } else {
      fetchFollows(pubkeyHex);
    }

    // Return store with cleanup
    return {
      subscribe: storeSubscribe,
      destroy: unsub,
    };
  }

  return {
    subscribe: storeSubscribe,
    destroy: () => {},
  };
}

/**
 * Get follows synchronously (from cache)
 */
export function getFollowsSync(pubkey?: string): Follows | undefined {
  if (!pubkey) return undefined;
  const pubkeyHex = pubkey.startsWith('npub1')
    ? (() => {
        try {
          const decoded = nip19.decode(pubkey);
          return decoded.data as string;
        } catch {
          return '';
        }
      })()
    : pubkey;
  return followsCache.get(pubkeyHex);
}

/**
 * Follow a pubkey - publishes kind 3 event with updated follow list
 */
export async function followPubkey(targetPubkey: string): Promise<boolean> {
  const pk = get(nostrStore).pubkey;
  if (!pk || !ndk.signer) return false;

  // Get current follows
  let currentFollows = followsCache.get(pk);
  if (!currentFollows) {
    await fetchFollows(pk);
    currentFollows = followsCache.get(pk);
  }

  const follows = currentFollows?.follows || [];
  if (follows.includes(targetPubkey)) return true; // Already following

  const newFollows = [...follows, targetPubkey];
  return publishFollowList(pk, newFollows);
}

/**
 * Unfollow a pubkey - publishes kind 3 event with updated follow list
 */
export async function unfollowPubkey(targetPubkey: string): Promise<boolean> {
  const pk = get(nostrStore).pubkey;
  if (!pk || !ndk.signer) return false;

  // Get current follows
  let currentFollows = followsCache.get(pk);
  if (!currentFollows) {
    await fetchFollows(pk);
    currentFollows = followsCache.get(pk);
  }

  const follows = currentFollows?.follows || [];
  if (!follows.includes(targetPubkey)) return true; // Already not following

  const newFollows = follows.filter(p => p !== targetPubkey);
  return publishFollowList(pk, newFollows);
}

// Track the last used timestamp to ensure strictly increasing timestamps
let lastFollowTimestamp = 0;

async function publishFollowList(pk: string, follows: string[]): Promise<boolean> {
  try {
    const event = new NDKEvent(ndk);
    event.kind = 3;
    event.content = '';
    event.tags = follows.map(p => ['p', p]);

    // Ensure strictly increasing timestamp (nostr-social-graph rejects equal timestamps)
    const now = Math.floor(Date.now() / 1000);
    event.created_at = Math.max(now, lastFollowTimestamp + 1);
    lastFollowTimestamp = event.created_at;

    await event.publish();

    // Update cache
    const newFollows: Follows = {
      pubkey: pk,
      follows,
      followedAt: event.created_at || Math.floor(Date.now() / 1000),
    };
    followsCache.set(pk, newFollows);
    followsEmitter.notify(pk, newFollows);

    // Sync to worker for WebRTC peer classification
    const { getWorkerAdapter } = await import('../workerAdapter');
    const adapter = getWorkerAdapter();
    if (adapter) {
      await adapter.setFollows(follows);
    }

    return true;
  } catch (e) {
    console.error('[follows] publish error', e);
    return false;
  }
}

/**
 * Invalidate cache for a pubkey (force refetch)
 */
export function invalidateFollows(pubkey: string): void {
  followsCache.delete(pubkey);
  fetchFollows(pubkey);
}
