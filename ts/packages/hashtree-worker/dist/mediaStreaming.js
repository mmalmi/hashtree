const DEFAULT_PREFETCH = 2;
/**
 * Stream a bounded byte range from a CID without materializing the whole range in memory.
 * Output chunks are capped at `chunkSize` bytes.
 */
export async function* streamFileRangeChunks(tree, cid, start, endInclusive, chunkSize, prefetch = DEFAULT_PREFETCH) {
    if (chunkSize <= 0) {
        throw new Error('chunkSize must be > 0');
    }
    if (endInclusive < start) {
        return;
    }
    let remaining = endInclusive - start + 1;
    let pending = new Uint8Array(chunkSize);
    let pendingLength = 0;
    for await (const sourceChunk of tree.readFileStream(cid, { offset: start, prefetch })) {
        if (remaining <= 0)
            break;
        if (sourceChunk.byteLength === 0)
            continue;
        const consumed = Math.min(sourceChunk.byteLength, remaining);
        const source = consumed === sourceChunk.byteLength ? sourceChunk : sourceChunk.subarray(0, consumed);
        remaining -= consumed;
        let cursor = 0;
        while (cursor < source.byteLength) {
            const available = source.byteLength - cursor;
            if (pendingLength === 0 && available >= chunkSize) {
                // Emit direct full-size chunks with independent buffers for transfer.
                yield source.slice(cursor, cursor + chunkSize);
                cursor += chunkSize;
                continue;
            }
            const toCopy = Math.min(chunkSize - pendingLength, available);
            pending.set(source.subarray(cursor, cursor + toCopy), pendingLength);
            pendingLength += toCopy;
            cursor += toCopy;
            if (pendingLength === chunkSize) {
                yield pending;
                pending = new Uint8Array(chunkSize);
                pendingLength = 0;
            }
        }
    }
    if (pendingLength > 0) {
        yield pending.slice(0, pendingLength);
    }
}
//# sourceMappingURL=mediaStreaming.js.map