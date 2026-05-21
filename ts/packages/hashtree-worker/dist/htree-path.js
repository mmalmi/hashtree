function safeDecodePathSegment(segment) {
    try {
        return decodeURIComponent(segment);
    }
    catch {
        return segment;
    }
}
export function getRawHtreePath(url) {
    const pathMatch = url.href.match(/^[^:]+:\/\/[^/]+(.*)$/);
    return pathMatch ? (pathMatch[1]?.split('?')[0] || '') : url.pathname;
}
export function parseMutableHtreePath(rawPath) {
    const parts = rawPath.replace(/^\/+/, '').split('/');
    if (parts[0] !== 'htree' || parts.length < 3 || !parts[1]?.startsWith('npub1')) {
        return null;
    }
    const treeName = safeDecodePathSegment(parts[2] || '');
    if (!treeName) {
        return null;
    }
    return {
        npub: parts[1],
        treeName,
        filePath: parts.slice(3).map(safeDecodePathSegment).join('/'),
    };
}
export function parseImmutableHtreePath(rawPath) {
    const parts = rawPath.replace(/^\/+/, '').split('/');
    if (parts[0] !== 'htree' || parts.length < 2 || !parts[1]?.startsWith('nhash1')) {
        return null;
    }
    return {
        nhash: parts[1],
        filePath: parts.slice(2).map(safeDecodePathSegment).join('/'),
    };
}
//# sourceMappingURL=htree-path.js.map