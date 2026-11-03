export const MEDIA_READ_RETRY_DELAYS_MS = [250, 1_000, 2_500, 5_000];
export function isTransientMediaReadError(error) {
    if (error instanceof AggregateError) {
        return error.errors.length > 0 && error.errors.every(isTransientMediaReadError);
    }
    if (!(error instanceof Error))
        return false;
    return error.name === 'TimeoutError'
        || error.message.startsWith('Missing chunk:')
        || error.message === 'Blob retrieval timed out before the search completed'
        || /^(?:Blob route [^:]+: )?(?:Blossom read|P2P blob request) timed out after \d+ms$/.test(error.message);
}
export async function waitForMediaRetry(ms) {
    await new Promise((resolve) => setTimeout(resolve, ms));
}
export async function* readFileStreamWithRetries(tree, cid, offset, prefetch, onRetry) {
    let nextOffset = offset;
    let attempt = 0;
    while (true) {
        try {
            for await (const chunk of tree.readFileStream(cid, { offset: nextOffset, prefetch })) {
                if (!chunk.byteLength)
                    continue;
                nextOffset += chunk.byteLength;
                attempt = 0;
                yield chunk;
            }
            return;
        }
        catch (error) {
            const delayMs = MEDIA_READ_RETRY_DELAYS_MS[attempt];
            if (delayMs === undefined || !isTransientMediaReadError(error))
                throw error;
            attempt++;
            onRetry?.(nextOffset, attempt, delayMs, error);
            await waitForMediaRetry(delayMs);
        }
    }
}
//# sourceMappingURL=mediaReadRetry.js.map