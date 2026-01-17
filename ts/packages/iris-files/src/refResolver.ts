/**
 * Ref resolver singleton for the explorer app
 *
 * Provides access to the NostrRefResolver which maps npub/treename keys
 * to merkle root hashes (refs), with subscription support for live updates.
 */
import { nip19 } from 'nostr-tools';
import { NDKEvent, type NDKFilter, type NDKSubscriptionOptions, NDKSubscriptionCacheUsage } from 'ndk';
import { createNostrRefResolver, fromHex, toHex, type RefResolver, type NostrFilter, type NostrEvent, type VisibilityCallbacks } from 'hashtree';
import { ndk, useNostrStore, encrypt, decrypt } from './nostr';
import { parseRoute } from './utils/route';

// Use window to store the resolver to ensure it's truly a singleton
// even if the module is reloaded by HMR or there are multiple bundle instances
declare global {
  interface Window {
    __hashtreeResolver?: RefResolver;
  }
}

/**
 * Get the ref resolver instance (creates it on first call)
 */
export function getRefResolver(): RefResolver {
  // Check window first to ensure true singleton
  if (typeof window !== 'undefined' && window.__hashtreeResolver) {
    return window.__hashtreeResolver;
  }
  // Visibility callbacks for NIP-44 encryption/decryption
  const visibilityCallbacks: VisibilityCallbacks = {
    // Encrypt data to a pubkey using NIP-44
    encrypt: async (data: Uint8Array, toPubkey: string): Promise<string> => {
      // Convert Uint8Array to hex string for NIP-44
      const dataHex = toHex(data);
      return encrypt(toPubkey, dataHex);
    },

    // Decrypt data from a pubkey using NIP-44
    decrypt: async (ciphertext: string, fromPubkey: string): Promise<Uint8Array | null> => {
      try {
        const decrypted = await decrypt(fromPubkey, ciphertext);
        // Convert hex string back to Uint8Array
        return fromHex(decrypted);
      } catch {
        return null;
      }
    },

    // Get link key from URL (k= parameter)
    getLinkKey: (): Uint8Array | null => {
      const route = parseRoute();
      const k = route.params.get('k');
      if (!k || k.length !== 64) return null;
      try {
        return fromHex(k);
      } catch {
        return null;
      }
    },
  };

  const resolver = createNostrRefResolver({
      subscribe: (filter: NostrFilter, onEvent: (event: NostrEvent) => void) => {
        const ndkFilter: NDKFilter = {
          kinds: filter.kinds,
          authors: filter.authors,
          '#d': filter['#d'],
          '#l': filter['#l'],
        };
        const opts: NDKSubscriptionOptions = {
          closeOnEose: false,
          // Use ONLY_RELAY to ensure we get fresh data, not stale cache
          // CACHE_FIRST can return outdated tree roots after updates
          cacheUsage: NDKSubscriptionCacheUsage.ONLY_RELAY,
        };

        const attachSub = () => {
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
          return sub;
        };

        // Subscribe immediately - worker NDK handles relay connections
        let sub = attachSub();
        let lastConnectedRelays = useNostrStore.getState().connectedRelays;
        const relayUnsub = useNostrStore.subscribe((state) => {
          if (state.connectedRelays > 0 && lastConnectedRelays === 0) {
            try {
              sub.stop();
            } catch {
              // ignore
            }
            sub = attachSub();
          }
          lastConnectedRelays = state.connectedRelays;
        });

        return () => {
          relayUnsub?.();
          sub.stop();
        };
      },
      publish: async (event) => {
        try {
          const ndkEvent = new NDKEvent(ndk);
          ndkEvent.kind = event.kind;
          ndkEvent.content = event.content;
          ndkEvent.tags = event.tags;
          // Pass through created_at if set (important for delete events to have higher timestamp)
          if (event.created_at) {
            ndkEvent.created_at = event.created_at;
          }
          await ndkEvent.publish();
          return true;
        } catch {
          return false;
        }
      },
      getPubkey: () => useNostrStore.getState().pubkey,
      nip19,
      visibility: visibilityCallbacks,
    });

  // Store on window for true singleton across HMR/bundle reloads
  if (typeof window !== 'undefined') {
    window.__hashtreeResolver = resolver;
  }

  return resolver;
}

/**
 * Build a resolver key from npub and tree name
 */
export function getResolverKey(npub: string | undefined, treeName: string | undefined): string | null {
  if (!npub || !treeName) return null;
  return `${npub}/${treeName}`;
}

/**
 * @deprecated Use resolver.publish directly instead
 */
export async function updateLocalTreeCache(
  _npub: string,
  _treeName: string,
  _hashHex: string,
  _keyHex?: string,
  _visibility: 'public' | 'link-visible' | 'private' = 'public'
): Promise<void> {
  console.warn('updateLocalTreeCache is deprecated - use resolver.publish directly');
}
