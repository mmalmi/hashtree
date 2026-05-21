const DEFAULT_STORAGE_KEY = 'htree.mediaClientId';
const DEFAULT_PREFIX = 'htc';
const cachedClientIds = new Map();
function getDefaultStorage() {
    try {
        if (typeof sessionStorage !== 'undefined') {
            return sessionStorage;
        }
    }
    catch {
        return null;
    }
    try {
        if (typeof window !== 'undefined' && window.sessionStorage) {
            return window.sessionStorage;
        }
    }
    catch {
        return null;
    }
    return null;
}
function hasClientRuntime(options) {
    return typeof window !== 'undefined'
        || typeof options.storage !== 'undefined'
        || typeof options.uuidFactory === 'function';
}
function getBaseOrigin(baseOrigin) {
    if (typeof baseOrigin === 'string' && baseOrigin.trim()) {
        return baseOrigin.trim().replace(/\/+$/, '');
    }
    if (typeof window !== 'undefined' && typeof window.location?.origin === 'string' && window.location.origin) {
        return window.location.origin;
    }
    return 'https://example.invalid';
}
export function createHtreeClientId(prefix = DEFAULT_PREFIX, uuidFactory) {
    if (typeof uuidFactory === 'function') {
        return uuidFactory();
    }
    if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
        return crypto.randomUUID();
    }
    return `${prefix}_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 10)}`;
}
export function getOrCreateHtreeClientId(options = {}) {
    const storageKey = options.storageKey?.trim() || DEFAULT_STORAGE_KEY;
    const cached = cachedClientIds.get(storageKey);
    if (cached) {
        return cached;
    }
    const storage = typeof options.storage === 'undefined' ? getDefaultStorage() : options.storage;
    try {
        const existing = storage?.getItem(storageKey);
        if (existing) {
            cachedClientIds.set(storageKey, existing);
            return existing;
        }
    }
    catch {
        // Ignore storage access failures and fall back to an in-memory id.
    }
    if (!hasClientRuntime(options)) {
        return null;
    }
    const nextId = createHtreeClientId(options.prefix ?? DEFAULT_PREFIX, options.uuidFactory);
    try {
        storage?.setItem(storageKey, nextId);
    }
    catch {
        // Ignore storage write failures.
    }
    cachedClientIds.set(storageKey, nextId);
    return nextId;
}
export function appendHtreeQueryParam(url, key, value, options = {}) {
    const trimmedValue = `${value ?? ''}`.trim();
    if (!trimmedValue) {
        return url;
    }
    try {
        const baseOrigin = getBaseOrigin(options.baseOrigin);
        const parsed = new URL(url, baseOrigin);
        parsed.searchParams.set(key, trimmedValue);
        if (!/^[a-z][a-z0-9+.-]*:\/\//i.test(url) && parsed.origin === baseOrigin) {
            return `${parsed.pathname}${parsed.search}${parsed.hash}`;
        }
        return parsed.toString();
    }
    catch {
        const separator = url.includes('?') ? '&' : '?';
        return `${url}${separator}${encodeURIComponent(key)}=${encodeURIComponent(trimmedValue)}`;
    }
}
export function appendHtreeClientId(url, clientId, options = {}) {
    return appendHtreeQueryParam(url, 'htree_c', clientId, options);
}
//# sourceMappingURL=client-id.js.map