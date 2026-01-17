/**
 * Store for caching feed videos across components
 * Populated by VideoHome, consumed by FeedSidebar
 */
import { writable, get } from 'svelte/store';
import type { CID } from 'hashtree';
import { ndk, pubkeyToNpub, nostrStore } from '../nostr';
import { createFollowsStore, getFollowsSync } from './follows';
import { getFollows as getSocialGraphFollows } from '../utils/socialGraph';
import { getWorkerAdapter, waitForWorkerAdapter } from '../lib/workerInit';
import { DEFAULT_BOOTSTRAP_PUBKEY, DEFAULT_VIDEO_FEED_PUBKEYS } from '../utils/constants';
import { fromHex } from 'hashtree';
import { updateSubscriptionCache } from './treeRoot';
import { orderFeedWithInterleaving } from '../utils/feedOrder';
import { clearDeletedVideo, getDeletedVideoTimestamp, recordDeletedVideo } from './videoDeletes';
import { isHtreeDebugEnabled, logHtreeDebug } from '../lib/htreeDebug';
import { getAppType } from '../appType';

const log = (event: string, data?: Record<string, unknown>) => {
  if (!isHtreeDebugEnabled()) return;
  logHtreeDebug(`feed:${event}`, data);
};

const MIN_FOLLOWS_THRESHOLD = 5;
const RELAY_WAIT_TIMEOUT_MS = 10000;
const RELAY_RETRY_TIMEOUT_MS = 30000;
const EMPTY_FEED_RETRY_MS = 15000;
const EMPTY_FEED_MAX_RETRIES = 3;

let retryOnRelayScheduled = false;
let emptyFeedRetryTimer: ReturnType<typeof setTimeout> | null = null;
let emptyFeedRetryCount = 0;
let activeSubscription: { stop: () => void } | null = null;

function clearEmptyFeedRetry(): void {
  if (emptyFeedRetryTimer) {
    clearTimeout(emptyFeedRetryTimer);
    emptyFeedRetryTimer = null;
  }
  emptyFeedRetryCount = 0;
}

function scheduleEmptyFeedRetry(reason: string): void {
  if (emptyFeedRetryTimer) return;
  if (emptyFeedRetryCount >= EMPTY_FEED_MAX_RETRIES) {
    log('retry:empty:maxed', { reason, attempts: emptyFeedRetryCount });
    return;
  }
  emptyFeedRetryCount += 1;
  log('retry:empty:schedule', { reason, delayMs: EMPTY_FEED_RETRY_MS, attempt: emptyFeedRetryCount });
  emptyFeedRetryTimer = setTimeout(() => {
    emptyFeedRetryTimer = null;
    if (get(feedStore).length === 0) {
      log('retry:empty:run', { attempt: emptyFeedRetryCount });
      void fetchFeedVideos();
    } else {
      clearEmptyFeedRetry();
    }
  }, EMPTY_FEED_RETRY_MS);
}

export interface FeedVideo {
  href: string;
  title: string;
  ownerPubkey: string | null;
  ownerNpub: string | null;
  treeName: string | null;
  videoId?: string;
  duration?: number;
  timestamp?: number;
  rootCid?: CID;
}

export const feedStore = writable<FeedVideo[]>([]);

// Track if we're already fetching to avoid duplicate requests
let isFetching = false;
let hasInitialFetch = false;

// Cache for fallback follows (bootstrap user's follows)
let fallbackFollowsCache: string[] | null = null;

let lastPubkey: string | null = null;
nostrStore.subscribe((state) => {
  if (state.pubkey === lastPubkey) return;
  lastPubkey = state.pubkey;
  resetFeedFetchState();
  feedStore.set([]);
  log('reset:pubkey', { pubkey: state.pubkey });
});

async function waitForRelayConnection(timeoutMs: number): Promise<number> {
  const initial = get(nostrStore).connectedRelays;
  if (initial > 0) return initial;

  return new Promise<number>((resolve) => {
    let done = false;
    const unsub = nostrStore.subscribe((state) => {
      if (done) return;
      if (state.connectedRelays > 0) {
        done = true;
        unsub();
        resolve(state.connectedRelays);
      }
    });

    setTimeout(() => {
      if (done) return;
      done = true;
      unsub();
      resolve(0);
    }, timeoutMs);
  });
}

function scheduleRelayRetry(): void {
  if (retryOnRelayScheduled) return;
  retryOnRelayScheduled = true;
  log('retry:relay:wait');

  const unsub = nostrStore.subscribe((state) => {
    if (state.connectedRelays > 0) {
      unsub();
      retryOnRelayScheduled = false;
      log('retry:relay:trigger', { connectedRelays: state.connectedRelays });
      void fetchFeedVideos();
    }
  });

  setTimeout(() => {
    if (!retryOnRelayScheduled) return;
    retryOnRelayScheduled = false;
    unsub();
  }, RELAY_RETRY_TIMEOUT_MS);
}

/**
 * Fetch fallback follows from the bootstrap user (for users with few/no follows)
 */
async function fetchFallbackFollows(): Promise<string[]> {
  if (fallbackFollowsCache) {
    return fallbackFollowsCache;
  }

  return new Promise<string[]>((resolve) => {
    let latestTimestamp = 0;
    let latestEventId: string | null = null;
    const sub = ndk.subscribe(
      { kinds: [3], authors: [DEFAULT_BOOTSTRAP_PUBKEY] },
      { closeOnEose: true }
    );

    sub.on('event', (event) => {
      const eventTime = event.created_at || 0;
      if (eventTime < latestTimestamp) return;
      if (eventTime === latestTimestamp && event.id && event.id === latestEventId) return;
      latestTimestamp = eventTime;
      latestEventId = event.id ?? null;

      const followPubkeys = event.tags
        .filter((t: string[]) => t[0] === 'p' && t[1])
        .map((t: string[]) => t[1]);

      fallbackFollowsCache = followPubkeys;
    });

    sub.on('eose', () => {
      resolve(fallbackFollowsCache || []);
    });

    setTimeout(() => resolve(fallbackFollowsCache || []), 5000);
  });
}

/**
 * Get effective follows list with fallback (shared logic with VideoHome)
 * Optimized for speed - uses short timeout and parallel fallback fetch
 */
export async function getEffectiveFollows(userPubkey: string): Promise<string[]> {
  let follows: string[] = [];

  // Try synchronous sources first (instant)
  const cachedFollows = getFollowsSync(userPubkey);
  if (cachedFollows && cachedFollows.follows.length >= MIN_FOLLOWS_THRESHOLD) {
    return cachedFollows.follows;
  }
  if (cachedFollows) follows = cachedFollows.follows;

  const socialGraphFollows = getSocialGraphFollows(userPubkey);
  if (socialGraphFollows && socialGraphFollows.size >= MIN_FOLLOWS_THRESHOLD) {
    return Array.from(socialGraphFollows);
  }
  if (socialGraphFollows && socialGraphFollows.size > follows.length) {
    follows = Array.from(socialGraphFollows);
  }

  // Start fetching fallback in parallel (don't await yet)
  const fallbackPromise = fetchFallbackFollows();

  // Try async sources with SHORT timeout (1.5s max)
  if (follows.length < MIN_FOLLOWS_THRESHOLD) {
    const adapter = getWorkerAdapter();
    if (adapter) {
      try {
        const workerFollows = await Promise.race([
          adapter.getFollows(userPubkey),
          new Promise<string[]>((resolve) => setTimeout(() => resolve([]), 500))
        ]);
        if (workerFollows && workerFollows.length > follows.length) {
          follows = workerFollows;
        }
      } catch {
        // Worker not ready
      }
    }
  }

  // If still not enough, try followsStore with very short timeout
  if (follows.length < MIN_FOLLOWS_THRESHOLD) {
    const followsStore = createFollowsStore(userPubkey);
    const storeFollows = await new Promise<string[]>((resolve) => {
      let resolved = false;
      let unsubscribe: (() => void) | null = null;
      const cleanup = () => {
        if (unsubscribe) unsubscribe();
        followsStore.destroy();
      };
      // Short timeout - 1 second max
      const timeout = setTimeout(() => {
        if (!resolved) { resolved = true; cleanup(); resolve([]); }
      }, 1000);
      unsubscribe = followsStore.subscribe((value) => {
        if (value && value.follows.length > 0 && !resolved) {
          resolved = true; clearTimeout(timeout); cleanup();
          resolve(value.follows);
        }
      });
    });
    if (storeFollows.length > follows.length) {
      follows = storeFollows;
    }
  }

  // Use fallback if user has few/no follows
  if (follows.length < MIN_FOLLOWS_THRESHOLD) {
    const fallbackFollows = await fallbackPromise;
    const combined = new Set(follows);
    combined.add(DEFAULT_BOOTSTRAP_PUBKEY);
    if (getAppType() === 'video') {
      for (const pk of DEFAULT_VIDEO_FEED_PUBKEYS) {
        combined.add(pk);
      }
      if (DEFAULT_VIDEO_FEED_PUBKEYS.length > 0) {
        log('follows:video-seed', { count: DEFAULT_VIDEO_FEED_PUBKEYS.length });
      }
    }
    for (const pk of fallbackFollows) {
      combined.add(pk);
    }
    return Array.from(combined);
  }

  return follows;
}

/**
 * Fetch feed videos - same logic as VideoHome (kind:30078 hashtree events)
 */
export async function fetchFeedVideos(): Promise<void> {
  if (isFetching || hasInitialFetch) {
    log('skip:state', { isFetching, hasInitialFetch });
    return;
  }

  if (get(feedStore).length > 0) {
    log('skip:existing');
    return;
  }

  isFetching = true;
  log('fetch:start');

  try {
    if (!getWorkerAdapter()) {
      log('fetch:worker-not-ready');
      const readyAdapter = await waitForWorkerAdapter(10000);
      if (!readyAdapter) {
        log('fetch:worker-timeout');
      }
    }

    const statePubkey = get(nostrStore).pubkey;
    const usingBootstrap = !statePubkey;
    const userPubkey = statePubkey ?? DEFAULT_BOOTSTRAP_PUBKEY;
    if (usingBootstrap) {
      log('fetch:bootstrap', { pubkey: userPubkey });
    }

    const connectedRelays = await waitForRelayConnection(RELAY_WAIT_TIMEOUT_MS);
    log('relays:connected', { connectedRelays });

    const follows = await getEffectiveFollows(userPubkey);
    log('follows:effective', { count: follows.length });

    const authors = Array.from(new Set([userPubkey, ...follows]));
    const seenVideos = new Map<string, FeedVideo>();
    let flushTimer: ReturnType<typeof setTimeout> | null = null;
    let eventCount = 0;

    const flushVideos = (reason: string) => {
      const videos = orderFeedWithInterleaving(Array.from(seenVideos.values()));
      log('flush', { reason, count: videos.length });
      if (videos.length > 0) {
        feedStore.set(videos);
        hasInitialFetch = true;
        clearEmptyFeedRetry();
      }
    };

    const scheduleFlush = (reason: string) => {
      if (flushTimer) return;
      flushTimer = setTimeout(() => {
        flushTimer = null;
        flushVideos(reason);
      }, 250);
    };

    // Fetch kind:30078 hashtree events (same as VideoHome)
    const sub = ndk.subscribe({
      kinds: [30078],
      authors: authors.slice(0, 500),
      '#l': ['hashtree'],
    }, { closeOnEose: true });

    activeSubscription?.stop();
    activeSubscription = sub;

    await new Promise<void>((resolve) => {
      sub.on('event', (event) => {
        const dTag = event.tags.find(t => t[0] === 'd')?.[1];
        if (!dTag || !dTag.startsWith('videos/')) return;

        const ownerPubkey = event.pubkey;
        const ownerNpub = pubkeyToNpub(ownerPubkey);
        if (!ownerNpub) return;
        const key = `${ownerNpub}/${dTag}`;

        const hashTag = event.tags.find(t => t[0] === 'hash')?.[1];
        const createdAt = event.created_at || 0;
        if (!hashTag) {
          recordDeletedVideo(ownerNpub, dTag, createdAt);
          const existing = seenVideos.get(key);
          if (existing && (existing.timestamp || 0) <= createdAt) {
            seenVideos.delete(key);
            scheduleFlush('delete');
          }
          return;
        }

        // Only public videos
        const hasEncryptedKey = event.tags.some(t => t[0] === 'encryptedKey');
        const hasSelfEncryptedKey = event.tags.some(t => t[0] === 'selfEncryptedKey');
        if (hasEncryptedKey || hasSelfEncryptedKey) return;

        const deletedAt = getDeletedVideoTimestamp(ownerNpub, dTag);
        if (deletedAt && deletedAt >= createdAt) return;
        if (deletedAt && deletedAt < createdAt) {
          clearDeletedVideo(ownerNpub, dTag);
        }

        const existing = seenVideos.get(key);
        if (existing && (existing.timestamp || 0) >= createdAt) return;

        const keyTag = event.tags.find(t => t[0] === 'key')?.[1];
        const hash = fromHex(hashTag);
        const encKey = keyTag ? fromHex(keyTag) : undefined;

        // Pre-populate tree root cache so SW can resolve thumbnails
        updateSubscriptionCache(`${ownerNpub}/${dTag}`, hash, encKey);

        seenVideos.set(key, {
          href: `#/${ownerNpub}/${encodeURIComponent(dTag)}`,
          title: dTag.slice(7),
          ownerPubkey,
          ownerNpub,
          treeName: dTag,
          timestamp: createdAt,
          rootCid: { hash, key: encKey },
        });
        scheduleFlush('event');
        eventCount += 1;
        if (eventCount <= 3) {
          log('event', { dTag, ownerNpub, createdAt });
        }
      });

      sub.on('eose', () => {
        log('eose');
        resolve();
      });
      setTimeout(resolve, 5000);
    });

    // Order with interleaving to prevent one owner from dominating the feed
    const videos = orderFeedWithInterleaving(Array.from(seenVideos.values()));

    log('complete', { count: videos.length });

    if (videos.length > 0) {
      feedStore.set(videos);
      hasInitialFetch = true;
      clearEmptyFeedRetry();
    }

    if (videos.length === 0) {
      if (connectedRelays === 0) {
        log('retry:relay:none');
        scheduleRelayRetry();
      } else {
        scheduleEmptyFeedRetry('empty-feed');
      }
    }
  } finally {
    isFetching = false;
  }
}

export function resetFeedFetchState(): void {
  activeSubscription?.stop();
  activeSubscription = null;
  clearEmptyFeedRetry();
  hasInitialFetch = false;
  isFetching = false;
}
