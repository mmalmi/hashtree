// @ts-nocheck
/**
 * Tree Root Subscription Handler
 *
 * Worker subscribes directly to tree root events (kind 30078 with #l=hashtree).
 * Updates local cache and notifies main thread of changes.
 */
import { SimplePool } from 'nostr-tools';
import { storeTreeEventSnapshot } from '@hashtree/nostr';
import { getNdk, subscribe as ndkSubscribe, unsubscribe as ndkUnsubscribe } from './ndk';
import { getCachedRoot, getTreeRootCacheStore, setCachedRoot } from './treeRootCache';
import { nip19 } from 'nostr-tools';
import { NDKSubscriptionCacheUsage } from 'ndk';
// Active subscriptions by pubkey
const activeSubscriptions = new Map(); // pubkeyHex -> subId
const inFlightRootResolutions = new Map();
const inFlightHistoricalRootLists = new Map();
const historicalRootListCache = new Map();
const MAX_HISTORICAL_TREE_ROOT_EVENTS = 20;
const HISTORICAL_TREE_ROOT_CACHE_TTL_MS = 30_000;
const DEFAULT_TREE_ROOT_RELAYS = [
    'wss://relay.damus.io',
    'wss://relay.primal.net',
    'wss://relay.nostr.band',
    'wss://relay.snort.social',
];
// Callback to notify main thread
let notifyCallback = null;
async function storeSnapshotNhash(event) {
    const snapshotTarget = getTreeRootCacheStore();
    if (!snapshotTarget) {
        return undefined;
    }
    const snapshot = await storeTreeEventSnapshot(snapshotTarget, nip19, event);
    return snapshot?.snapshotNhash;
}
function withTimeout(promise, timeoutMs) {
    return Promise.race([
        promise,
        new Promise((resolve) => setTimeout(() => resolve(null), timeoutMs)),
    ]);
}
function getHistoryRelayUrls() {
    const urls = new Set(DEFAULT_TREE_ROOT_RELAYS);
    const ndk = getNdk();
    const connected = typeof ndk?.pool?.connectedRelays === 'function'
        ? Array.from(ndk.pool.connectedRelays()).map((relay) => relay.url)
        : [];
    for (const url of connected) {
        urls.add(url);
    }
    if (typeof ndk?.pool?.urls === 'function') {
        for (const url of ndk.pool.urls()) {
            urls.add(url);
        }
    }
    return Array.from(urls);
}
function toSignedEvent(event) {
    return {
        id: event.id,
        pubkey: event.pubkey,
        kind: event.kind,
        content: event.content,
        tags: event.tags,
        created_at: event.created_at,
        sig: event.sig,
    };
}
function uniqueEvents(events) {
    const seen = new Set();
    const result = [];
    for (const event of events) {
        const eventKey = event.id || `${event.created_at}:${event.pubkey}:${event.tags.find((tag) => tag[0] === 'd')?.[1] ?? ''}`;
        if (seen.has(eventKey))
            continue;
        seen.add(eventKey);
        result.push(event);
    }
    return result;
}
function compareReplaceableEvents(left, right) {
    const leftCreatedAt = left.created_at ?? 0;
    const rightCreatedAt = right.created_at ?? 0;
    if (leftCreatedAt !== rightCreatedAt) {
        return rightCreatedAt - leftCreatedAt;
    }
    const leftId = left.id ?? '';
    const rightId = right.id ?? '';
    if (leftId === rightId) {
        return 0;
    }
    if (!leftId)
        return 1;
    if (!rightId)
        return -1;
    return rightId.localeCompare(leftId);
}
function parseLabels(event) {
    const seen = new Set();
    const labels = [];
    for (const tag of event.tags) {
        if (tag[0] !== 'l' || !tag[1] || seen.has(tag[1]))
            continue;
        seen.add(tag[1]);
        labels.push(tag[1]);
    }
    return labels.length > 0 ? labels : undefined;
}
function parseLegacyContent(event) {
    const content = event.content?.trim();
    if (!content)
        return null;
    try {
        const parsed = JSON.parse(content);
        if (parsed && typeof parsed === 'object') {
            const payload = parsed;
            return {
                hash: typeof payload.hash === 'string' ? payload.hash : undefined,
                key: typeof payload.key === 'string' ? payload.key : undefined,
                visibility: typeof payload.visibility === 'string' ? payload.visibility : undefined,
                encryptedKey: typeof payload.encryptedKey === 'string' ? payload.encryptedKey : undefined,
                keyId: typeof payload.keyId === 'string' ? payload.keyId : undefined,
                selfEncryptedKey: typeof payload.selfEncryptedKey === 'string' ? payload.selfEncryptedKey : undefined,
                selfEncryptedLinkKey: typeof payload.selfEncryptedLinkKey === 'string' ? payload.selfEncryptedLinkKey : undefined,
            };
        }
    }
    catch {
        // Ignore JSON parse errors.
    }
    if (/^[0-9a-fA-F]{64}$/.test(content)) {
        return { hash: content };
    }
    return null;
}
export function parseTreeRootEvent(event) {
    const hashTag = event.tags.find(t => t[0] === 'hash')?.[1];
    const legacyContent = hashTag ? null : parseLegacyContent(event);
    const hash = hashTag ?? legacyContent?.hash;
    if (!hash)
        return null;
    const keyTag = event.tags.find(t => t[0] === 'key')?.[1];
    const encryptedKeyTag = event.tags.find(t => t[0] === 'encryptedKey')?.[1];
    const keyIdTag = event.tags.find(t => t[0] === 'keyId')?.[1];
    const selfEncryptedKeyTag = event.tags.find(t => t[0] === 'selfEncryptedKey')?.[1];
    const selfEncryptedLinkKeyTag = event.tags.find(t => t[0] === 'selfEncryptedLinkKey')?.[1];
    const key = keyTag ?? legacyContent?.key;
    const encryptedKey = encryptedKeyTag ?? legacyContent?.encryptedKey;
    const keyId = keyIdTag ?? legacyContent?.keyId;
    const selfEncryptedKey = selfEncryptedKeyTag ?? legacyContent?.selfEncryptedKey;
    const selfEncryptedLinkKey = selfEncryptedLinkKeyTag ?? legacyContent?.selfEncryptedLinkKey;
    let visibility;
    if (encryptedKey) {
        visibility = 'link-visible';
    }
    else if (selfEncryptedKey) {
        visibility = 'private';
    }
    else {
        visibility = legacyContent?.visibility ?? 'public';
    }
    return {
        hash,
        key,
        visibility,
        labels: parseLabels(event),
        encryptedKey,
        keyId,
        selfEncryptedKey,
        selfEncryptedLinkKey,
    };
}
async function fetchTreeRootEventsFromNdk(pubkeyHex, treeName, timeoutMs) {
    const ndk = getNdk();
    if (!ndk)
        return [];
    try {
        const events = await withTimeout(ndk.fetchEvents({
            kinds: [30078],
            authors: [pubkeyHex],
            '#d': [treeName],
            limit: MAX_HISTORICAL_TREE_ROOT_EVENTS,
        }, {
            cacheUsage: NDKSubscriptionCacheUsage.ONLY_RELAY,
        }), timeoutMs);
        if (!events)
            return [];
        return uniqueEvents(Array.from(events).map((event) => toSignedEvent(event.rawEvent?.() ?? event)))
            .sort(compareReplaceableEvents);
    }
    catch {
        return [];
    }
}
async function fetchTreeRootEventsFromRelays(pubkeyHex, treeName, timeoutMs) {
    const relayUrls = getHistoryRelayUrls();
    if (relayUrls.length === 0)
        return [];
    const pool = new SimplePool();
    try {
        const events = await withTimeout(pool.querySync(relayUrls, {
            kinds: [30078],
            authors: [pubkeyHex],
            '#d': [treeName],
            limit: MAX_HISTORICAL_TREE_ROOT_EVENTS,
        }, {
            maxWait: timeoutMs,
        }), timeoutMs + 500);
        if (!events)
            return [];
        return uniqueEvents(Array.from(events).map((event) => toSignedEvent(event)))
            .sort(compareReplaceableEvents);
    }
    catch {
        return [];
    }
    finally {
        try {
            pool.close(relayUrls);
        }
        catch { }
        try {
            pool.destroy();
        }
        catch { }
    }
}
function cidKey(cid) {
    const hash = Array.from(cid.hash, (byte) => byte.toString(16).padStart(2, '0')).join('');
    const key = cid.key
        ? Array.from(cid.key, (byte) => byte.toString(16).padStart(2, '0')).join('')
        : '';
    return `${hash}:${key}`;
}
function dedupeRoots(roots) {
    const seen = new Set();
    const result = [];
    for (const root of roots) {
        const key = cidKey(root);
        if (seen.has(key))
            continue;
        seen.add(key);
        result.push(root);
    }
    return result;
}
export async function getHistoricalTreeRoots(npub, treeName, timeoutMs = 8000) {
    const cacheKey = `${npub}/${treeName}`;
    const cached = historicalRootListCache.get(cacheKey);
    if (cached && cached.expiresAt > Date.now()) {
        return cached.roots;
    }
    if (cached) {
        historicalRootListCache.delete(cacheKey);
    }
    const inFlight = inFlightHistoricalRootLists.get(cacheKey);
    if (inFlight) {
        return await inFlight;
    }
    const lookup = (async () => {
        let pubkeyHex;
        try {
            const decoded = nip19.decode(npub);
            if (decoded.type !== 'npub')
                return [];
            pubkeyHex = decoded.data;
        }
        catch {
            return [];
        }
        const [ndkEvents, relayEvents] = await Promise.all([
            fetchTreeRootEventsFromNdk(pubkeyHex, treeName, timeoutMs),
            fetchTreeRootEventsFromRelays(pubkeyHex, treeName, timeoutMs),
        ]);
        const roots = dedupeRoots(uniqueEvents([...ndkEvents, ...relayEvents])
            .sort(compareReplaceableEvents)
            .map((event) => {
            const parsed = parseTreeRootEvent(event);
            if (!parsed)
                return null;
            try {
                return {
                    hash: hexToBytes(parsed.hash),
                    key: parsed.key ? hexToBytes(parsed.key) : undefined,
                };
            }
            catch {
                return null;
            }
        })
            .filter((root) => !!root));
        historicalRootListCache.set(cacheKey, {
            roots,
            expiresAt: Date.now() + HISTORICAL_TREE_ROOT_CACHE_TTL_MS,
        });
        return roots;
    })();
    inFlightHistoricalRootLists.set(cacheKey, lookup);
    try {
        return await lookup;
    }
    finally {
        inFlightHistoricalRootLists.delete(cacheKey);
    }
}
export async function resolveTreeRootNow(npub, treeName, timeoutMs = 8000) {
    const cached = await getCachedRoot(npub, treeName);
    if (cached) {
        return cached;
    }
    const cacheKey = `${npub}/${treeName}`;
    const inFlight = inFlightRootResolutions.get(cacheKey);
    if (inFlight) {
        return await inFlight;
    }
    const lookup = (async () => {
        let pubkeyHex;
        try {
            const decoded = nip19.decode(npub);
            if (decoded.type !== 'npub')
                return null;
            pubkeyHex = decoded.data;
        }
        catch {
            return null;
        }
        const fetched = (await fetchTreeRootEventsFromNdk(pubkeyHex, treeName, timeoutMs))[0] ?? (await fetchTreeRootEventsFromRelays(pubkeyHex, treeName, timeoutMs))[0];
        if (!fetched) {
            return null;
        }
        const parsed = parseTreeRootEvent(fetched);
        if (!parsed) {
            return null;
        }
        const hash = hexToBytes(parsed.hash);
        const key = parsed.key ? hexToBytes(parsed.key) : undefined;
        const { applied, record } = await setCachedRoot(npub, treeName, { hash, key }, parsed.visibility, {
            updatedAt: fetched.created_at,
            eventId: fetched.id,
            labels: parsed.labels,
            snapshotNhash: await storeSnapshotNhash(fetched),
            encryptedKey: parsed.encryptedKey,
            keyId: parsed.keyId,
            selfEncryptedKey: parsed.selfEncryptedKey,
            selfEncryptedLinkKey: parsed.selfEncryptedLinkKey,
        });
        historicalRootListCache.delete(cacheKey);
        if (notifyCallback && applied) {
            notifyCallback(npub, treeName, {
                hash: record.hash,
                key: record.key,
                visibility: record.visibility,
                labels: record.labels,
                updatedAt: record.updatedAt,
                snapshotNhash: record.snapshotNhash,
                encryptedKey: record.encryptedKey,
                keyId: record.keyId,
                selfEncryptedKey: record.selfEncryptedKey,
                selfEncryptedLinkKey: record.selfEncryptedLinkKey,
            });
        }
        return { hash: record.hash, key: record.key };
    })();
    inFlightRootResolutions.set(cacheKey, lookup);
    try {
        return await lookup;
    }
    finally {
        inFlightRootResolutions.delete(cacheKey);
    }
}
/**
 * Set callback to notify main thread of tree root updates
 */
export function setNotifyCallback(callback) {
    notifyCallback = callback;
}
/**
 * Subscribe to tree roots for a specific pubkey
 */
export function subscribeToTreeRoots(pubkeyHex) {
    // Already subscribed?
    if (activeSubscriptions.has(pubkeyHex)) {
        return () => unsubscribeFromTreeRoots(pubkeyHex);
    }
    const subId = `tree-${pubkeyHex.slice(0, 8)}`;
    activeSubscriptions.set(pubkeyHex, subId);
    ndkSubscribe(subId, [{
            kinds: [30078],
            authors: [pubkeyHex],
        }], {
        cacheUsage: NDKSubscriptionCacheUsage.ONLY_RELAY,
    });
    return () => unsubscribeFromTreeRoots(pubkeyHex);
}
/**
 * Unsubscribe from tree roots for a specific pubkey
 */
export function unsubscribeFromTreeRoots(pubkeyHex) {
    const subId = activeSubscriptions.get(pubkeyHex);
    if (subId) {
        ndkUnsubscribe(subId);
        activeSubscriptions.delete(pubkeyHex);
    }
}
/**
 * Handle incoming tree root event (kind 30078 with #l=hashtree)
 * Called from worker.ts event router
 */
function hasLabel(event, label) {
    return event.tags.some(tag => tag[0] === 'l' && tag[1] === label);
}
function hasAnyLabel(event) {
    return event.tags.some(tag => tag[0] === 'l');
}
export async function handleTreeRootEvent(event) {
    // Extract tree name from #d tag
    const dTag = event.tags.find(t => t[0] === 'd');
    if (!dTag || !dTag[1])
        return;
    const treeName = dTag[1];
    // Accept unlabeled legacy events, ignore other labeled apps.
    if (hasAnyLabel(event) && !hasLabel(event, 'hashtree'))
        return;
    const parsed = parseTreeRootEvent(event);
    if (!parsed)
        return;
    // Convert pubkey to npub
    const npub = nip19.npubEncode(event.pubkey);
    // Parse hash and optional key
    const hash = hexToBytes(parsed.hash);
    const key = parsed.key ? hexToBytes(parsed.key) : undefined;
    const visibility = parsed.visibility || 'public';
    // Update cache
    const { applied, record } = await setCachedRoot(npub, treeName, { hash, key }, visibility, {
        updatedAt: event.created_at,
        eventId: event.id,
        labels: parsed.labels,
        snapshotNhash: await storeSnapshotNhash(event),
        encryptedKey: parsed.encryptedKey,
        keyId: parsed.keyId,
        selfEncryptedKey: parsed.selfEncryptedKey,
        selfEncryptedLinkKey: parsed.selfEncryptedLinkKey,
    });
    if (!applied)
        return;
    historicalRootListCache.delete(`${npub}/${treeName}`);
    // Notify main thread
    if (notifyCallback) {
        notifyCallback(npub, treeName, record);
    }
}
/**
 * Check if an event is a tree root event
 */
export function isTreeRootEvent(event) {
    if (event.kind !== 30078)
        return false;
    if (hasLabel(event, 'hashtree'))
        return true;
    return !hasAnyLabel(event);
}
/**
 * Get all active subscription pubkeys
 */
export function getActiveSubscriptions() {
    return Array.from(activeSubscriptions.keys());
}
// Helper: hex string to Uint8Array
function hexToBytes(hex) {
    const bytes = new Uint8Array(hex.length / 2);
    for (let i = 0; i < bytes.length; i++) {
        bytes[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
    }
    return bytes;
}
//# sourceMappingURL=treeRootSubscription.js.map