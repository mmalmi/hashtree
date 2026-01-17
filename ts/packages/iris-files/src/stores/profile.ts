import { writable, type Readable } from 'svelte/store';
import { nip19 } from 'nostr-tools';
import { LRUCache } from '../utils/lruCache';
import { KeyedEventEmitter } from '../utils/keyedEventEmitter';
import { ndk } from '../nostr';

export interface Profile {
  pubkey: string;
  name?: string;
  display_name?: string;
  username?: string;
  about?: string;
  picture?: string;
  nip05?: string;
  website?: string;
  banner?: string;
  lud16?: string;
  lud06?: string;
}

// In-memory profile cache
const profileCache = new LRUCache<string, Profile>(200);

// Track in-flight fetches to avoid duplicates
const pendingFetches = new Set<string>();

// Event emitter for profile updates
const profileEmitter = new KeyedEventEmitter<string, Profile>();

function fetchProfile(pubkey: string): void {
  // Skip if fetch already in progress
  if (pendingFetches.has(pubkey)) {
    return;
  }

  pendingFetches.add(pubkey);

  let bestEvent: { created_at: number; content: string; pubkey: string } | null = null;

  const sub = ndk.subscribe({ kinds: [0], authors: [pubkey], limit: 1 }, { closeOnEose: true });

  sub.on('event', (event) => {
    // Keep most recent
    if (!bestEvent || (event.created_at || 0) > bestEvent.created_at) {
      bestEvent = { created_at: event.created_at || 0, content: event.content, pubkey: event.pubkey };
    }
    // Update immediately with each event
    try {
      const profile = JSON.parse(event.content) as Profile;
      profile.pubkey = event.pubkey;
      profileCache.set(pubkey, profile);
      profileEmitter.notify(pubkey, profile);
    } catch (e) {
      console.error('[profile] JSON parse error', e);
    }
  });

  sub.on('eose', () => {
    pendingFetches.delete(pubkey);
  });
}

/**
 * Create a Svelte store for a profile
 */
export function createProfileStore(pubkey: string | undefined): Readable<Profile | undefined> {
  const pubkeyHex = pubkey
    ? pubkey.startsWith('npub1')
      ? (() => {
          try {
            const decoded = nip19.decode(pubkey);
            return decoded.data as string;
          } catch {
            return '';
          }
        })()
      : pubkey
    : '';

  const store = writable<Profile | undefined>(pubkeyHex ? profileCache.get(pubkeyHex) : undefined);

  if (pubkeyHex) {
    // Subscribe to updates
    const unsubListener = profileEmitter.subscribe(pubkeyHex, (profile) => {
      store.set(profile);
    });

    // Fetch if not cached
    if (!profileCache.get(pubkeyHex)) {
      fetchProfile(pubkeyHex);
    }

    return {
      subscribe: (run, invalidate) => {
        const unsubStore = store.subscribe(run, invalidate);
        return () => {
          unsubStore();
          unsubListener();
        };
      },
    };
  }

  return { subscribe: store.subscribe };
}

/**
 * Invalidate cached profile and refetch
 */
export function invalidateProfile(pubkey: string) {
  profileCache.delete(pubkey);
  pendingFetches.delete(pubkey);
  fetchProfile(pubkey);
}

/**
 * Get a name from profile, with fallback priority
 */
export function getProfileName(profile?: Profile, pubkey?: string): string | undefined {
  if (!profile && !pubkey) return undefined;

  if (profile) {
    return profile.display_name || profile.name || profile.username ||
           (profile.nip05 ? profile.nip05.split('@')[0] : undefined);
  }

  return undefined;
}

/**
 * Get cached profile synchronously (for non-reactive use)
 */
export function getProfileSync(pubkey: string): Profile | undefined {
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
  return profileCache.get(pubkeyHex);
}
