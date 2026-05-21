import { canUseInjectedHtreeServerUrl, resolveRuntimeHtreeBaseUrl, } from './runtime.js';
function safeDecodePathSegment(segment) {
    try {
        return decodeURIComponent(segment);
    }
    catch {
        return segment;
    }
}
function encodePath(path) {
    return path
        .split('/')
        .filter(Boolean)
        .map((segment) => encodeURIComponent(segment))
        .join('/');
}
function stripQueryAndHash(path) {
    const hashIndex = path.indexOf('#');
    const pathWithoutHash = hashIndex === -1 ? path : path.slice(0, hashIndex);
    const queryIndex = pathWithoutHash.indexOf('?');
    return queryIndex === -1 ? pathWithoutHash : pathWithoutHash.slice(0, queryIndex);
}
function normalizeRelativePath(path) {
    return stripQueryAndHash(path).replace(/^\/+/, '');
}
function resolveMutableRequestStyle(windowLike, baseUrl, explicitStyle) {
    if (explicitStyle)
        return explicitStyle;
    if (!baseUrl)
        return 'htree';
    if (canUseInjectedHtreeServerUrl(windowLike))
        return 'htree';
    return 'gateway';
}
export function parseHtreeUrl(input) {
    const trimmed = input.trim();
    if (!trimmed)
        return null;
    if (trimmed.startsWith('htree://')) {
        const withoutScheme = trimmed.slice('htree://'.length);
        const firstSlash = withoutScheme.indexOf('/');
        const head = firstSlash === -1 ? withoutScheme : withoutScheme.slice(0, firstSlash);
        const tail = firstSlash === -1 ? '' : withoutScheme.slice(firstSlash + 1);
        const rawSegments = normalizeRelativePath(tail).split('/').filter(Boolean);
        if (head.startsWith('npub1')) {
            const [rawTreeName = '', ...rawPathSegments] = rawSegments;
            const treeName = safeDecodePathSegment(rawTreeName);
            if (!treeName) {
                return null;
            }
            return {
                kind: 'mutable',
                npub: head,
                treeName,
                path: rawPathSegments.map(safeDecodePathSegment).join('/'),
            };
        }
        if (head.startsWith('nhash1')) {
            return {
                kind: 'immutable',
                nhash: head,
                path: rawSegments.map(safeDecodePathSegment).join('/'),
            };
        }
        return null;
    }
    if (trimmed.startsWith('nhash1')) {
        return {
            kind: 'immutable',
            nhash: stripQueryAndHash(trimmed),
            path: '',
        };
    }
    return null;
}
export function buildHtreeRequestPath(input, mutableStyle = 'htree') {
    if (typeof input === 'string') {
        const trimmed = input.trim();
        if (!trimmed)
            return null;
        if (trimmed.startsWith('/htree/')) {
            return trimmed;
        }
    }
    const parsed = typeof input === 'string' ? parseHtreeUrl(input) : input;
    if (!parsed)
        return typeof input === 'string' ? input.trim() : null;
    if (parsed.kind === 'mutable') {
        const encodedTreeName = encodeURIComponent(parsed.treeName);
        const encodedPath = encodePath(parsed.path);
        const prefix = mutableStyle === 'htree' ? `/htree/${parsed.npub}` : `/${parsed.npub}`;
        return `${prefix}/${encodedTreeName}${encodedPath ? `/${encodedPath}` : ''}`;
    }
    const encodedPath = encodePath(parsed.path);
    return `/htree/${parsed.nhash}${encodedPath ? `/${encodedPath}` : ''}`;
}
export function resolveHtreeRequestUrl(input, options = {}) {
    const trimmedInput = typeof input === 'string' ? input.trim() : '';
    if (trimmedInput && /^https?:\/\//i.test(trimmedInput)) {
        return trimmedInput;
    }
    const baseUrl = options.baseUrl?.trim()
        ? options.baseUrl.trim().replace(/\/$/, '')
        : resolveRuntimeHtreeBaseUrl({
            windowLike: options.windowLike,
            fallbackBaseUrl: options.fallbackBaseUrl,
        });
    const mutableStyle = resolveMutableRequestStyle(options.windowLike, baseUrl, options.mutableStyle);
    const requestPath = buildHtreeRequestPath(input, mutableStyle);
    if (!requestPath) {
        return trimmedInput;
    }
    if (!baseUrl) {
        return requestPath;
    }
    return `${baseUrl}${requestPath}`;
}
//# sourceMappingURL=htree-url.js.map