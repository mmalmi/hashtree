function getWindowLike(windowLike) {
    if (windowLike)
        return windowLike;
    if (typeof window === 'undefined')
        return null;
    return window;
}
function normalizeBaseUrl(url) {
    return (url ?? '').trim().replace(/\/$/, '');
}
function getQueryParam(name, windowLike) {
    const runtimeWindow = getWindowLike(windowLike);
    if (!runtimeWindow)
        return null;
    try {
        const value = new URLSearchParams(runtimeWindow.location?.search ?? '').get(name);
        return typeof value === 'string' ? value.trim() || null : null;
    }
    catch {
        return null;
    }
}
function getPageProtocol(windowLike) {
    const runtimeWindow = getWindowLike(windowLike);
    const protocol = runtimeWindow?.location?.protocol;
    return typeof protocol === 'string' ? protocol.toLowerCase() : null;
}
function getPageHostname(windowLike) {
    const runtimeWindow = getWindowLike(windowLike);
    const hostname = runtimeWindow?.location?.hostname;
    return typeof hostname === 'string' ? hostname.toLowerCase() : null;
}
function hasCanonicalHtreeIdentity(windowLike) {
    const runtimeWindow = getWindowLike(windowLike);
    const injectedCanonical = runtimeWindow?.__HTREE_CANONICAL_URL__;
    const canonical = typeof injectedCanonical === 'string' && injectedCanonical.trim()
        ? injectedCanonical.trim()
        : getQueryParam('htree_canonical', windowLike);
    return typeof canonical === 'string' && canonical.toLowerCase().startsWith('htree://');
}
function isLoopbackChildRuntime(windowLike) {
    if (getPageProtocol(windowLike) !== 'http:')
        return false;
    const hostname = getPageHostname(windowLike);
    if (!hostname)
        return false;
    return hostname === '127.0.0.1'
        || hostname === 'localhost'
        || hostname.endsWith('.htree.localhost');
}
function getServerProtocol(serverUrl) {
    try {
        return new URL(serverUrl).protocol.toLowerCase();
    }
    catch {
        return null;
    }
}
function isLocalHttpServerUrl(serverUrl) {
    try {
        const parsed = new URL(serverUrl);
        const hostname = parsed.hostname.toLowerCase();
        return (parsed.protocol === 'http:' || parsed.protocol === 'https:')
            && (hostname === '127.0.0.1' || hostname === 'localhost' || hostname.endsWith('.htree.localhost'));
    }
    catch {
        return false;
    }
}
function getWindowHtreeBaseUrl(windowLike) {
    const runtimeWindow = getWindowLike(windowLike);
    return normalizeBaseUrl(runtimeWindow?.htree?.htreeBaseUrl);
}
export function getInjectedHtreeServerUrl(windowLike) {
    const runtimeWindow = getWindowLike(windowLike);
    if (!runtimeWindow)
        return null;
    const override = runtimeWindow.__HTREE_SERVER_URL__;
    const fallback = getQueryParam('htree_server', runtimeWindow);
    const candidate = typeof override === 'string' && override.trim() ? override : fallback;
    const normalized = normalizeBaseUrl(candidate);
    return normalized || null;
}
export function shouldEagerLoadMediaInNativeChildRuntime(windowLike) {
    return isLoopbackChildRuntime(windowLike) && hasCanonicalHtreeIdentity(windowLike);
}
export function shouldPreferSameOriginHtreeRoutes(windowLike) {
    const serverUrl = getInjectedHtreeServerUrl(windowLike);
    if (!serverUrl)
        return false;
    const serverProtocol = getServerProtocol(serverUrl);
    if (serverProtocol !== 'http:')
        return false;
    const pageProtocol = getPageProtocol(windowLike);
    if (pageProtocol === 'https:')
        return true;
    if (pageProtocol === 'htree:') {
        const hostname = getPageHostname(windowLike);
        return hostname?.startsWith('npub1') === true || hostname === 'self';
    }
    if (hasCanonicalHtreeIdentity(windowLike) && !isLoopbackChildRuntime(windowLike))
        return true;
    return false;
}
export function canUseInjectedHtreeServerUrl(windowLike) {
    const serverUrl = getInjectedHtreeServerUrl(windowLike);
    return !!serverUrl && !shouldPreferSameOriginHtreeRoutes(windowLike);
}
export function canUseSameOriginHtreeProtocolStreaming(windowLike) {
    return getPageProtocol(windowLike) === 'htree:';
}
export function resolveRuntimeHtreeBaseUrl(options = {}) {
    const { windowLike, fallbackBaseUrl } = options;
    const injectedServerUrl = getInjectedHtreeServerUrl(windowLike);
    if (injectedServerUrl && canUseInjectedHtreeServerUrl(windowLike)) {
        return injectedServerUrl;
    }
    const windowBaseUrl = getWindowHtreeBaseUrl(windowLike);
    if (windowBaseUrl) {
        if (!isLocalHttpServerUrl(windowBaseUrl) || canUseInjectedHtreeServerUrl(windowLike)) {
            return windowBaseUrl;
        }
    }
    return normalizeBaseUrl(fallbackBaseUrl);
}
//# sourceMappingURL=runtime.js.map