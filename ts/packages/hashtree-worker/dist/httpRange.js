export function parseHttpByteRange(rangeHeader, totalSize) {
    if (!rangeHeader)
        return { kind: 'unsupported' };
    const bytesRange = rangeHeader.startsWith('bytes=')
        ? rangeHeader.slice('bytes='.length)
        : null;
    if (!bytesRange || bytesRange.includes(','))
        return { kind: 'unsupported' };
    if (totalSize <= 0)
        return { kind: 'unsatisfiable' };
    const parts = bytesRange.split('-', 2);
    if (parts.length !== 2)
        return { kind: 'unsupported' };
    const [startPart, endPart] = parts;
    if (!startPart) {
        const suffixLength = Number.parseInt(endPart, 10);
        if (!Number.isFinite(suffixLength) || suffixLength <= 0) {
            return { kind: 'unsatisfiable' };
        }
        const clampedSuffix = Math.min(suffixLength, totalSize);
        return {
            kind: 'range',
            range: {
                start: totalSize - clampedSuffix,
                endInclusive: totalSize - 1,
            },
        };
    }
    const start = Number.parseInt(startPart, 10);
    if (!Number.isFinite(start) || start < 0 || start >= totalSize) {
        return { kind: 'unsatisfiable' };
    }
    const endInclusive = endPart ? Number.parseInt(endPart, 10) : totalSize - 1;
    if (!Number.isFinite(endInclusive) || endInclusive < start) {
        return { kind: 'unsatisfiable' };
    }
    return {
        kind: 'range',
        range: {
            start,
            endInclusive: Math.min(endInclusive, totalSize - 1),
        },
    };
}
//# sourceMappingURL=httpRange.js.map