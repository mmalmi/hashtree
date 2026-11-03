import type { CID, HashTree } from '@hashtree/core';

export const MEDIA_READ_RETRY_DELAYS_MS = [250, 1_000, 2_500, 5_000] as const;

export function isTransientMediaReadError(error: unknown): boolean {
  if (error instanceof AggregateError) {
    return error.errors.length > 0 && error.errors.every(isTransientMediaReadError);
  }
  if (!(error instanceof Error)) return false;
  return error.name === 'TimeoutError'
    || error.message.startsWith('Missing chunk:')
    || error.message === 'Blob retrieval timed out before the search completed'
    || /^(?:Blob route [^:]+: )?(?:Blossom read|P2P blob request) timed out after \d+ms$/.test(error.message);
}

export async function waitForMediaRetry(ms: number): Promise<void> {
  await new Promise<void>((resolve) => setTimeout(resolve, ms));
}

export async function* readFileStreamWithRetries(
  tree: Pick<HashTree, 'readFileStream'>,
  cid: CID,
  offset: number,
  prefetch: number,
  onRetry?: (offset: number, attempt: number, delayMs: number, error: unknown) => void,
): AsyncGenerator<Uint8Array> {
  let nextOffset = offset;
  let attempt = 0;
  while (true) {
    try {
      for await (const chunk of tree.readFileStream(cid, { offset: nextOffset, prefetch })) {
        if (!chunk.byteLength) continue;
        nextOffset += chunk.byteLength;
        attempt = 0;
        yield chunk;
      }
      return;
    } catch (error) {
      const delayMs = MEDIA_READ_RETRY_DELAYS_MS[attempt];
      if (delayMs === undefined || !isTransientMediaReadError(error)) throw error;
      attempt++;
      onRetry?.(nextOffset, attempt, delayMs, error);
      await waitForMediaRetry(delayMs);
    }
  }
}
