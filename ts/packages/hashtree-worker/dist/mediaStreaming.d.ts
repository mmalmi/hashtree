import type { CID, HashTree } from '@hashtree/core';
/**
 * Stream a bounded byte range from a CID without materializing the whole range in memory.
 * Output chunks are capped at `chunkSize` bytes.
 */
export declare function streamFileRangeChunks(tree: Pick<HashTree, 'readFileStream'>, cid: CID, start: number, endInclusive: number, chunkSize: number, prefetch?: number): AsyncGenerator<Uint8Array>;
//# sourceMappingURL=mediaStreaming.d.ts.map