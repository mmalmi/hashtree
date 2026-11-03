import type { CID, HashTree } from '@hashtree/core';
export declare const MEDIA_READ_RETRY_DELAYS_MS: readonly [250, 1000, 2500, 5000];
export declare function isTransientMediaReadError(error: unknown): boolean;
export declare function waitForMediaRetry(ms: number): Promise<void>;
export declare function readFileStreamWithRetries(tree: Pick<HashTree, 'readFileStream'>, cid: CID, offset: number, prefetch: number, onRetry?: (offset: number, attempt: number, delayMs: number, error: unknown) => void): AsyncGenerator<Uint8Array>;
//# sourceMappingURL=mediaReadRetry.d.ts.map