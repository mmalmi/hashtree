/**
 * Block loader: waits for a block to become available before resolving.
 *
 * Tree-level read operations should use this instead of `store.get` directly.
 * `store.get` returning null means "not in this store right now" — for a P2P
 * content-addressed system that's a transient state, not a final answer. The
 * tree layer keeps the lookup open until the block arrives, so callers always
 * get a definitive result and don't need to retry on their own.
 *
 * Pass an `AbortSignal` to bound the wait.
 */

import { Store, Hash } from '../types.js';

/** How often to fall back to polling when the store has no `watch`. */
const POLL_INTERVAL_MS = 500;

/**
 * Load a block by hash, waiting until it's available.
 *
 * @param store - Backing store
 * @param hash - Block hash to load
 * @param signal - Optional AbortSignal to bound the wait
 * @returns Block data
 * @throws If aborted via `signal`
 */
export async function loadBlock(
  store: Store,
  hash: Hash,
  signal?: AbortSignal
): Promise<Uint8Array> {
  if (signal?.aborted) {
    throw signal.reason ?? new DOMException('Aborted', 'AbortError');
  }

  const initial = await store.get(hash);
  if (initial) return initial;
  if (signal?.aborted) {
    throw signal.reason ?? new DOMException('Aborted', 'AbortError');
  }

  return new Promise<Uint8Array>((resolve, reject) => {
    let settled = false;
    let unwatch: (() => void) | undefined;
    let pollTimer: ReturnType<typeof setTimeout> | undefined;

    const cleanup = () => {
      settled = true;
      if (unwatch) unwatch();
      if (pollTimer !== undefined) clearTimeout(pollTimer);
      if (signal) signal.removeEventListener('abort', onAbort);
    };

    const onAbort = () => {
      if (settled) return;
      cleanup();
      reject(signal?.reason ?? new DOMException('Aborted', 'AbortError'));
    };

    if (signal) signal.addEventListener('abort', onAbort);

    if (typeof store.watch === 'function') {
      unwatch = store.watch(hash, (data) => {
        if (settled) return;
        cleanup();
        resolve(data);
      });
      // Race: data may have been put between our initial get and the watch
      // registration. Re-check once so we don't hang on data that's already
      // local.
      void store.get(hash).then((data) => {
        if (settled || !data) return;
        cleanup();
        resolve(data);
      });
    } else {
      const poll = async () => {
        if (settled) return;
        try {
          const data = await store.get(hash);
          if (settled) return;
          if (data) {
            cleanup();
            resolve(data);
            return;
          }
        } catch {
          // Treat transient errors as "still not available" and keep polling.
        }
        if (settled) return;
        pollTimer = setTimeout(poll, POLL_INTERVAL_MS);
      };
      pollTimer = setTimeout(poll, POLL_INTERVAL_MS);
    }
  });
}
