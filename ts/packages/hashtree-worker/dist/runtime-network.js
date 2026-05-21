import { getInjectedHtreeServerUrl } from './runtime.js';
export function normalizeRuntimeServerUrl(url) {
    return url.trim().replace(/\/+$/, '');
}
export function normalizeRuntimeRelayUrl(url) {
    return url.trim().replace(/\/+$/, '');
}
function normalizeBlossomServer(server) {
    const url = normalizeRuntimeServerUrl(server.url);
    if (!url)
        return null;
    return {
        url,
        read: server.read ?? true,
        write: server.write ?? false,
    };
}
function uniqueRelayUrls(relays) {
    const seen = new Set();
    const normalized = [];
    for (const relay of relays) {
        const url = normalizeRuntimeRelayUrl(relay);
        if (!url || seen.has(url))
            continue;
        seen.add(url);
        normalized.push(url);
    }
    return normalized;
}
export function getRuntimeHtreeServerUrl(windowLike) {
    const serverUrl = getInjectedHtreeServerUrl(windowLike);
    if (!serverUrl)
        return null;
    return normalizeRuntimeServerUrl(serverUrl);
}
export function getRuntimeNostrRelayUrl(windowLike) {
    const serverUrl = getRuntimeHtreeServerUrl(windowLike);
    if (!serverUrl)
        return null;
    try {
        const url = new URL(serverUrl);
        if (url.protocol === 'http:') {
            url.protocol = 'ws:';
        }
        else if (url.protocol === 'https:') {
            url.protocol = 'wss:';
        }
        else {
            return null;
        }
        url.pathname = '/ws';
        url.search = '';
        url.hash = '';
        return normalizeRuntimeRelayUrl(url.toString());
    }
    catch {
        return null;
    }
}
export function getRuntimeBlossomServer(windowLike) {
    const serverUrl = getRuntimeHtreeServerUrl(windowLike);
    if (!serverUrl)
        return null;
    return {
        url: serverUrl,
        read: true,
        write: true,
    };
}
export function getRuntimeNostrRelays(relays, options = {}) {
    const runtimeRelayUrl = getRuntimeNostrRelayUrl(options.windowLike);
    if (runtimeRelayUrl) {
        return [runtimeRelayUrl];
    }
    return uniqueRelayUrls(relays);
}
export function getRuntimeBlossomServers(servers, options = {}) {
    const merged = new Map();
    const runtimeServer = getRuntimeBlossomServer(options.windowLike);
    const candidates = runtimeServer ? [runtimeServer, ...servers] : [...servers];
    for (const candidate of candidates) {
        const normalized = normalizeBlossomServer(candidate);
        if (!normalized)
            continue;
        const existing = merged.get(normalized.url);
        if (existing) {
            merged.set(normalized.url, {
                url: normalized.url,
                read: existing.read || normalized.read,
                write: existing.write || normalized.write,
            });
            continue;
        }
        merged.set(normalized.url, normalized);
    }
    return Array.from(merged.values());
}
export function resolveRuntimeEndpoints(options = {}) {
    return {
        htreeServerUrl: getRuntimeHtreeServerUrl(options.windowLike),
        nostrRelays: getRuntimeNostrRelays(options.relays ?? [], options),
        blossomServers: getRuntimeBlossomServers(options.blossomServers ?? [], options),
    };
}
//# sourceMappingURL=runtime-network.js.map