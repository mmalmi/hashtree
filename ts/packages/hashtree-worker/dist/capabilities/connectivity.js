const PROBE_TIMEOUT_MS = 3500;
const DUMMY_HASH = '0000000000000000000000000000000000000000000000000000000000000000';
function stripTrailingSlash(url) {
    return url.replace(/\/+$/, '');
}
async function probe(url) {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), PROBE_TIMEOUT_MS);
    try {
        const res = await fetch(url, {
            method: 'HEAD',
            // Connectivity checks should not fail just because cross-origin response
            // headers are restricted. If the fetch resolves in no-cors mode, the
            // endpoint is reachable.
            mode: 'no-cors',
            signal: controller.signal,
            cache: 'no-store',
        });
        if (res.type === 'opaque') {
            return true;
        }
        return res.status >= 100;
    }
    catch {
        return false;
    }
    finally {
        clearTimeout(timeout);
    }
}
export async function probeConnectivity(servers) {
    const readServers = servers.filter(server => server.read !== false);
    const writeServers = servers.filter(server => server.write);
    const [readResults, writeResults] = await Promise.all([
        Promise.all(readServers.map(server => probe(`${stripTrailingSlash(server.url)}/${DUMMY_HASH}.bin`))),
        Promise.all(writeServers.map(server => probe(`${stripTrailingSlash(server.url)}/upload`))),
    ]);
    const reachableReadServers = readResults.filter(Boolean).length;
    const reachableWriteServers = writeResults.filter(Boolean).length;
    return {
        online: typeof navigator === 'undefined' ? true : navigator.onLine,
        reachableReadServers,
        totalReadServers: readServers.length,
        reachableWriteServers,
        totalWriteServers: writeServers.length,
        updatedAt: Date.now(),
    };
}
//# sourceMappingURL=connectivity.js.map