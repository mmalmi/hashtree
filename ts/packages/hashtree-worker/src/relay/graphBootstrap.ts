import type { SocialGraph } from 'nostr-social-graph';
import { verifyMobileGraphSnapshot, type MobileGraphReceipt } from 'nostr-social-graph/mobileGraphSnapshot';
import { MOBILE_GRAPH_MAX_BYTES } from 'nostr-social-graph/hashtreeSnapshot';

/** Read only the app's packaged asset. The receipt binds its root, bytes and rows. */
export async function loadBundledGraphBootstrap(
  fallback: { url: string; receipt: MobileGraphReceipt }, signal: AbortSignal,
): Promise<SocialGraph> {
  const { receipt } = fallback;
  if (!Number.isSafeInteger(receipt.bytes) || receipt.bytes <= 0 || receipt.bytes > MOBILE_GRAPH_MAX_BYTES ||
      !Number.isSafeInteger(receipt.users) || receipt.users < 10_000 || receipt.users > 30_000) {
    throw new Error('Invalid bundled graph budget');
  }
  const active = AbortSignal.any([signal, AbortSignal.timeout(3000)]);
  const response = await fetch(fallback.url, { signal: active, credentials: 'omit' });
  if (!response.ok || !response.body) throw new Error('Bundled graph unavailable');
  const declared = response.headers.get('content-length');
  if (declared !== null && (!/^\d+$/.test(declared) || Number(declared) > receipt.bytes)) {
    await response.body.cancel();
    throw new Error('Bundled graph byte limit');
  }
  const reader = response.body.getReader(), chunks: Uint8Array[] = [];
  let length = 0;
  try {
    for (;;) {
      active.throwIfAborted();
      const { value, done } = await reader.read();
      if (done) break;
      length += value.length;
      if (length > receipt.bytes) throw new Error('Bundled graph byte limit');
      chunks.push(value);
    }
  } finally { await reader.cancel().catch(() => {}); reader.releaseLock(); }
  active.throwIfAborted();
  const bytes = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.length; }
  return verifyMobileGraphSnapshot(bytes, receipt);
}
