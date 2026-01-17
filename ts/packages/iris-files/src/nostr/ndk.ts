/**
 * NDK Setup for Main Thread
 *
 * Main thread has minimal NDK for signing only.
 * All relay connections and caching run in the worker.
 *
 * NIP-07 signing must happen in main thread (browser extension access).
 */
import NDK, { NDKEvent, NDKPrivateKeySigner, NDKNip07Signer, type NostrEvent } from 'ndk';

// Minimal NDK instance for signing only - no relays, no cache
export const ndk = new NDK({
  explicitRelayUrls: [], // No relay connections in main thread
});

// Expose NDK on window for debugging
if (typeof window !== 'undefined') {
  (window as Window & { __ndk?: NDK }).__ndk = ndk;
}

/**
 * Unified sign function - works with both nsec and extension login
 */
export async function signEvent(event: NostrEvent): Promise<NostrEvent> {
  if (!ndk.signer) {
    throw new Error('No signing method available');
  }
  const ndkEvent = new NDKEvent(ndk, event);
  await ndkEvent.sign();
  return ndkEvent.rawEvent() as NostrEvent;
}

// Re-export for convenience
export { NDKEvent, NDKPrivateKeySigner, NDKNip07Signer, type NostrEvent };
